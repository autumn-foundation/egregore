//! Rust Tree-sitter extraction.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{
        DeprecationMark, EdgeLabel, EntryPointKind, EntryPointMark, Graph, GraphRecord,
        LintSuppressionFacts, LintSuppressionScope, MAX_DEPRECATION_STRING_LEN, NodeKind,
        RouteAnnotation, SourceSpan, SymbolRole, stable_id,
    },
    languages::{
        common::{
            SymbolBody, add_graph_edge, collapse_whitespace, emit_reference_edges,
            next_symbol_ordinal, node_name, path_segments, reference_text, span,
        },
        cross_file::{
            BlockLocalDefinitionFact, CallKind, CallPathRoot, CallSiteFact, ConstructSiteFact,
            DefinitionFact, FileFacts, ImplTargetFact, ImplTraitRelationFact, OutOfLineModFact,
            PendingImplFact, RouteRegistrationFact, UseImportFact, crate_root_id,
        },
    },
    redaction::REDACTION_POLICY_VERSION,
};

/// Extracts Rust syntax records from one source file.
///
/// Line endings are normalized to LF at the parse boundary
/// (`extract_file_source`, issue #242), so this file-read entry point agrees
/// with the scan funnel's canonical text.
///
/// Returns the file's cross-file resolution facts (issue #152) for the
/// repo-wide `CALLS` resolution pass.
///
/// # Errors
///
/// Returns an error when the file cannot be read, the Rust grammar cannot be
/// loaded, or Tree-sitter cannot produce a syntax tree.
pub fn extract_file(
    file: &SourceFile,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<FileFacts> {
    let source =
        std::fs::read_to_string(&file.path).map_err(|source| CodegraphError::ReadFile {
            path: file.path.clone(),
            source,
        })?;
    extract_file_source(file, &source, file_id, repository_id, graph)
}

/// Extracts Rust syntax records from supplied source text.
///
/// Line endings are normalized to LF at this parse boundary (issue #242):
/// CRLF and lone CR both become LF before Tree-sitter sees the source, so
/// every caller — the scan funnel, the file-read entry point, history
/// replay, and direct API users — gets byte-stable spans, symbol text,
/// signatures, summaries, and content hashes. Idempotent: already-normalized
/// text passes through unchanged.
///
/// Returns the file's cross-file resolution facts (issue #152) for the
/// repo-wide `CALLS` resolution pass.
///
/// # Errors
///
/// Returns an error when the Rust grammar cannot be loaded, or Tree-sitter
/// cannot produce a syntax tree.
pub fn extract_file_source(
    file: &SourceFile,
    source: &str,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<FileFacts> {
    // Normalize line endings at the parse boundary (issue #242): CRLF and
    // lone CR both become LF before Tree-sitter sees the source, so byte
    // spans and content-derived fields are canonical no matter which line
    // endings the checkout used. `normalize_line_endings` is idempotent, so
    // callers that already normalized (the scan funnel, `extract_file`) pay
    // only the fast path.
    let source_lf = super::normalize_line_endings(source);
    let source = source_lf.as_str();
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| CodegraphError::ParserLanguage(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| CodegraphError::Parse {
            path: file.path.clone(),
        })?;

    let mut extractor = RustExtractor::new(
        file,
        file_id,
        repository_id,
        graph,
        source,
        file_level_cfg_gates(tree.root_node(), source),
    );
    extractor.walk(tree.root_node());
    extractor.resolve_pending_impl_edges();
    extractor.finalize_use_imports();
    extractor.emit_reference_edges();
    // File-level `#![cfg(...)]` gates (issue #190): the scan funnel stamps
    // them on the `File` node it emitted before extraction, so the file's own
    // record carries the same gate every symbol in it inherits.
    extractor.facts.file_cfg_gates = extractor.file_cfg_gates.clone();
    Ok(extractor.facts)
}

/// Tree-sitter node kinds whose text never yields reference edges (issue #134):
/// comment and literal content must not produce `CALLS`/`REFERENCES` matches.
const REFERENCE_EXCLUDED_KINDS: &[&str] = &[
    "line_comment",
    "block_comment",
    "string_literal",
    "raw_string_literal",
    "char_literal",
];

/// The closed panic-risk method-call set for issue #223: safe `.unwrap()` and
/// `.expect(..)` method calls only. Unsafe variants such as
/// `.unwrap_unchecked()` and non-panicking variants such as `.unwrap_or(..)`
/// are intentionally excluded from this slice.
const PANIC_RISK_METHODS: [&str; 2] = ["expect", "unwrap"];

/// The closed unsafe-site kind set for issue #222: `unsafe { .. }` block
/// expressions (`block`), `unsafe fn` declarations (`fn`), and `unsafe impl`
/// blocks (`impl`). `unsafe trait` declarations and `unsafe` introduced by
/// macro expansion or build scripts are intentionally outside this slice.
const UNSAFE_SITE_KINDS: [&str; 3] = ["block", "fn", "impl"];

#[derive(Debug, Clone)]
struct ImplContext {
    display: String,
    method_owner: String,
    id: String,
}

/// An impl whose trait lookup is deferred until the whole file is indexed:
/// Rust item order is insignificant, so a trait defined after its impl must
/// edge-back all the same. Captures the module scope the impl was walked in.
///
/// The IMPLEMENTS-resolution `decision` is computed at walk time directly from
/// the Tree-sitter `impl_item` node (issue #343/#344), not re-derived from the
/// display string later: the AST fields (`trait`, `type`, `type_parameters`)
/// bound the header structurally, so a return arrow in a binder bound
/// (`impl<T: Fn() -> u32> Target for Wrapper<T>`), a spaced binder, or a
/// reference/pointer blanket target can never leak across the trait/`for` split.
#[derive(Debug, Clone)]
struct PendingImplEdge {
    source_id: String,
    display: String,
    module_names: Vec<String>,
    decision: ImplTargetDecision,
}

struct RustExtractor<'graph, 'source> {
    file: &'source SourceFile,
    file_id: &'source str,
    repository_id: &'source str,
    graph: &'graph mut Graph,
    source: &'source str,
    module_names: Vec<String>,
    owner_ids: Vec<String>,
    impl_context: Option<ImplContext>,
    /// Simple name of the `trait` body currently being walked, if any (issue
    /// #390). A trait method carries no `impl_context` (trait bodies establish
    /// none), so this is what threads the enclosing trait name into a trait
    /// method's `match_segments` (`Device::read`) and flags it as a trait
    /// method. Set on entering a `trait_item`, restored on exit, mirroring how
    /// `impl_context` is managed.
    trait_context: Option<String>,
    definitions: BTreeMap<String, String>,
    /// Module-qualified names only (`m::T`; root items bare), restricted to
    /// symbols that can be IMPLEMENTS targets — never the bare-name aliases
    /// `definitions` also carries and never value-namespace items.
    /// Path-qualified impl trait lookups (`crate::T`, `self::T`, `super::T`)
    /// resolve here so a nested symbol's bare alias can never shadow a root
    /// item.
    qualified_definitions: BTreeMap<String, String>,
    /// Bare-name aliases restricted to symbols that can be IMPLEMENTS
    /// targets. The unqualified impl trait fallback (use-imported traits
    /// from another module) consults this instead of `definitions`, so a
    /// later value-namespace item (`fn T()`) can never capture an impl edge.
    type_definitions: BTreeMap<String, String>,
    /// Simple names each `use` declaration binds into scope, keyed by the
    /// module chain (`module_names`) in force where the `use` appears — the
    /// same chain an impl records — so an import in a scope `S` is visible to
    /// an impl whose chain has `S` as a prefix (the impl's own module and its
    /// descendants). Drives the AST-derived IMPORT-SHADOW VETO on both
    /// IMPLEMENTS resolution paths (issues #343/#344 round 9): a bare
    /// trait/type name shadowed by a same-final-segment `use` refers to the
    /// import, not any local same-name definition, so the impl is left
    /// unresolved (correct import-aware resolution is follow-up #393).
    imports_by_scope: BTreeMap<Vec<String>, BTreeSet<String>>,
    /// Import PATHS each module-item `use` binds into scope (issue #393):
    /// module scope -> (bound simple name -> the SET of distinct import paths as
    /// written). Populated from the SAME module-item `use` declarations as
    /// `imports_by_scope`, so the import-aware cross-file `IMPLEMENTS` resolver
    /// respects the identical Rust-visibility scoping the veto uses. Serialized
    /// into `FileFacts::use_trait_imports`; the deferred cross-file pass resolves
    /// the path so `use crate::a::T; impl T for Foo` binds `a::T`, not a root `T`.
    ///
    /// The value is a `BTreeSet` — NOT a last-wins single path — so a simple name
    /// bound to two DISTINCT paths in one scope (`#[cfg(feature = "std")] use
    /// std::fmt::Display;` alongside `#[cfg(not(feature = "std"))] use
    /// crate::local::Display;`) preserves BOTH bindings (Codex round-4 finding E,
    /// PR #399). Collapsing them to one path let the resolver mint a local edge in
    /// the configuration where the name is external; keeping both surfaces the
    /// collision as `use_trait_imports` multiplicity the resolver treats as
    /// ambiguous — no import-aware resolution, no edge (the conservative
    /// pre-#393 shadow-veto outcome).
    import_paths_by_scope: BTreeMap<Vec<String>, BTreeMap<String, BTreeSet<String>>>,
    /// The crate root this file belongs to (issue #394), stamped onto every
    /// exported trait/type and pending-impl fact so the repo-wide index can
    /// partition same-named root definitions across crate roots.
    crate_root: String,
    /// Impl trait lookups deferred to after the walk (source order).
    pending_impl_edges: Vec<PendingImplEdge>,
    symbol_bodies: Vec<SymbolBody>,
    symbol_ordinals: BTreeMap<(String, String), u64>,
    diagnostic_ordinals: BTreeMap<String, u64>,
    debt_marker_ordinals: BTreeMap<String, u64>,
    facts: FileFacts,
    panic_risk_ordinals: BTreeMap<String, u64>,
    unsafe_site_ordinals: BTreeMap<String, u64>,
    lint_suppression_ordinals: BTreeMap<String, u64>,
    /// Inline-module segments currently enclosing the walk (out-of-line
    /// `mod x;` declarations do not push here).
    inline_module_stack: Vec<String>,
    /// Count of enclosing inline modules that carry a `#[path]` attribute
    /// (which rebases everything nested in them; see issue #223 resolution
    /// gap).
    inline_path_override_depth: usize,
    /// Depth of enclosing test scopes (`#[cfg(test)]` modules and `#[test]`
    /// functions). Non-zero means panic-risk call sites classify as `test`.
    test_scope_depth: usize,
    /// Normalized `#[cfg(...)]` / `#[cfg_attr(...)]` predicates from `#![…]`
    /// inner attributes at the file top level (issue #190). They gate the
    /// whole file, so they lead every symbol's gate chain and are exported
    /// on [`FileFacts`](cross_file::FileFacts) for the `File` node.
    file_cfg_gates: Vec<String>,
    /// `true` when the whole file lives under a top-level `tests/` or
    /// `benches/` directory (Cargo's integration-test and bench roots).
    file_in_test_root: bool,
    /// Per-function receiver-type environment (issue #441): a receiver binding
    /// identifier -> its reduced nominal type path, built from typed fn params
    /// and `let x: T` ascriptions whose binding is UNSHADOWED in the function
    /// body. Set on entering a function body (before its call sites are
    /// collected) and restored on exit, so `call_site_fact` can stamp a
    /// provable `receiver_type` on `x.method()` when `x` is in this map.
    type_env: BTreeMap<String, String>,
    /// Per-function trait-dispatch environment (issue #267): a receiver binding
    /// identifier -> the trait path as written in source, built alongside
    /// [`type_env`](Self::type_env) for bindings provably trait-typed — a
    /// `&dyn Trait` ascription (params and `let` bindings), or a generic type
    /// parameter with exactly one trait bound (`fn g<T: Trait>(t: T)` / `where
    /// T: Trait`). Same shadowing veto as `type_env`: shadowed bindings are
    /// dropped. Set/restored with `type_env` so `call_site_fact` can stamp
    /// `dispatch_trait` on `x.method()` when `x` is in this map.
    dispatch_trait_env: BTreeMap<String, String>,
    /// Symbol IDs of the lexically-enclosing functions/methods/tests currently
    /// being extracted (issue #422). Pushed on entering `extract_function`,
    /// popped on exit, so a block-local `fn` can record its enclosing scope's
    /// identity for lexically-scoped call recall. Closures never push (they
    /// are not symbols), so calls inside a closure body correctly inherit the
    /// enclosing function's scope.
    enclosing_fn_ids: Vec<String>,
}

/// The callee-side components of a [`CallSiteFact`]: everything the callee
/// `function` node's kind determines, before the caller identity and span
/// are attached. Lets [`call_site_fact`](RustExtractor::call_site_fact) stay
/// small by delegating method-receiver resolution to
/// [`method_call_callee`](RustExtractor::method_call_callee).
struct CallCallee {
    display: String,
    segments: Vec<String>,
    call_kind: CallKind,
    receiver_owner: Option<String>,
    path_root: CallPathRoot,
    receiver_type: Option<String>,
    dispatch_trait: Option<String>,
}

impl<'graph, 'source> RustExtractor<'graph, 'source> {
    fn new(
        file: &'source SourceFile,
        file_id: &'source str,
        repository_id: &'source str,
        graph: &'graph mut Graph,
        source: &'source str,
        file_cfg_gates: Vec<String>,
    ) -> Self {
        Self {
            file,
            file_id,
            repository_id,
            graph,
            source,
            module_names: file_module_path(&file.repo_relative_path),
            owner_ids: vec![file_id.to_owned()],
            impl_context: None,
            trait_context: None,
            definitions: BTreeMap::new(),
            qualified_definitions: BTreeMap::new(),
            type_definitions: BTreeMap::new(),
            imports_by_scope: BTreeMap::new(),
            import_paths_by_scope: BTreeMap::new(),
            crate_root: crate_root_id(&file.repo_relative_path),
            pending_impl_edges: Vec::new(),
            symbol_bodies: Vec::new(),
            symbol_ordinals: BTreeMap::new(),
            diagnostic_ordinals: BTreeMap::new(),
            debt_marker_ordinals: BTreeMap::new(),
            facts: FileFacts::default(),
            panic_risk_ordinals: BTreeMap::new(),
            unsafe_site_ordinals: BTreeMap::new(),
            lint_suppression_ordinals: BTreeMap::new(),
            inline_module_stack: Vec::new(),
            inline_path_override_depth: 0,
            test_scope_depth: 0,
            file_cfg_gates,
            file_in_test_root: path_segments(&file.repo_relative_path)
                .first()
                .is_some_and(|segment| segment == "tests" || segment == "benches"),
            type_env: BTreeMap::new(),
            dispatch_trait_env: BTreeMap::new(),
            enclosing_fn_ids: Vec::new(),
        }
    }

    fn walk(&mut self, node: Node<'_>) {
        match node.kind() {
            "mod_item" => self.extract_module(node),
            "use_declaration" => self.extract_import(node),
            "function_item" => self.extract_function(node),
            "struct_item" => self.extract_named_symbol(node, "struct"),
            "enum_item" => self.extract_named_symbol(node, "enum"),
            "trait_item" => self.extract_named_symbol(node, "trait"),
            "impl_item" => self.extract_impl(node),
            "const_item" => self.extract_named_symbol(node, "const"),
            "static_item" => self.extract_named_symbol(node, "static"),
            "type_item" => self.extract_named_symbol(node, "type_alias"),
            "macro_invocation" => self.extract_macro_diagnostic(node),
            "call_expression" => self.extract_call_expression(node),
            "line_comment" | "block_comment" => self.extract_comment_markers(node),
            "attribute_item" | "inner_attribute_item" => self.extract_lint_suppression(node),
            "function_signature_item" => self.extract_function_signature(node),
            "unsafe_block" => self.extract_unsafe_block(node),
            _ => self.walk_children(node),
        }
    }

    fn walk_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child);
        }
    }

    fn extract_module(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = stable_id(&[
            "node",
            "module",
            self.repository_id,
            &self.file.repo_relative_path,
            &qualified_name,
        ]);
        // Module records carry the declaration visibility class so
        // reachability queries (issue #213) can resolve the module chain
        // without re-parsing source. Additive per
        // `docs/schema/schema-versioning.md §2`; never an identity input.
        let mut module_record = GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Module,
            self.file.repo_relative_path.clone(),
            span(node),
            qualified_name.clone(),
            "rust",
            format!("Rust module {qualified_name}"),
        )
        .with_declaration_surface(Some(self.symbol_visibility(node).to_owned()), None, None)
        // Test-vs-production role (issue #238): a `#[cfg(test)] mod`
        // declaration is itself the lexical gate, so the Module record
        // carries the same role its member symbols get. Stamped before
        // the scope counter is entered for the module's children.
        .with_role(self.symbol_role(node));
        // Conditional-compilation gates (issue #190): the module's own
        // `#[cfg(...)]` / `#[cfg_attr(...)]` predicates plus every enclosing
        // gated item/module, outermost first. Member symbols inherit this
        // chain through the parent-chain walk. Stamped only when non-empty —
        // ungated modules carry no `cfg` field. Additive, never an identity
        // input.
        let module_cfg_gates = self.cfg_gate_chain(node);
        if !module_cfg_gates.is_empty() {
            module_record = module_record.with_cfg(module_cfg_gates);
        }
        self.graph.push(
            module_record
                // The module summary is name-only, so a body change with an
                // unchanged name would hash identically. Stamp a compact BLAKE3
                // handle over the normalized body so evidence-freshness drift stays
                // content-detectable (issue #206). Inline `mod foo { .. }` covers
                // the whole body; out-of-line `mod foo;` covers just the
                // declaration (the target file's own records carry its body).
                .with_content_signature(content_signature(self.node_text(node))),
        );
        self.add_edge(
            EdgeLabel::Contains,
            self.owner_id(),
            id.clone(),
            format!("{} contains module {qualified_name}", self.owner_name()),
        );

        let is_test_module = self.has_cfg_test_attribute(node);
        let is_inline = node.child_by_field_name("body").is_some();
        if !is_inline {
            // Out-of-line declaration (`mod name;`): the module body lives in
            // its own file, extracted with no view of this gating attribute.
            // Export the declaration so the repo-wide pass (issue #223) can
            // mark the module file's panic-risk sites as test context.
            self.facts.out_of_line_mods.push(OutOfLineModFact {
                name: local_name.clone(),
                inline_module_path: self.inline_module_stack.clone(),
                test_gated: is_test_module || self.in_test_context(),
                path_override: self.mod_path_override(node),
                under_inline_path_override: self.inline_path_override_depth > 0,
                // The declaration's full gate chain (issue #190): the target
                // file's records inherit this via the repo-wide
                // `apply_out_of_line_cfg_gates` pass. Computed before the
                // module-name stack is pushed — the chain is a pure function
                // of the AST parent chain plus the file's inner attributes,
                // so stack state is irrelevant.
                cfg_gates: self.cfg_gate_chain(node),
            });
        }

        let inline_has_path_override = is_inline && self.has_path_attribute(node);
        self.module_names.push(local_name.clone());
        self.owner_ids.push(id);
        if is_inline {
            self.inline_module_stack.push(local_name);
        }
        if inline_has_path_override {
            self.inline_path_override_depth += 1;
        }
        if is_test_module {
            self.test_scope_depth += 1;
        }
        self.walk_children(node);
        if is_test_module {
            self.test_scope_depth -= 1;
        }
        if inline_has_path_override {
            self.inline_path_override_depth -= 1;
        }
        if is_inline {
            self.inline_module_stack.pop();
        }
        self.owner_ids.pop();
        self.module_names.pop();
    }

    /// `true` when the item carries any `#[path ...]` attribute in the
    /// attribute items immediately preceding it (comments are skipped).
    fn has_path_attribute(&self, node: Node<'_>) -> bool {
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    let text: String = self
                        .node_text(sibling)
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if text.starts_with("#[path=") || text.starts_with("#[path]") {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        false
    }

    /// Extracts a trivial `#[path = "literal"]` override from the attribute
    /// items immediately preceding an out-of-line module declaration.
    /// Non-literal path attributes yield `None` (documented resolution gap).
    fn mod_path_override(&self, node: Node<'_>) -> Option<String> {
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    let text: String = self
                        .node_text(sibling)
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if let Some(literal) = text
                        .strip_prefix("#[path=\"")
                        .and_then(|rest| rest.strip_suffix("\"]"))
                        && !literal.is_empty()
                        && !literal.contains('"')
                    {
                        return Some(literal.to_owned());
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        None
    }

    fn extract_import(&mut self, node: Node<'_>) {
        // Record the simple names this `use` binds into the current module
        // scope for the import-shadow veto (issues #343/#344 round 9). Keyed by
        // the live module chain so visibility follows module nesting.
        //
        // Only a MODULE-ITEM `use` feeds the veto: a `use` inside a function
        // body / block / expression is invisible at module level, so it must
        // never shadow a module-level impl's bare trait name (round-10 Codex
        // finding). A module item's `use_declaration` sits directly under the
        // `source_file` root or a `mod_item`'s `declaration_list` body; a
        // block-local `use` sits under a `block`, so its parent kind reveals
        // the difference. Non-module-item imports still emit their Import node
        // and IMPORTS edge below — only the veto index skips them.
        if is_module_item_use(node) {
            let bound = self.collect_use_bound_names(node);
            if !bound.is_empty() {
                self.imports_by_scope
                    .entry(self.module_names.clone())
                    .or_default()
                    .extend(bound);
            }
            // Capture the resolved import PATH for each bound simple name
            // (issue #393), keyed by the same module scope, so the cross-file
            // resolver can bind a bare imported trait/type name to its true
            // aliased target instead of vetoing it. Glob imports bind no simple
            // name and contribute nothing here (they stay bounded out).
            for (simple, path) in use_bound_import_paths(node, self.source) {
                // Insert into the per-name SET (not last-wins) so a cfg-gated
                // same-name collision keeps every distinct binding (finding E).
                self.import_paths_by_scope
                    .entry(self.module_names.clone())
                    .or_default()
                    .entry(simple)
                    .or_default()
                    .insert(path);
            }
        }
        let name = import_name(self.node_text(node));
        let id = stable_id(&[
            "node",
            "import",
            self.repository_id,
            &self.file.repo_relative_path,
            &name,
        ]);
        // Imports carry NO `content_signature`: the import stable ID already
        // encodes the full `use ...;` declaration (via `import_name`, the whole
        // trimmed path — not the bound leaf), so any body change (glob
        // expansion, alias, added path segment) mints a DIFFERENT record ID. A
        // content signature could therefore never be the drift trigger for an
        // import — two versions with differing bodies never share an ID for
        // evidence-freshness to compare within. Such a change surfaces as a
        // handle-identity change (`unresolved`/removed), not `drifted` (#206).
        let mut record = GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Import,
            self.file.repo_relative_path.clone(),
            span(node),
            name.clone(),
            "rust",
            format!("Rust import {name}"),
        );
        // Doc comments above a `use` declaration attach to the item rustdoc
        // exposes at the re-export site (issue #257); capture them as the
        // import's doc fact. Additive, never an identity input.
        if let Some(doc) = self.symbol_doc(node) {
            record = record
                .with_declaration_surface(None, None, Some(doc))
                .with_redaction_policy_version(REDACTION_POLICY_VERSION);
        }
        self.graph.push(record);
        self.add_edge(
            EdgeLabel::Imports,
            self.owner_id(),
            id,
            format!("{} imports {name}", self.owner_name()),
        );
    }

    /// Collects the simple names a `use` declaration binds into its enclosing
    /// scope. Thin `&self` wrapper over the free [`use_bound_names`] worker so
    /// the same AST walk is unit-testable in isolation.
    fn collect_use_bound_names(&self, node: Node<'_>) -> Vec<String> {
        use_bound_names(node, self.source)
    }

    /// Reports whether a `use` import visible in the impl's module scope binds
    /// the bare simple name `bare`. Thin `&self` wrapper over the free
    /// [`scope_imports_bare_name`] predicate — the SINGLE shared answer behind
    /// the import-shadow veto on BOTH IMPLEMENTS resolution paths (the local
    /// per-file resolver consults it directly, the deferred cross-file pass
    /// reads it off `PendingImplFact::shadowed_by_use`), so the two paths can
    /// never diverge (issues #343/#344 round 9).
    fn scope_imports_bare_name(&self, module_names: &[String], bare: &str) -> bool {
        scope_imports_bare_name(&self.imports_by_scope, module_names, bare)
    }

    fn extract_named_symbol(&mut self, node: Node<'_>, symbol_kind: &str) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());
        // A trait body's simple name threads into its methods' match segments
        // and trait-method marker (issue #390). Capture it before `local_name`
        // is potentially moved into the impl-target key spaces below.
        let trait_name = (symbol_kind == "trait").then(|| local_name.clone());
        // Only symbols that can be IMPLEMENTS targets enter the impl-lookup
        // key spaces: value-namespace items (`const`, `static`, functions)
        // must never shadow a trait or type in impl trait resolution.
        if is_impl_target_kind(symbol_kind) {
            // Export this trait/type for the repo-wide cross-file IMPLEMENTS
            // pass (issue #344), keyed on the crate-root-relative qualified
            // name so an out-of-line impl in another file can resolve to it.
            self.facts.impl_targets.push(ImplTargetFact {
                id: id.clone(),
                qualified_name: qualified_name.clone(),
                crate_root: self.crate_root.clone(),
                module_path: self.module_names.clone(),
                symbol_kind: symbol_kind.to_owned(),
            });
            self.qualified_definitions
                .insert(qualified_name, id.clone());
            self.type_definitions.insert(local_name, id);
        }
        // Establish the trait body scope while walking its methods, restoring
        // the previous context on exit (mirrors `impl_context` management).
        if let Some(trait_name) = trait_name {
            let previous = self.trait_context.replace(trait_name);
            self.walk_children(node);
            self.trait_context = previous;
        } else {
            self.walk_children(node);
        }
    }

    /// True when `node` (a `function_item` or `function_signature_item`) is a
    /// DIRECT structural member of the enclosing `trait` body — its immediate
    /// declaration list belongs to the `trait_item` — and NOT a function
    /// nested deeper inside a trait method's body (issue #390).
    ///
    /// Trait-method attribution (the `is_trait_method` marker and the
    /// enclosing-trait owner segment in `match_segments`) rides on this exact
    /// structural position — a true peer of an impl method being the direct
    /// member of its `impl_item` — rather than the broad persistent
    /// `trait_context` flag. The flag stays `Some` while descending into a
    /// trait method's body, so a block-local `fn helper` inside a default
    /// method would otherwise be mis-marked a trait method and drop its bare
    /// call site (prefer a MISSING edge over a WRONG one, so also prefer no
    /// wrong exclusion). An impl always sets `impl_context`, so an impl method
    /// (even nested in a trait default body) never satisfies this.
    fn is_direct_trait_method(&self, node: Node<'_>) -> bool {
        self.impl_context.is_none()
            && self.trait_context.is_some()
            && node
                .parent()
                .filter(|parent| parent.kind() == "declaration_list")
                .and_then(|parent| parent.parent())
                .is_some_and(|grandparent| grandparent.kind() == "trait_item")
    }

    /// True when `node` is a DIRECT member of an `impl` block's declaration
    /// list — its immediate parent is the impl's `declaration_list` and the
    /// grandparent is the `impl_item`. A block-local `fn` nested inside an impl
    /// method body fails this check, so it is attributed as a free function,
    /// not `Owner::fn` (issue #413 — the impl-side mirror of
    /// `is_direct_trait_method`). The persistent `impl_context` flag stays
    /// `Some` while descending into an impl method's body, so without this
    /// structural gate a block-local `fn helper` would be mis-recorded as
    /// `method` `Owner::helper` and drop its bare `helper()` call (prefer a
    /// MISSING edge over a WRONG one).
    fn is_direct_impl_method(&self, node: Node<'_>) -> bool {
        self.impl_context.is_some()
            && node
                .parent()
                .filter(|parent| parent.kind() == "declaration_list")
                .and_then(|parent| parent.parent())
                .is_some_and(|grandparent| grandparent.kind() == "impl_item")
    }

    /// True when `node` (a `function_item`) is declared inside a function or
    /// closure BODY rather than as a direct item of a module, `impl`, or `trait`
    /// (issue #413 round 3, Codex finding A). Walks up the enclosing scopes: the
    /// nearest scope-defining ancestor being a `block` (a fn/closure body, an
    /// `if`/`match`/loop arm, or a bare block) means the `fn` is a block-local
    /// statement, lexically unreachable from other scopes; a `declaration_list`
    /// (a `mod`/`impl`/`trait` item list) or the `source_file` crate root means a
    /// directly, cross-scope-reachable item. A module-level or impl/trait-method
    /// `function_item` never sits under a `block`, so it is never block-local.
    fn is_block_local_fn(node: Node<'_>) -> bool {
        let mut current = node.parent();
        while let Some(parent) = current {
            match parent.kind() {
                "block" => return true,
                "declaration_list" | "source_file" => return false,
                _ => current = parent.parent(),
            }
        }
        false
    }

    fn extract_function(&mut self, node: Node<'_>) {
        if has_unsafe_modifier(node) {
            self.emit_unsafe_site(node, "fn");
        }
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        // Method attribution rides DIRECT structural impl membership (issue
        // #413): a block-local `fn` nested inside an impl method body keeps
        // `impl_context` `Some` but is NOT a direct impl member, so it falls
        // through to the free-function/test branches rather than being
        // mis-recorded as `Owner::fn`.
        let is_direct_impl_method = self.is_direct_impl_method(node);
        let (symbol_kind, qualified_name) = match self.impl_context.as_ref() {
            Some(impl_context) if is_direct_impl_method => (
                "method",
                self.qualify(&format!("{}::{local_name}", impl_context.method_owner)),
            ),
            _ if self.is_test_function(node) => ("test", self.qualify(&local_name)),
            _ => ("function", self.qualify(&local_name)),
        };

        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        let is_trait_method = self.is_direct_trait_method(node);
        // The lexically-enclosing function/method/test symbol, if any — the
        // scope identity a block-local `fn` declared here records for
        // lexically-scoped call recall (issue #422). Captured BEFORE this
        // function's own id is pushed onto `enclosing_fn_ids` below.
        let enclosing_scope_id = self.enclosing_fn_ids.last().cloned();
        let is_block_local = Self::is_block_local_fn(node);
        // A block-local `fn` (declared inside a function/closure BODY) is
        // lexically unreachable from other scopes (issue #413 round 3, Codex
        // finding A). Its Symbol node keeps the corrected free-function identity
        // (kind `function`, module-qualified name, no false `Owner::fn` method,
        // no owner DEFINES — `add_symbol` above is intentionally NOT gated, so
        // the node + its DEFINES edge and referential closure are preserved).
        // But it MUST NOT enter ANY flat call-candidate set, or a call from
        // another scope could bind the buried item — a wrong or ambiguous edge
        // (and a confident WRONG edge when the real target is external, leaving
        // the block-local the sole in-repo candidate). Two candidate sets feed
        // call resolution, so both stay gated: the per-file name→id map
        // (`self.definitions`, read by the issue #134 `emit_reference_edges`
        // same-file pass) AND the repo-wide `FileFacts::definitions`
        // (cross-file + same-file-labeling). Lexically-scoped recall of
        // intra-block calls rides a THIRD, scope-gated set instead
        // (`FileFacts::block_local_definitions`, issue #422): the def records
        // its enclosing scope's identity, and the resolver admits it as a
        // candidate ONLY for calls from that scope.
        if !is_block_local {
            self.definitions.insert(local_name.clone(), id.clone());
            self.definitions.insert(qualified_name.clone(), id.clone());
            self.facts.definitions.push(DefinitionFact {
                id: id.clone(),
                qualified_name: qualified_name.clone(),
                simple_name: local_name.clone(),
                match_segments: self.definition_match_segments(
                    &local_name,
                    is_trait_method,
                    is_direct_impl_method,
                ),
                symbol_kind: symbol_kind.to_owned(),
                is_trait_method,
                repo_relative_path: self.file.repo_relative_path.clone(),
            });
        } else if let Some(enclosing_scope_id) = enclosing_scope_id {
            // Issue #422: a block-local `fn` with an enclosing function symbol
            // joins the scope-gated candidate set (never the flat index).
            // `local_name` is moved (not cloned): nothing below uses it.
            self.facts
                .block_local_definitions
                .push(BlockLocalDefinitionFact {
                    id: id.clone(),
                    qualified_name: qualified_name.clone(),
                    simple_name: local_name,
                    symbol_kind: symbol_kind.to_owned(),
                    enclosing_scope_id,
                    repo_relative_path: self.file.repo_relative_path.clone(),
                });
        }
        // A block-local `fn` with NO enclosing function symbol (e.g. inside a
        // `const` initializer at module level) records no scoped fact either:
        // no call site can carry a matching caller id, so its calls stay
        // unresolved — a MISS, never a WRONG edge.
        self.enclosing_fn_ids.push(id.clone());
        // Build this function's provable receiver-type environment (issue #441)
        // and trait-dispatch environment (issue #267) and install them for the
        // duration of the call-site collection, then restore the caller's
        // environments. Nested functions collect their own call sites through
        // their own `extract_function` and build their own environments, so
        // scopes never bleed.
        let (type_env, dispatch_trait_env) = self.build_type_env(node);
        let outer_type_env = std::mem::replace(&mut self.type_env, type_env);
        let outer_dispatch_env =
            std::mem::replace(&mut self.dispatch_trait_env, dispatch_trait_env);
        self.collect_call_sites(node, &id, &qualified_name);
        self.type_env = outer_type_env;
        self.dispatch_trait_env = outer_dispatch_env;
        self.symbol_bodies.push(SymbolBody {
            id,
            name: qualified_name,
            text: reference_text(node, self.source, REFERENCE_EXCLUDED_KINDS),
        });
        let is_test_fn = self.has_test_attribute(node);
        if is_test_fn {
            self.test_scope_depth += 1;
        }
        self.walk_children(node);
        if is_test_fn {
            self.test_scope_depth -= 1;
        }
        self.enclosing_fn_ids.pop();
    }

    /// Match segments for a callable definition: the module path, plus the
    /// normalized impl owner for methods (or the enclosing trait name for a
    /// DIRECT trait method — issue #390), plus the simple name.
    ///
    /// `is_trait_method` is the caller's structural verdict from
    /// [`is_direct_trait_method`](Self::is_direct_trait_method): the enclosing
    /// trait name is pushed ONLY for a function that is a direct member of the
    /// trait body, never a block-local `fn` nested inside a trait method (which
    /// is a free function whose owner is its module).
    fn definition_match_segments(
        &self,
        local_name: &str,
        is_trait_method: bool,
        is_direct_impl_method: bool,
    ) -> Vec<String> {
        let mut segments = self.module_names.clone();
        if let Some(impl_context) = self.impl_context.as_ref().filter(|_| is_direct_impl_method) {
            // The impl-owner segment is pushed ONLY for a DIRECT impl member
            // (issue #413), never a block-local `fn` nested inside an impl
            // method body (which is a free function whose owner is its module).
            if let Some(owner) = normalize_impl_owner(&impl_context.method_owner) {
                segments.push(owner);
            }
        } else if let Some(trait_name) = self.trait_context.as_ref().filter(|_| is_trait_method) {
            // A trait method's owner segment is the enclosing trait, so a
            // `Device::read` path call name-resolves to it (issue #390). The
            // last segment stays the bare method name for the `Method` pool.
            // Gated on DIRECT trait membership (`is_trait_method`), never the
            // broad persistent `trait_context`, so a block-local `fn` nested
            // in a trait method stays a free function.
            segments.push(trait_name.clone());
        }
        segments.push(local_name.to_owned());
        segments
    }

    /// Collects syntactic call sites inside a recorded symbol body (issue #152).
    ///
    /// Recursion stops at nested definition scopes (`fn`, `impl`, `trait`,
    /// `mod`) — those register their own symbols and collect their own calls —
    /// and never enters macro token trees, comments, or string literals
    /// (Tree-sitter parses none of them as `call_expression`).
    fn collect_call_sites(&mut self, node: Node<'_>, caller_id: &str, caller_name: &str) {
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            match child.kind() {
                "function_item" | "impl_item" | "trait_item" | "mod_item" | "macro_definition" => {}
                "macro_invocation" => {
                    // Macro token trees are otherwise never walked (Tree-sitter
                    // parses none of their contents as `call_expression`). A
                    // NARROW exception (issue #445): a route-registration macro
                    // in the closed set (`routes![…]`) contributes one
                    // route-registration reference per bare handler identifier.
                    // Every other macro is still skipped — no recursion into the
                    // token tree, no arbitrary-macro parsing.
                    self.collect_route_registrations(child, caller_id, caller_name);
                }
                "call_expression" => {
                    if let Some(fact) = self.call_site_fact(child, caller_id, caller_name) {
                        self.facts.call_sites.push(fact);
                    }
                    self.collect_call_sites(child, caller_id, caller_name);
                }
                "struct_expression" => {
                    // A struct-literal construction `Type { … }` (issue #443).
                    // Record it, then keep walking children so a nested literal
                    // in a field value (`Outer { inner: Inner { … } }`) is also
                    // collected.
                    if let Some(fact) = self.construct_site_fact(child, caller_id, caller_name) {
                        self.facts.construct_sites.push(fact);
                    }
                    self.collect_call_sites(child, caller_id, caller_name);
                }
                _ => self.collect_call_sites(child, caller_id, caller_name),
            }
        }
    }

    /// Classifies one `call_expression` into a [`CallSiteFact`], or `None`
    /// when the callee is not a resolvable name form (closure calls, chained
    /// call results, qualified `<T as Trait>::` paths, tuple-index fields).
    fn call_site_fact(
        &self,
        node: Node<'_>,
        caller_id: &str,
        caller_name: &str,
    ) -> Option<CallSiteFact> {
        let mut function = node.child_by_field_name("function")?;
        if function.kind() == "generic_function" {
            function = function.child_by_field_name("function")?;
        }
        let callee = match function.kind() {
            "identifier" => {
                let name = self.node_text(function).trim().to_owned();
                CallCallee {
                    display: name.clone(),
                    segments: vec![name],
                    call_kind: CallKind::Direct,
                    receiver_owner: None,
                    path_root: CallPathRoot::Unqualified,
                    receiver_type: None,
                    dispatch_trait: None,
                }
            }
            "scoped_identifier" => {
                let display = self.node_text(function).trim().to_owned();
                let segments = self.normalize_call_path(&display)?;
                // Classify the leading crate scope BEFORE it is lost to
                // normalization (issue #440): a `crate`/`self`/`super` head names
                // the caller's own crate; any other head may name a workspace
                // crate, so retain the raw first segment for the registry lookup.
                let path_root = match display.split("::").next().map(str::trim) {
                    Some("crate" | "self" | "super") => CallPathRoot::CurrentCrate,
                    Some(first) if !first.is_empty() => CallPathRoot::Leading(first.to_owned()),
                    _ => CallPathRoot::Unqualified,
                };
                CallCallee {
                    display,
                    segments,
                    call_kind: CallKind::Path,
                    receiver_owner: None,
                    path_root,
                    receiver_type: None,
                    dispatch_trait: None,
                }
            }
            "field_expression" => self.method_call_callee(function)?,
            _ => return None,
        };
        if callee.segments.is_empty()
            || callee
                .segments
                .iter()
                .any(|segment| !is_simple_ident(segment))
        {
            return None;
        }
        Some(CallSiteFact {
            caller_id: caller_id.to_owned(),
            caller_name: caller_name.to_owned(),
            callee_display: callee.display,
            callee_segments: callee.segments,
            call_kind: callee.call_kind,
            path_root: callee.path_root,
            receiver_owner: callee.receiver_owner,
            receiver_type: callee.receiver_type,
            dispatch_trait: callee.dispatch_trait,
            span: span(node),
        })
    }

    /// Resolves the callee components of a `field_expression` call
    /// (`receiver.method()`): the method name plus the receiver-derived
    /// narrowing facts — the `self`-receiver owner (issue #390), the provable
    /// receiver type (issue #441), and the provable trait-dispatch binding
    /// (issue #267). Returns `None` when the field is not a plain method
    /// identifier.
    fn method_call_callee(&self, function: Node<'_>) -> Option<CallCallee> {
        let field = function.child_by_field_name("field")?;
        if field.kind() != "field_identifier" {
            return None;
        }
        let name = self.node_text(field).trim().to_owned();
        let receiver_value = function.child_by_field_name("value");
        let receiver_is_self = receiver_value.is_some_and(|value| value.kind() == "self");
        let owner = receiver_is_self
            .then(|| {
                // A `self.method()` receiver call carries the owner of
                // the enclosing `Self` so the SelfMethod branch can
                // narrow to it: the impl owner inside an impl block
                // (unchanged), else the enclosing trait name inside a
                // trait body (issue #390). Inside a trait there is no
                // `impl_context`, so before this the owner was None and
                // the call collapsed to a plain `Method` that fanned out
                // to every same-named trait method. `impl_context` takes
                // precedence when both are set (a nested impl inside a
                // trait default body). The trait name is the raw
                // `trait_context` string, matching the trait-method
                // owner segment in `definition_match_segments`.
                self.impl_context
                    .as_ref()
                    .and_then(|impl_context| normalize_impl_owner(&impl_context.method_owner))
                    .or_else(|| self.trait_context.clone())
            })
            .flatten();
        let call_kind = if owner.is_some() {
            CallKind::SelfMethod
        } else {
            CallKind::Method
        };
        // Provable receiver type (issue #441): for a non-`self` receiver
        // that is a simple `identifier` binding whose type is in this
        // function's unshadowed type environment, stamp the reduced
        // nominal type so the resolver can narrow `x.method()` to that
        // type's own method. A `self` receiver keeps `receiver_owner`
        // only; a receiver that is not a bare identifier, or whose name
        // is not a provable binding, gets `None` (today's fan-out).
        let receiver_type = (!receiver_is_self)
            .then(|| receiver_value.filter(|value| value.kind() == "identifier"))
            .flatten()
            .and_then(|value| self.type_env.get(self.node_text(value).trim()).cloned());
        // Provable trait-dispatch binding (issue #267): for a
        // non-`self` receiver that is a simple `identifier` binding
        // whose ascribed type is provably trait-typed (`&dyn
        // Trait`, `Box<dyn Trait>`, or a single-bound type
        // parameter), stamp the trait path as written so the
        // resolver can expand the call to every known in-crate
        // implementor method.
        let dispatch_trait = (!receiver_is_self)
            .then(|| receiver_value.filter(|value| value.kind() == "identifier"))
            .flatten()
            .and_then(|value| {
                self.dispatch_trait_env
                    .get(self.node_text(value).trim())
                    .cloned()
            });
        Some(CallCallee {
            display: name.clone(),
            segments: vec![name],
            call_kind,
            receiver_owner: owner,
            path_root: CallPathRoot::Unqualified,
            receiver_type,
            dispatch_trait,
        })
    }

    /// Classifies one `struct_expression` (`Type { … }`) into a
    /// [`ConstructSiteFact`] (issue #443), or `None` when the constructed type is
    /// not a resolvable simple nominal path.
    ///
    /// The `name` field is reduced to its nominal type path (generic arguments
    /// dropped) and normalized with the same `crate`/`self`/`super`-stripping and
    /// leading-`Self`→impl-owner rewrite calls use. Exhaustiveness is the
    /// absence of a `..base` (`base_field_initializer`) in the literal body.
    fn construct_site_fact(
        &self,
        node: Node<'_>,
        constructor_id: &str,
        constructor_name: &str,
    ) -> Option<ConstructSiteFact> {
        let name_node = node.child_by_field_name("name")?;
        // Reuse the receiver-type reducer: it yields the nominal path text for a
        // `type_identifier`/`scoped_type_identifier` and peels a single-level
        // generic (`Wrapper<T>` -> `Wrapper`).
        let display = reduce_receiver_type(name_node, self.source)?;
        let segments = self.normalize_call_path(&display)?;
        if segments.is_empty() || segments.iter().any(|segment| !is_simple_ident(segment)) {
            return None;
        }
        // Classify the leading crate scope BEFORE normalization erased it,
        // mirroring the path-qualified call classifier (issue #440). A leading
        // `Self` names the impl owner's own (current) crate, so it is
        // `CurrentCrate`, not a `Leading` type segment.
        let path_root = match display.split("::").next().map(str::trim) {
            Some("crate" | "self" | "super" | "Self") => CallPathRoot::CurrentCrate,
            Some(first) if display.contains("::") && !first.is_empty() => {
                CallPathRoot::Leading(first.to_owned())
            }
            _ => CallPathRoot::Unqualified,
        };
        let is_exhaustive = !struct_literal_has_base(node);
        Some(ConstructSiteFact {
            constructor_id: constructor_id.to_owned(),
            constructor_name: constructor_name.to_owned(),
            type_display: display,
            type_segments: segments,
            path_root,
            is_exhaustive,
            span: span(node),
        })
    }

    /// Builds the per-function receiver-type environment (issue #441) AND the
    /// trait-dispatch environment (issue #267).
    ///
    /// Entries come from (a) fn params with a simple-identifier pattern and a
    /// nominal type, and (b) `let x: T` declarations with a simple-identifier
    /// pattern, an explicit type ascription, and a nominal type. Reference,
    /// pointer, and single-level generic types reduce to their core nominal
    /// type; anything else yields no entry ([`reduce_receiver_type`]).
    ///
    /// The dispatch environment maps a binding to its trait path as written
    /// when the ascribed type is provably trait-typed: a `dyn Trait` type
    /// (directly, or inside `Box`/`Rc`/`Arc` — the std smart pointers whose
    /// method calls deref to the trait object), or a bare type parameter with
    /// exactly one trait bound (`fn g<T: Trait>(t: T)`, `where T: Trait`).
    /// Multi-bound parameters and non-`dyn` types yield no dispatch entry.
    ///
    /// SHADOWING VETO (conservative): every binding occurrence of each
    /// identifier anywhere in the body is counted — additional `let` shadows,
    /// `for x in`, closure params, `if let`/`while let`, and `match` arm
    /// bindings, including nested destructuring. Any identifier bound at MORE
    /// THAN ONE site is non-provable and dropped from BOTH environments, so a
    /// shadowed receiver falls back to today's ambiguous fan-out (prefer a
    /// MISSING narrowing to a WRONG one). Only expression-position identifiers
    /// (the receiver USE `x.m()`) are never counted as binders, so a single
    /// typed binding survives.
    fn build_type_env(
        &self,
        fn_node: Node<'_>,
    ) -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let mut type_env: BTreeMap<String, String> = BTreeMap::new();
        let mut dispatch_env: BTreeMap<String, String> = BTreeMap::new();
        let mut binder_counts: BTreeMap<String, usize> = BTreeMap::new();

        // Type-parameter trait bounds for generic dispatch (issue #267): `T`
        // -> its single trait bound's as-written path, from inline bounds and
        // `where` clauses alike.
        let mut type_param_bounds: BTreeMap<String, String> = BTreeMap::new();
        self.collect_type_param_bounds(fn_node, &mut type_param_bounds);

        if let Some(params) = fn_node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            let children: Vec<Node<'_>> = params.named_children(&mut cursor).collect();
            for param in children {
                if param.kind() != "parameter" {
                    continue;
                }
                let Some(pattern) = param.child_by_field_name("pattern") else {
                    continue;
                };
                let mut idents = Vec::new();
                collect_pattern_idents(pattern, self.source, &mut idents);
                for ident in &idents {
                    *binder_counts.entry(ident.clone()).or_default() += 1;
                }
                if pattern.kind() != "identifier" {
                    continue;
                }
                let Some(type_node) = param.child_by_field_name("type") else {
                    continue;
                };
                let binding = self.node_text(pattern).trim().to_owned();
                // Dispatch first: a `dyn Trait` ascription (even wrapped in
                // `Box`/`Rc`/`Arc`) is trait-typed, never a nominal receiver
                // type — `reduce_receiver_type` would reduce `Box<dyn Trait>`
                // to the misleading `Box`.
                if let Some(trait_path) = reduce_dispatch_trait(type_node, self.source) {
                    dispatch_env.insert(binding, trait_path);
                } else if let Some(reduced) = reduce_receiver_type(type_node, self.source) {
                    type_env.insert(binding.clone(), reduced.clone());
                    // A generic type-parameter receiver (`fn g<T: Trait>(t: T)`
                    // or `fn g<T>(t: &T) where T: Trait`): the bound is the
                    // dispatch trait.
                    if let Some(bound) = type_param_bounds.get(reduced.as_str()) {
                        dispatch_env.insert(binding, bound.clone());
                    }
                }
            }
        }

        if let Some(body) = fn_node.child_by_field_name("body") {
            self.scan_body_binders(
                body,
                &mut type_env,
                &mut dispatch_env,
                &type_param_bounds,
                &mut binder_counts,
            );
        }

        type_env.retain(|name, _| binder_counts.get(name).copied() == Some(1));
        dispatch_env.retain(|name, _| binder_counts.get(name).copied() == Some(1));
        (type_env, dispatch_env)
    }

    /// Collects `type_parameter_name -> single trait bound path` from a
    /// function item's inline `type_parameters` and its `where_clause`
    /// (issue #267). Only a parameter with EXACTLY ONE DISTINCT simple trait
    /// bound across BOTH sources is recorded: bounds accumulate per parameter
    /// (a `T: A` inline plus a `T: B` where-clause is two bounds, not one),
    /// and multi-bound, lifetime-only, or complex bounds yield nothing.
    fn collect_type_param_bounds(&self, fn_node: Node<'_>, out: &mut BTreeMap<String, String>) {
        let mut accumulated: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut push_bound = |name: String, bound: String| {
            let entry = accumulated.entry(name).or_default();
            if !entry.contains(&bound) {
                entry.push(bound);
            }
        };
        if let Some(params) = fn_node.child_by_field_name("type_parameters") {
            let mut cursor = params.walk();
            let children: Vec<Node<'_>> = params.named_children(&mut cursor).collect();
            for param in children {
                if param.kind() != "type_parameter" {
                    continue;
                }
                let Some(name_node) = param.child_by_field_name("name") else {
                    continue;
                };
                let name = node_source(name_node, self.source).trim().to_owned();
                if name.is_empty() {
                    continue;
                }
                if let Some(bounds) = param.child_by_field_name("bounds")
                    && let Some(trait_path) = single_trait_bound_path(bounds, self.source)
                {
                    push_bound(name, trait_path);
                }
            }
        }
        // Where clauses: `fn g<T>(...) where T: Trait`. The grammar exposes the
        // `where_clause` as a direct named child of the function item, not a
        // field.
        let where_clause = {
            let mut cursor = fn_node.walk();
            fn_node
                .named_children(&mut cursor)
                .find(|child| child.kind() == "where_clause")
        };
        if let Some(where_clause) = where_clause {
            let mut cursor = where_clause.walk();
            let children: Vec<Node<'_>> = where_clause.named_children(&mut cursor).collect();
            for predicate in children {
                if predicate.kind() != "where_predicate" {
                    continue;
                }
                let Some(left) = predicate.child_by_field_name("left") else {
                    continue;
                };
                // A bare type parameter's `left` is a `type_identifier`; a
                // concrete type's predicate (`where Vec<T>: Clone`) is not a
                // dispatch source.
                if left.kind() != "type_identifier" {
                    continue;
                }
                let name = node_source(left, self.source).trim().to_owned();
                if name.is_empty() {
                    continue;
                }
                if let Some(bounds) = predicate.child_by_field_name("bounds")
                    && let Some(trait_path) = single_trait_bound_path(bounds, self.source)
                {
                    push_bound(name, trait_path);
                }
            }
        }
        for (name, bounds) in accumulated {
            // Exactly one DISTINCT bound across inline and where sources.
            if let [bound] = bounds.as_slice() {
                out.insert(name, bound.clone());
            }
        }
    }

    /// Recursively scans a function body, recording `let x: T` type-env entries
    /// and counting every binder occurrence for the shadowing veto (issue #441).
    /// Recursion stops at nested item scopes (`fn`/`impl`/`trait`/`mod`/macros),
    /// mirroring [`collect_call_sites`](Self::collect_call_sites), so a nested
    /// item's bindings never veto (or populate) the enclosing function's
    /// environment; closures ARE descended into (their call sites belong to this
    /// function, so their params shadow this scope).
    fn scan_body_binders(
        &self,
        node: Node<'_>,
        type_env: &mut BTreeMap<String, String>,
        dispatch_env: &mut BTreeMap<String, String>,
        type_param_bounds: &BTreeMap<String, String>,
        binder_counts: &mut BTreeMap<String, usize>,
    ) {
        let kind = node.kind();
        match kind {
            "let_declaration" => {
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    let mut idents = Vec::new();
                    collect_pattern_idents(pattern, self.source, &mut idents);
                    for ident in &idents {
                        *binder_counts.entry(ident.clone()).or_default() += 1;
                    }
                    if pattern.kind() == "identifier"
                        && let Some(type_node) = node.child_by_field_name("type")
                    {
                        let binding = self.node_text(pattern).trim().to_owned();
                        // Dispatch first, mirroring the param handling above:
                        // a `dyn Trait` ascription is trait-typed.
                        if let Some(trait_path) = reduce_dispatch_trait(type_node, self.source) {
                            dispatch_env.insert(binding, trait_path);
                        } else if let Some(reduced) = reduce_receiver_type(type_node, self.source) {
                            type_env.insert(binding.clone(), reduced.clone());
                            if let Some(bound) = type_param_bounds.get(reduced.as_str()) {
                                dispatch_env.insert(binding, bound.clone());
                            }
                        }
                    }
                }
            }
            "for_expression" | "let_condition" | "match_arm" => {
                if let Some(pattern) = node.child_by_field_name("pattern") {
                    let mut idents = Vec::new();
                    collect_pattern_idents(pattern, self.source, &mut idents);
                    for ident in &idents {
                        *binder_counts.entry(ident.clone()).or_default() += 1;
                    }
                }
            }
            "closure_expression" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    let mut idents = Vec::new();
                    collect_pattern_idents(params, self.source, &mut idents);
                    for ident in &idents {
                        *binder_counts.entry(ident.clone()).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
        if matches!(
            kind,
            "function_item"
                | "impl_item"
                | "trait_item"
                | "mod_item"
                | "macro_definition"
                | "macro_invocation"
        ) {
            return;
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            self.scan_body_binders(
                child,
                type_env,
                dispatch_env,
                type_param_bounds,
                binder_counts,
            );
        }
    }

    /// Normalizes a `::`-separated call path for matching: strips leading
    /// `crate`/`self`/`super` segments and rewrites a leading `Self` to the
    /// surrounding impl owner when known.
    fn normalize_call_path(&self, display: &str) -> Option<Vec<String>> {
        let mut segments: Vec<String> = display
            .split("::")
            .map(|segment| segment.trim().to_owned())
            .collect();
        while segments
            .first()
            .is_some_and(|first| matches!(first.as_str(), "crate" | "self" | "super"))
        {
            segments.remove(0);
        }
        if segments.first().is_some_and(|first| first == "Self") {
            segments.remove(0);
            if let Some(owner) = self
                .impl_context
                .as_ref()
                .and_then(|impl_context| normalize_impl_owner(&impl_context.method_owner))
            {
                segments.insert(0, owner);
            } else if let Some(trait_name) = &self.trait_context {
                // Inside a trait body there is no `impl_context`, so a
                // `Self::make()` call in a default method names the trait's OWN
                // associated item: substitute the enclosing trait as the owner
                // (issue #390), `["make"]` -> `["T", "make"]`. The existing
                // multi-segment `Path` suffix match then binds it to the trait
                // method's `["T", "make"]` segments. `impl_context` still takes
                // precedence when set. Because the match is an exact suffix,
                // this only mints an edge when `T::make` actually exists —
                // `Self::other()` with no `other` on the trait stays
                // unresolved, and it can never bind a free function `make`
                // (`["make"]`) nor another trait's `U::make` (`["U", "make"]`).
                segments.insert(0, trait_name.clone());
            }
        }
        (!segments.is_empty()).then_some(segments)
    }

    fn extract_impl(&mut self, node: Node<'_>) {
        if has_unsafe_modifier(node) {
            self.emit_unsafe_site(node, "impl");
        }
        let display = impl_display(self.node_text(node));
        let qualified_name = self.qualify(&display);
        let id = self.add_symbol(node, "impl", &qualified_name);
        self.definitions.insert(qualified_name, id.clone());

        // Compute the IMPLEMENTS-resolution decision now, from the AST node,
        // while the parse tree is in hand — never re-parsed from the display
        // string later (issue #343/#344).
        let decision = impl_target_decision(node, self.source, &display);

        // Capture a comprehensive `impl Trait for Type` relation for the
        // repo-wide IMPLEMENTS-gated self-dispatch join (issue #414). Unlike
        // `pending_impls` (deferred ONLY when local resolution fails), this
        // records EVERY trait impl — same-file and cross-file — so the
        // resolution pass has a complete "S implements T" fact set. Emit only
        // for a trait impl that names a concrete implementing type: a `trait`
        // field must be present (excludes inherent `impl S {}`, whose decision
        // is also `Resolve` but carries the TYPE name), and the decision must be
        // `Resolve` (excludes blanket/negative/non-nominal impls, whose decision
        // is `NoEdge`, and generic inherent impls, whose decision is
        // `Verbatim`). The `trait_path` reuses `impl_target_decision`'s exact
        // normalization so the join stays consistent with the IMPLEMENTS pass.
        if node.child_by_field_name("trait").is_some()
            && let ImplTargetDecision::Resolve(trait_path) = &decision
            && let Some(impl_type) = normalize_impl_owner(&display)
            && let Some(impl_type_path) = normalize_impl_owner_path(&display)
        {
            self.facts.impl_trait_relations.push(ImplTraitRelationFact {
                impl_type,
                impl_type_path,
                trait_path: trait_path.clone(),
                crate_root: self.crate_root.clone(),
                module_names: self.module_names.clone(),
            });
        }

        // The trait lookup is deferred until the whole file is indexed
        // (`resolve_pending_impl_edges`): Rust item order is insignificant,
        // so a trait defined after this impl must edge-back all the same.
        self.pending_impl_edges.push(PendingImplEdge {
            source_id: id.clone(),
            display: display.clone(),
            module_names: self.module_names.clone(),
            decision,
        });

        let previous = self.impl_context.replace(ImplContext {
            method_owner: method_owner(&display),
            display,
            id,
        });
        self.walk_children(node);
        self.impl_context = previous;
    }

    /// Visits a Tree-sitter comment node (`line_comment` / `block_comment` —
    /// rustdoc `///`, `//!`, and `/** */` docs included) and emits one
    /// deterministic `DebtMarker` record per debt-marker token occurrence
    /// (issue #218).
    ///
    /// Comment ranges come exclusively from the Tree-sitter parse tree, so a
    /// marker token inside a string or character literal can never match;
    /// scanning within the identified comment's text enforces word
    /// boundaries, so identifier substrings (`TODOIST`, `fixmeup`) and longer
    /// words (`XXXL`) never match either.
    fn extract_comment_markers(&mut self, node: Node<'_>) {
        let text = self.node_text(node);
        let comment_start_byte = node.start_byte();
        let comment_start_row = node.start_position().row;
        for marker in comment_debt_markers(text) {
            let disambiguator = self.next_debt_marker_disambiguator(marker.category);
            let id = stable_id(&[
                "node",
                "debt_marker",
                self.repository_id,
                &self.file.repo_relative_path,
                marker.category,
                &disambiguator.to_string(),
            ]);
            let line = comment_start_row + 1 + marker.line_offset;
            let marker_span = SourceSpan {
                start_byte: comment_start_byte + marker.token_start,
                end_byte: comment_start_byte + marker.note_end,
                start_line: line,
                end_line: line,
                // Debt-marker offsets are computed from comment byte offsets;
                // the column within the line is not tracked (issue #463).
                start_column: None,
                end_column: None,
            };
            self.graph.push(
                GraphRecord::syntax_node(
                    id.clone(),
                    NodeKind::DebtMarker,
                    self.file.repo_relative_path.clone(),
                    marker_span,
                    marker.category.to_owned(),
                    "rust",
                    format!(
                        "Rust {} debt-comment marker",
                        marker.category.to_ascii_uppercase()
                    ),
                )
                .with_note(&crate::redaction::redact_value(&marker.note))
                .with_redaction_policy_version(REDACTION_POLICY_VERSION),
            );
            self.add_edge(
                EdgeLabel::Contains,
                self.file_id.to_owned(),
                id,
                format!(
                    "{} contains {} debt-comment marker",
                    self.file.repo_relative_path,
                    marker.category.to_ascii_uppercase()
                ),
            );
        }
    }

    fn next_debt_marker_disambiguator(&mut self, category: &str) -> u64 {
        let disambiguator = self
            .debt_marker_ordinals
            .entry(category.to_owned())
            .or_default();
        let current = *disambiguator;
        *disambiguator += 1;
        current
    }

    fn extract_macro_diagnostic(&mut self, node: Node<'_>) {
        let invocation = macro_invocation_name(self.node_text(node));
        let disambiguator = self.next_diagnostic_disambiguator(&invocation);
        let id = stable_id(&[
            "node",
            "diagnostic",
            self.repository_id,
            &self.file.repo_relative_path,
            &invocation,
            &disambiguator.to_string(),
        ]);
        self.graph.push(GraphRecord::syntax_node(
            id,
            NodeKind::Diagnostic,
            self.file.repo_relative_path.clone(),
            span(node),
            invocation.clone(),
            "rust",
            format!("unsupported macro invocation {invocation}"),
        ));
    }

    /// Visits a `call_expression`: emits a deterministic `PanicRiskSite`
    /// record when the call is a `.unwrap()` / `.expect(..)` method call
    /// (issue #223), then keeps walking so nested and chained calls are
    /// visited too.
    ///
    /// Detection is purely AST-shaped — the callee must be a
    /// `field_expression` whose `field` child is a `field_identifier` in the
    /// closed [`PANIC_RISK_METHODS`] set — so text inside comments, string
    /// literals, doc comments, and unrelated `unwrap` identifiers can never
    /// match.
    fn extract_call_expression(&mut self, node: Node<'_>) {
        if let Some(category) = self.panic_risk_category(node) {
            self.emit_panic_risk_site(node, category);
        }
        self.walk_children(node);
    }

    /// Returns the closed panic-risk category (`unwrap` / `expect`) when the
    /// call expression is a matching method call; `None` otherwise.
    fn panic_risk_category(&self, node: Node<'_>) -> Option<&'static str> {
        let function = node.child_by_field_name("function")?;
        if function.kind() != "field_expression" {
            return None;
        }
        let field = function.child_by_field_name("field")?;
        if field.kind() != "field_identifier" {
            return None;
        }
        let name = self.node_text(field);
        PANIC_RISK_METHODS.iter().find(|m| **m == name).copied()
    }

    fn emit_panic_risk_site(&mut self, node: Node<'_>, category: &'static str) {
        let context = if self.in_test_context() {
            "test"
        } else {
            "production"
        };
        let disambiguator = self.next_panic_risk_disambiguator(category);
        let id = stable_id(&[
            "node",
            "panic_risk_site",
            self.repository_id,
            &self.file.repo_relative_path,
            category,
            &disambiguator.to_string(),
        ]);
        self.graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::PanicRiskSite,
                self.file.repo_relative_path.clone(),
                span(node),
                category.to_owned(),
                "rust",
                format!("Rust .{category}() panic-risk call site"),
            )
            .with_call_context(context),
        );
        self.add_edge(
            EdgeLabel::Contains,
            self.file_id.to_owned(),
            id,
            format!(
                "{} contains .{category}() panic-risk call site",
                self.file.repo_relative_path
            ),
        );
    }

    /// `true` when the cursor is inside any test scope: a file under a
    /// top-level `tests/` or `benches/` directory, a `#[cfg(test)]` module,
    /// or a `#[test]`-family function. The classification set is closed for
    /// issue #223; issue #238 reuses it as signals (b) and (c).
    const fn in_test_context(&self) -> bool {
        self.file_in_test_root || self.test_scope_depth > 0
    }

    /// Derives the deterministic test-vs-production role for one symbol
    /// (issue #238): `Test` when the item carries a test-family attribute
    /// (signal a) or sits in a test context (signals b/c, composed by
    /// [`Self::in_test_context`]); `Production` otherwise. A `mod` item
    /// carrying `#[cfg(test)]` is itself `Test`: its declaration is the
    /// lexical gate, even though the scope counter is entered only for its
    /// children. A bare `#[cfg(test)] fn` is NOT test — the gate applies to
    /// modules, not items. The full decision procedure is documented in
    /// `docs/cli/test-production-roles.md`.
    fn symbol_role(&self, node: Node<'_>) -> SymbolRole {
        if self.has_test_family_attribute(node)
            || self.in_test_context()
            || (node.kind() == "mod_item" && self.has_cfg_test_attribute(node))
        {
            SymbolRole::Test
        } else {
            SymbolRole::Production
        }
    }

    /// Builds the item's conditional-compilation gate chain (issue #190):
    /// the file's `#![cfg(...)]` inner attributes first, then every enclosing
    /// gated item/module outermost-first, then the item's own `#[cfg(...)]` /
    /// `#[cfg_attr(...)]` predicates. Empty when the item is ungated — the
    /// caller stamps nothing, so ungated records carry no `cfg` field. Pure
    /// function of the AST: deterministic and walk-order independent.
    fn cfg_gate_chain(&self, node: Node<'_>) -> Vec<String> {
        let mut gates = self.file_cfg_gates.clone();
        gates.extend(enclosing_cfg_gates(node, self.source));
        gates.extend(own_cfg_gates(node, self.source));
        gates
    }

    /// `true` when the item carries a test-family attribute in the attribute
    /// items immediately preceding it: `#[test]`, `#[bench]`, or a path
    /// attribute ending in `::test` / `::bench` (e.g. `#[tokio::test]`),
    /// with or without arguments (issue #238 signal a). Reuses issue #240's
    /// closed [`entry_point_kind_from_attribute`] vocabulary rather than
    /// defining a second one; configuration attributes that merely mention
    /// these tokens (`#[cfg(test)]`, `#[cfg_attr(test, ...)]`) never match.
    fn has_test_family_attribute(&self, node: Node<'_>) -> bool {
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    if entry_point_kind_from_attribute(self.node_text(sibling))
                        == Some(EntryPointKind::Test)
                    {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        false
    }

    /// `true` when the function carries a dedicated test attribute in the
    /// attribute items immediately preceding it: `#[test]` or a path
    /// attribute ending in `::test` (e.g. `#[tokio::test]`), including
    /// parameterized forms. `#[cfg(not(test))]` and `#[cfg_attr(test, ...)]`
    /// never match — the issue #223 panic-risk context contract is closed.
    fn has_test_attribute(&self, node: Node<'_>) -> bool {
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    if attribute_is_test(self.node_text(sibling)) {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        false
    }

    /// Collects routing attributes (`#[get("/path")]`, `#[post("/path")]`, …)
    /// on the item preceding `node`, walking prev-siblings exactly like
    /// [`Self::has_test_attribute`] (issue #445). Each attribute whose name is
    /// in the closed HTTP-method set and which carries a string-literal path
    /// yields one [`RouteAnnotation`]. Extraction is Tree-sitter node walking —
    /// never regex. Results are in source order (the prev-sibling walk visits
    /// nearest-first, then reverses), so a handler with several method
    /// attributes records them deterministically.
    fn route_annotations(&self, node: Node<'_>) -> Vec<RouteAnnotation> {
        let mut collected: Vec<RouteAnnotation> = Vec::new();
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    if let Some(annotation) = self.route_annotation_from_attribute(sibling) {
                        collected.push(annotation);
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        collected.reverse();
        collected
    }

    /// Parses one `attribute_item` into a [`RouteAnnotation`] when it is a
    /// routing attribute: its name identifier is in the closed HTTP-method set
    /// (case-insensitive) and its token tree carries at least one string
    /// literal (the route path is the first such literal). Returns `None` for
    /// every other attribute. Tree-sitter node walking only.
    fn route_annotation_from_attribute(&self, attribute_item: Node<'_>) -> Option<RouteAnnotation> {
        let attribute = first_descendant_of_kind(attribute_item, "attribute")?;
        // The attribute's name path is its first named child (`identifier` for
        // `#[get(...)]`, `scoped_identifier` for `#[rocket::get(...)]`); reduce
        // it to its trailing segment before matching the method set.
        let name_node = attribute.named_child(0)?;
        let raw_name = self.node_text(name_node).trim().to_owned();
        let leaf = raw_name.rsplit("::").next().unwrap_or(&raw_name).trim();
        let method = http_method_from_attribute_name(leaf)?;
        let literal = first_descendant_of_kind(attribute, "string_literal")?;
        let path = string_literal_text(self.node_text(literal).trim())?;
        Some(RouteAnnotation {
            method: method.to_owned(),
            path,
        })
    }

    /// Collects the `#[deprecated]` attribute facts on the item preceding
    /// `node`, walking prev-siblings exactly like [`Self::has_test_attribute`]
    /// (issue #249). The nearest `deprecated` attribute wins; the mark's
    /// *presence* is the fact, so a bare `#[deprecated]` records a mark with
    /// both payloads absent. Returns `None` when no `deprecated` attribute
    /// precedes the item.
    fn deprecation_attribute(&self, node: Node<'_>) -> Option<DeprecationMark> {
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    if let Some(mark) = deprecation_from_attribute(sibling, self.source) {
                        return Some(mark);
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        None
    }

    /// Collects the non-call entry-point facts on the item at `node` (issue
    /// #240): `#[test]` / `#[bench]` (or a path attribute ending in
    /// `::test` / `::bench`, e.g. `#[tokio::test]`), `#[no_mangle]` /
    /// `#[export_name = "..."]`, and a free `fn main` in a binary crate
    /// root. Attribute siblings are walked exactly like
    /// [`Self::deprecation_attribute`]; the nearest matching attribute
    /// wins. Returns `None` when the item is not a recognized entry point.
    fn entry_point_mark(
        &self,
        node: Node<'_>,
        symbol_kind: &str,
        qualified_name: &str,
    ) -> Option<EntryPointMark> {
        // A free `fn main` in `src/main.rs` / `src/bin/**` is the binary's
        // entry point. `enclosing_fn_ids` holds only lexically-enclosing
        // functions here (the item's own id is pushed after `add_symbol`),
        // so a non-empty stack means a nested `fn main`, not the entry.
        // Trait-method signatures and default-bodied trait methods also
        // arrive with `symbol_kind == "function"` but are not free items: a
        // `trait_item` ancestor disqualifies them.
        if symbol_kind == "function"
            && self.enclosing_fn_ids.is_empty()
            && qualified_name.rsplit("::").next() == Some("main")
            && is_binary_crate_path(&self.file.repo_relative_path)
            && !has_trait_item_ancestor(node)
        {
            return Some(EntryPointMark {
                kind: EntryPointKind::BinaryEntry,
            });
        }
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    if let Some(kind) = entry_point_kind_from_attribute(self.node_text(sibling)) {
                        return Some(EntryPointMark { kind });
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        None
    }
    /// Collects route-registration references from a `macro_invocation` when its
    /// macro name is in the closed registration set (`routes`) (issue #445).
    /// Each bare `identifier` token inside the macro's `token_tree` is recorded
    /// as one unqualified handler reference to be resolved cross-file to a
    /// handler Symbol. Every other macro contributes nothing (the token tree is
    /// not walked). Path-qualified handler references (`module::handler` inside
    /// the token tree) are out of this slice's scope — a raw token tree does not
    /// parse `::` paths, and the fixture registers imported bare names.
    fn collect_route_registrations(&mut self, node: Node<'_>, caller_id: &str, caller_name: &str) {
        let Some(macro_node) = node.child_by_field_name("macro") else {
            return;
        };
        let raw = self.node_text(macro_node).trim().to_owned();
        let leaf = raw.rsplit("::").next().unwrap_or(&raw).trim();
        if !is_route_registration_macro(leaf) {
            return;
        }
        // The `token_tree` child carries no Tree-sitter field name, so locate it
        // by kind among the macro invocation's direct children.
        let mut macro_cursor = node.walk();
        let Some(token_tree) = node
            .children(&mut macro_cursor)
            .find(|child| child.kind() == "token_tree")
        else {
            return;
        };
        let mut cursor = token_tree.walk();
        let idents: Vec<Node<'_>> = token_tree
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .collect();
        for ident in idents {
            let name = self.node_text(ident).trim().to_owned();
            if name.is_empty() || !is_simple_ident(&name) {
                continue;
            }
            self.facts
                .route_registration_sites
                .push(RouteRegistrationFact {
                    owner_id: caller_id.to_owned(),
                    owner_name: caller_name.to_owned(),
                    handler_display: name.clone(),
                    handler_segments: vec![name],
                    path_root: CallPathRoot::Unqualified,
                    span: span(ident),
                });
        }
    }

    /// `true` when the item is annotated with exactly `#[cfg(test)]` in the
    /// attribute items immediately preceding it (comments are skipped).
    fn has_cfg_test_attribute(&self, node: Node<'_>) -> bool {
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "attribute_item" => {
                    let text: String = self
                        .node_text(sibling)
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect();
                    if text == "#[cfg(test)]" {
                        return true;
                    }
                }
                "line_comment" | "block_comment" => {}
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        false
    }

    fn next_panic_risk_disambiguator(&mut self, category: &str) -> u64 {
        let disambiguator = self
            .panic_risk_ordinals
            .entry(category.to_owned())
            .or_default();
        let current = *disambiguator;
        *disambiguator += 1;
        current
    }

    /// Visits a trait-method or foreign-function signature.
    ///
    /// Emits a deterministic `UnsafeSite` record when the declaration is an
    /// `unsafe fn` (issue #222).
    ///
    /// A `function_signature_item` appears in two contexts, both routed here.
    /// A signature-only trait method (nearest enclosing item is a
    /// `trait_item`) is a first-class citable `Symbol`, recorded exactly like
    /// a default-bodied trait method — kind `"function"`, `qualify(local_name)`
    /// qualified name, a `DEFINES` edge from the owning scope (a trait
    /// establishes no owner scope), plus the same `definitions` registration
    /// and `DefinitionFact` so a call to a body-less trait method has a
    /// definition to resolve to (issue #342). A foreign declaration inside an
    /// `extern` block (nearest enclosing item is a `foreign_mod_item`) is out
    /// of scope: it mints no Symbol, matching the pre-#342 behavior.
    fn extract_function_signature(&mut self, node: Node<'_>) {
        if has_unsafe_modifier(node) {
            self.emit_unsafe_site(node, "fn");
        }
        if !signature_is_trait_method(node) {
            // Foreign (`extern` block) declaration: symbol-less, out of scope.
            self.walk_children(node);
            return;
        }
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        // A signature-only item never carries an `impl_context` (impl methods
        // always have bodies), so the kind/name selection mirrors
        // `extract_function`'s free-item branch: kind `"function"`, qualified
        // by the enclosing module path only.
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, "function", &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());
        let is_trait_method = self.is_direct_trait_method(node);
        // A signature-only item never carries an `impl_context` (impl methods
        // always have bodies), so it is never a direct impl method (issue #413).
        self.facts.definitions.push(DefinitionFact {
            id,
            qualified_name: qualified_name.clone(),
            simple_name: local_name.clone(),
            match_segments: self.definition_match_segments(&local_name, is_trait_method, false),
            symbol_kind: "function".to_owned(),
            is_trait_method,
            repo_relative_path: self.file.repo_relative_path.clone(),
        });
        // No `SymbolBody` and no `collect_call_sites`: a signature-only
        // declaration has no body to scan for call sites.
        self.walk_children(node);
    }

    /// Visits an `unsafe { .. }` block expression: emits a deterministic
    /// `UnsafeSite` record (issue #222), then keeps walking so nested blocks,
    /// calls, and items are visited too.
    ///
    /// Detection is purely AST-shaped — the node kind must be `unsafe_block`
    /// — so the word `unsafe` inside comments, string literals, doc comments,
    /// and identifiers can never match.
    fn extract_unsafe_block(&mut self, node: Node<'_>) {
        self.emit_unsafe_site(node, "block");
        self.walk_children(node);
    }

    /// Emits one deterministic `UnsafeSite` record plus the `CONTAINS` edge
    /// from the owning file. `site_kind` is drawn from the closed
    /// [`UNSAFE_SITE_KINDS`] set and carried in the record's `name` field.
    fn emit_unsafe_site(&mut self, node: Node<'_>, site_kind: &'static str) {
        debug_assert!(UNSAFE_SITE_KINDS.contains(&site_kind));
        let disambiguator = self.next_unsafe_site_disambiguator(site_kind);
        let id = stable_id(&[
            "node",
            "unsafe_site",
            self.repository_id,
            &self.file.repo_relative_path,
            site_kind,
            &disambiguator.to_string(),
        ]);
        self.graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::UnsafeSite,
            self.file.repo_relative_path.clone(),
            span(node),
            site_kind.to_owned(),
            "rust",
            format!("Rust unsafe {site_kind} site"),
        ));
        self.add_edge(
            EdgeLabel::Contains,
            self.file_id.to_owned(),
            id,
            format!(
                "{} contains unsafe {site_kind} site",
                self.file.repo_relative_path
            ),
        );
    }

    fn next_unsafe_site_disambiguator(&mut self, site_kind: &str) -> u64 {
        let disambiguator = self
            .unsafe_site_ordinals
            .entry(site_kind.to_owned())
            .or_default();
        let current = *disambiguator;
        *disambiguator += 1;
        current
    }

    /// Visits an `attribute_item` (`#[...]`) or `inner_attribute_item`
    /// (`#![...]`): emits one deterministic `LintSuppression` record when the
    /// attribute is an `allow` suppression (issue #227), then keeps walking
    /// so any nested content (e.g. comments inside the token tree) is
    /// visited exactly as before.
    ///
    /// Detection is purely AST-shaped — the attribute's own name leaf must be
    /// exactly `allow` — so `#[allow(...)]` text inside line comments, doc
    /// comments, block comments, and string literals can never match (those
    /// are `line_comment` / `block_comment` / `string_literal` nodes, never
    /// attribute nodes).
    fn extract_lint_suppression(&mut self, node: Node<'_>) {
        if let Some(facts) = lint_suppression_from_attribute(node, self.source) {
            self.emit_lint_suppression(node, facts);
        }
        self.walk_children(node);
    }

    /// Emits one deterministic `LintSuppression` record plus the `CONTAINS`
    /// edge from the owning file. The record's `name` carries the closed
    /// scope string (`item` / `module` / `crate`); the sorted lint names and
    /// the adjacent justification-comment signal ride the additive
    /// [`LintSuppressionFacts`] payload.
    fn emit_lint_suppression(&mut self, node: Node<'_>, facts: LintSuppressionFacts) {
        let scope = facts.scope.as_str();
        let disambiguator = self.next_lint_suppression_disambiguator(scope);
        let id = stable_id(&[
            "node",
            "lint_suppression",
            self.repository_id,
            &self.file.repo_relative_path,
            scope,
            &disambiguator.to_string(),
        ]);
        self.graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::LintSuppression,
                self.file.repo_relative_path.clone(),
                span(node),
                scope.to_owned(),
                "rust",
                format!("Rust #[allow(...)] lint suppression ({scope} scope)"),
            )
            .with_lint_suppression(facts),
        );
        self.add_edge(
            EdgeLabel::Contains,
            self.file_id.to_owned(),
            id,
            format!(
                "{} contains #[allow(...)] lint suppression",
                self.file.repo_relative_path
            ),
        );
    }

    fn next_lint_suppression_disambiguator(&mut self, scope: &str) -> u64 {
        let disambiguator = self
            .lint_suppression_ordinals
            .entry(scope.to_owned())
            .or_default();
        let current = *disambiguator;
        *disambiguator += 1;
        current
    }

    fn add_symbol(&mut self, node: Node<'_>, symbol_kind: &str, qualified_name: &str) -> String {
        let disambiguator = self.next_symbol_disambiguator(symbol_kind, qualified_name);
        let id = stable_id(&[
            "node",
            "symbol",
            symbol_kind,
            self.repository_id,
            &self.file.repo_relative_path,
            qualified_name,
            &disambiguator.to_string(),
        ]);
        let node_text = self.node_text(node);
        let normalized = normalize_code(node_text);
        let mut record = GraphRecord::syntax_symbol(
            id.clone(),
            symbol_kind,
            self.file.repo_relative_path.clone(),
            span(node),
            qualified_name.to_owned(),
            "rust",
            disambiguator,
            format!("Rust {symbol_kind} {qualified_name}\nSource:\n{normalized}"),
        );
        if carries_declaration_surface(symbol_kind) {
            let doc = self.symbol_doc(node);
            let doc_present = doc.is_some();
            record = record.with_declaration_surface(
                Some(self.symbol_visibility(node).to_owned()),
                Some(self.symbol_signature(node)),
                doc,
            );
            if doc_present {
                record = record.with_redaction_policy_version(REDACTION_POLICY_VERSION);
            }
        }
        let route = self.route_annotations(node);
        if !route.is_empty() {
            record = record.with_route(route);
        }
        // Deprecation facts (issue #249): the mark's presence is the fact.
        // The `since` / `note` payloads passed through redaction policy v1
        // (like doc facts), so the policy version is stamped too.
        if let Some(deprecation) = self.deprecation_attribute(node) {
            record = record.with_deprecated(deprecation);
            record = record.with_redaction_policy_version(REDACTION_POLICY_VERSION);
        }
        // Entry-point facts (issue #240): the mark's presence is the fact —
        // a recognized non-call entry point is excluded from dead-code
        // triage candidacy downstream. Pure attribute/path signal, never
        // an identity input.
        if let Some(entry_point) = self.entry_point_mark(node, symbol_kind, qualified_name) {
            record = record.with_entry_point(entry_point);
        }
        // Test-vs-production role (issue #238): stamped on EVERY symbol
        // record — the classification is total, so there is no unknown case
        // at extraction time. Additive, never an identity input.
        record = record.with_role(self.symbol_role(node));
        // Conditional-compilation gates (issue #190): the item's own
        // predicates plus every enclosing gated item/module, outermost first.
        // Stamped only when the chain is non-empty — ungated symbols carry no
        // `cfg` field, never a fabricated gate. Additive, never an identity
        // input.
        let cfg_gates = self.cfg_gate_chain(node);
        if !cfg_gates.is_empty() {
            record = record.with_cfg(cfg_gates);
        }
        // Structural complexity (issue #162): stamped on callable symbols
        // (`function` / `method` / `test`) — a source-derived code fact, 1
        // plus one per decision point in the item's own body. Additive,
        // never an identity input.
        if matches!(symbol_kind, "function" | "method" | "test") {
            record = record.with_complexity(control_flow_complexity(node));
        }
        self.graph.push(record);
        self.add_edge(
            EdgeLabel::Defines,
            self.owner_id(),
            id.clone(),
            format!("{} defines {qualified_name}", self.owner_name()),
        );
        id
    }

    /// Maps the item's `pub` modifier onto the closed visibility set from
    /// issue #124: `public`, `crate`, `restricted`, or `private`.
    ///
    /// `pub(self)` is semantically private; `pub(super)` and `pub(in path)`
    /// map to `restricted`. Items with no visibility modifier are `private`.
    fn symbol_visibility(&self, node: Node<'_>) -> &'static str {
        let Some(modifier) = visibility_modifier(node) else {
            return "private";
        };
        let text: String = self
            .node_text(modifier)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        match text.as_str() {
            "pub" => "public",
            "pub(crate)" => "crate",
            "pub(self)" => "private",
            _ => "restricted",
        }
    }

    /// Extracts the normalized declaration header: item keyword through the
    /// end of the parameter list / return type / where-clause for callables,
    /// or the item header for type-defining items. The visibility modifier is
    /// excluded (it is carried by the `visibility` field), the body is
    /// excluded, and interior whitespace is collapsed via [`normalize_code`].
    fn symbol_signature(&self, node: Node<'_>) -> String {
        let start = visibility_modifier(node).map_or_else(|| node.start_byte(), |v| v.end_byte());
        let end = node
            .child_by_field_name("body")
            .filter(|body| {
                matches!(
                    body.kind(),
                    "block" | "field_declaration_list" | "enum_variant_list" | "declaration_list"
                )
            })
            .map_or_else(|| node.end_byte(), |body| body.start_byte());
        normalize_code(self.source.get(start..end).unwrap_or(""))
    }

    /// Collects the item's doc comment (`///` line docs, a `/** */` block
    /// doc, or `#[doc = "..."]` attributes) from the siblings immediately
    /// preceding the item, then applies redaction policy v1 to the collected
    /// text.
    ///
    /// Non-doc attribute items between the docs and the item are skipped; any
    /// other sibling (including plain `//` / `/* */` comments) terminates the
    /// doc block. Returns `None` when the item has no doc comment or the
    /// collected text is empty — the `doc` field is omitted, never an empty
    /// string.
    fn symbol_doc(&self, node: Node<'_>) -> Option<String> {
        let mut doc_parts: Vec<String> = Vec::new();
        let mut current = node.prev_sibling();
        while let Some(sibling) = current {
            match sibling.kind() {
                "line_comment" | "block_comment" => {
                    let Some(text) = doc_comment_text(self.node_text(sibling)) else {
                        break;
                    };
                    doc_parts.push(text);
                }
                "attribute_item" => {
                    if let Some(text) = doc_attribute_text(self.node_text(sibling)) {
                        doc_parts.push(text);
                    }
                }
                _ => break,
            }
            current = sibling.prev_sibling();
        }
        if doc_parts.is_empty() {
            return None;
        }
        doc_parts.reverse();
        let joined = doc_parts.join("\n");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(crate::redaction::redact_value(trimmed))
        }
    }

    fn next_symbol_disambiguator(&mut self, symbol_kind: &str, qualified_name: &str) -> u64 {
        next_symbol_ordinal(&mut self.symbol_ordinals, symbol_kind, qualified_name)
    }

    fn next_diagnostic_disambiguator(&mut self, invocation: &str) -> u64 {
        let disambiguator = self
            .diagnostic_ordinals
            .entry(invocation.to_owned())
            .or_default();
        let current = *disambiguator;
        *disambiguator += 1;
        current
    }

    fn add_edge(&mut self, label: EdgeLabel, source: String, target: String, summary: String) {
        add_graph_edge(self.graph, label, source, target, summary);
    }

    fn emit_reference_edges(&mut self) {
        // Issue #267: (body ID, simple callee name) pairs with a trait-dispatch
        // call site are repo-wide owned — the cross-file pass emits those
        // pairs with their resolution labels (same-file included). The textual
        // pass stays silent for them so its unlabeled edges never collide with
        // (shadow) the owning pass's stable edge IDs.
        let suppressed: BTreeSet<(String, String)> = self
            .facts
            .call_sites
            .iter()
            .filter(|call| call.dispatch_trait.is_some())
            .filter_map(|call| {
                call.callee_segments
                    .last()
                    .map(|name| (call.caller_id.clone(), name.clone()))
            })
            .collect();
        // A block-local `fn` shadows its bare simple name for the enclosing
        // body (issue #422): the text pass must not emit an edge from the
        // enclosing body to the shadowed module-level definition — the bare
        // call binds the block-local, whose edge the scope-gated resolver
        // emits instead. Grouped here (after the full walk) so definitions
        // nested anywhere in the body are all visible.
        let mut shadowed_names: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for definition in &self.facts.block_local_definitions {
            shadowed_names
                .entry(definition.enclosing_scope_id.clone())
                .or_default()
                .push(definition.simple_name.clone());
        }
        emit_reference_edges(
            self.graph,
            &self.definitions,
            &self.symbol_bodies,
            &suppressed,
            &shadowed_names,
        );
    }

    /// Resolves every deferred impl trait lookup after the whole file has
    /// been indexed, so a trait defined after its impl edge-backs all the
    /// same (Rust item order is insignificant). Emission follows source
    /// order, keeping the output deterministic.
    fn resolve_pending_impl_edges(&mut self) {
        let pending = std::mem::take(&mut self.pending_impl_edges);
        for entry in pending {
            // The resolution decision was computed at walk time from the AST
            // node (`impl_target_decision`), covering `unsafe impl ...`,
            // non-generic headers, and generic headers (`impl<T> Trait for
            // Type<T>`, `impl GenP<u32> for Plain`) uniformly (issue #343).
            match entry.decision.clone() {
                ImplTargetDecision::Resolve(trait_name) => {
                    if let Some(target) =
                        self.resolve_impl_trait_locally(&trait_name, &entry.module_names)
                    {
                        self.add_edge(
                            EdgeLabel::Implements,
                            entry.source_id,
                            target,
                            format!("{} implementation relationship", entry.display),
                        );
                    } else {
                        // The trait/type is not defined in THIS file. Defer to
                        // the repo-wide cross-file IMPLEMENTS pass (issue #344),
                        // which retries the same trait path against every
                        // file's exported trait/type definitions. Local
                        // resolution always wins, so a resolved edge here is
                        // never re-emitted cross-file.
                        //
                        // Carry the AST-derived import-shadow verdict forward:
                        // when this bare trait/type name is shadowed by a `use`
                        // visible in the impl's module scope, the cross-file
                        // resolver must veto it too (name refers to the import),
                        // so the two paths share one answer via the same
                        // predicate (issues #343/#344 round 9, follow-up #393).
                        let shadowed_by_use = !trait_name.contains("::")
                            && self.scope_imports_bare_name(&entry.module_names, &trait_name);
                        self.facts.pending_impls.push(PendingImplFact {
                            source_id: entry.source_id,
                            trait_path: trait_name,
                            crate_root: self.crate_root.clone(),
                            module_names: entry.module_names,
                            shadowed_by_use,
                        });
                    }
                }
                ImplTargetDecision::Verbatim => {
                    // A generic inherent impl (`impl<T> Type<T>`, no `for`
                    // clause) names no trait to reach. Keep the recorded
                    // self-referential edge via the verbatim display key:
                    // space-containing display keys never collide with
                    // identifier names. No cross-file lookup (no trait named).
                    if let Some(target) = self.definitions.get(entry.display.trim()).cloned() {
                        self.add_edge(
                            EdgeLabel::Implements,
                            entry.source_id,
                            target,
                            format!("{} implementation relationship", entry.display),
                        );
                    }
                }
                // A blanket impl (`impl<T> Trait for T`) whose `for` target is
                // a bare binder type parameter covers every type and has no
                // single implementing-type record; it mints no IMPLEMENTS edge.
                ImplTargetDecision::NoEdge => {}
            }
        }
    }

    /// Serializes the captured module-item `use`-import PATHS relevant to this
    /// file's bare pending impls into [`FileFacts::use_trait_imports`] (issue
    /// #393), so the deferred cross-file resolver can bind a bare imported
    /// trait/type name to its true aliased target. Only imports that a bare
    /// pending impl in the SAME module scope actually names are emitted, keeping
    /// the fact vector (and cache) minimal. Output is deterministically ordered.
    fn finalize_use_imports(&mut self) {
        let mut seen: BTreeSet<(Vec<String>, String)> = BTreeSet::new();
        let mut imports: Vec<UseImportFact> = Vec::new();
        for pending in &self.facts.pending_impls {
            if pending.trait_path.contains("::") {
                continue;
            }
            let key = (pending.module_names.clone(), pending.trait_path.clone());
            if seen.contains(&key) {
                continue;
            }
            if let Some(paths) = self
                .import_paths_by_scope
                .get(&pending.module_names)
                .and_then(|scope| scope.get(&pending.trait_path))
            {
                seen.insert(key);
                // Emit ONE fact per DISTINCT resolved path (finding E): a bare
                // name bound to 2+ paths by cfg-gated imports surfaces as
                // multiple `use_trait_imports` entries the resolver treats as
                // ambiguous (`lookup_use_import` returns `None` on distinct-path
                // multiplicity), so import-aware resolution never fires and no
                // edge is minted. A single binding still emits exactly one fact.
                for path in paths {
                    imports.push(UseImportFact {
                        module_names: pending.module_names.clone(),
                        simple_name: pending.trait_path.clone(),
                        resolved_path: path.clone(),
                    });
                }
            }
        }
        // Sort by resolved_path too so multiple same-name facts are ordered
        // deterministically (byte-identical output across runs).
        imports.sort_by(|a, b| {
            (&a.module_names, &a.simple_name, &a.resolved_path).cmp(&(
                &b.module_names,
                &b.simple_name,
                &b.resolved_path,
            ))
        });
        self.facts.use_trait_imports = imports;
    }

    /// Resolves a normalized impl trait path against THIS file's indexed
    /// trait/type definitions only, returning the target record ID when the
    /// trait is defined locally. A miss means the trait is defined in another
    /// file (or is external) and the impl is deferred to the repo-wide
    /// cross-file pass (issue #344).
    fn resolve_impl_trait_locally(&self, target: &str, module_names: &[String]) -> Option<String> {
        if target.contains("::") {
            // An absolute `crate::`/`self::`/`super::` path resolves against
            // the module-qualified key space only: a nested symbol's
            // bare-name alias in `definitions` must never shadow the root
            // item the path denotes.
            if let Some(normalized) = Self::normalize_local_trait_path(target, module_names) {
                return self.qualified_definitions.get(&normalized).cloned();
            }
            // A relative qualified path (`sibling::T`) resolves in the
            // impl's module scope first, walking outward to the crate root
            // (`m::sibling::T`, then `sibling::T`) — mirroring the
            // unqualified scope walk. Cross-crate paths (`std::fmt::Debug`)
            // match nothing and stay unresolved.
            for depth in (0..=module_names.len()).rev() {
                let candidate = if depth == 0 {
                    target.to_owned()
                } else {
                    format!("{}::{target}", module_names[..depth].join("::"))
                };
                if let Some(id) = self.qualified_definitions.get(&candidate) {
                    return Some(id.clone());
                }
            }
            // No general `definitions` fallback here: beyond the qualified
            // impl-target keys the scope walk already checked, that map
            // holds only value-namespace and callable names, which must
            // never capture an IMPLEMENTS edge.
            return None;
        }
        if target.contains("::") {
            // An absolute `crate::`/`self::`/`super::` path resolves against
            // the module-qualified key space only: a nested symbol's
            // bare-name alias in `definitions` must never shadow the root
            // item the path denotes.
            if let Some(normalized) = Self::normalize_local_trait_path(target, module_names) {
                return self.qualified_definitions.get(&normalized).cloned();
            }
            // A relative qualified path (`sibling::T`) resolves in the
            // impl's module scope first, walking outward to the crate root
            // (`m::sibling::T`, then `sibling::T`) — mirroring the
            // unqualified scope walk. Cross-crate paths (`std::fmt::Debug`)
            // match nothing and stay unresolved.
            for depth in (0..=module_names.len()).rev() {
                let candidate = if depth == 0 {
                    target.to_owned()
                } else {
                    format!("{}::{target}", module_names[..depth].join("::"))
                };
                if let Some(id) = self.qualified_definitions.get(&candidate) {
                    return Some(id.clone());
                }
            }
            // No general `definitions` fallback here: beyond the qualified
            // impl-target keys the scope walk already checked, that map
            // holds only value-namespace and callable names, which must
            // never capture an IMPLEMENTS edge.
            return None;
        }
        // Own-module definition wins at depth 0 BEFORE the veto (round-10 Codex
        // finding): a trait/type defined in the impl's OWN module binds a bare
        // name directly. In real Rust a same-module `use Name` PLUS a same-module
        // `item Name` is a name collision (a compile error), so trusting the
        // own-module definition here is safe and never masks a valid import —
        // and it keeps an ancestor/root `use` from ever suppressing an impl of a
        // same-named trait declared IN this module.
        let own_scope_candidate = if module_names.is_empty() {
            target.to_owned()
        } else {
            format!("{}::{target}", module_names.join("::"))
        };
        if let Some(id) = self.qualified_definitions.get(&own_scope_candidate) {
            return Some(id.clone());
        }
        // Import-shadow veto (issues #343/#344): a module-item `use` in the
        // impl's OWN module scope whose final bound segment equals this bare name
        // means the bare name refers to the IMPORT, not any outward same-name
        // definition. Leave the impl unresolved rather than walk outward to a
        // shallower/root same-name def — this is the AST-derived, name-shadow
        // boolean that closes the bare-name wrong-edge family (external/std
        // imports the local index cannot see AND non-root local aliases alike),
        // a strict generalization of the round-8 same-name ambiguity guard
        // below. Only the impl's own module scope is consulted (Rust `use`
        // visibility is not inherited) and only module-item imports feed it, so
        // block-local and ancestor-scope imports never fire. A qualified path
        // never reaches here, so this only vetoes bare names. Correct
        // import-aware resolution is follow-up #393.
        if self.scope_imports_bare_name(module_names, target) {
            return None;
        }
        // The own-module level was already checked above; now walk OUTWARD from
        // the nearest enclosing module to the crate root through the
        // qualified-only key space, so a same-named trait in an unrelated
        // nested module can never shadow the in-scope one via its bare alias.
        for depth in (0..module_names.len()).rev() {
            let candidate = if depth == 0 {
                target.to_owned()
            } else {
                format!("{}::{target}", module_names[..depth].join("::"))
            };
            if let Some(id) = self.qualified_definitions.get(&candidate) {
                // A match found only by walking OUTWARD to a shallower/root
                // module is the exact shape of the round-8 finding: `mod m { use
                // crate::a::T; impl T for X }` scope-walks bare `T` past the
                // (absent) `m::T` and binds the ROOT `T`, but the `use` alias
                // means `a::T`. When such an outward bind's simple name is
                // ambiguous across this file's impl-target definitions (root `T`
                // AND `a::T`), a `use` alias could redirect it, so leave it
                // unresolved rather than mint a WRONG edge — the local/inline
                // analog of the cross-file guard, sharing its counting predicate
                // ([`bare_simple_name_is_ambiguous`]) so the two never diverge.
                if self.bare_name_is_ambiguous(target) {
                    return None;
                }
                return Some(id.clone());
            }
        }
        // Final fallback: the bare alias still covers names the scope walk
        // cannot see, such as use-imported traits from another module — but
        // only through the type-namespace view, so a later value-namespace
        // item (`fn T()`) can never capture the edge. This lookup is inherently
        // an outward/foreign bind (the scope walk already missed every
        // in-scope module), so the same-name ambiguity guard applies here too:
        // an ambiguous bare alias (`a::T` AND `b::T`, no in-scope `T`) picks one
        // arbitrarily by insertion order, exactly the WRONG-edge class the guard
        // closes.
        if self.bare_name_is_ambiguous(target) {
            return None;
        }
        self.type_definitions.get(target).cloned()
    }

    /// Reports whether the bare simple name `target` is ambiguous across THIS
    /// file's impl-target definitions — more than one distinct qualified name
    /// (root `T`, `a::T`, …) shares it. Delegates to the shared
    /// [`bare_simple_name_is_ambiguous`] counting predicate over the
    /// impl-target-only `qualified_definitions` key space (already
    /// [`is_impl_target_kind`]-filtered at insertion), so the local guard and
    /// the cross-file guard count the same target-kinds the same way.
    fn bare_name_is_ambiguous(&self, target: &str) -> bool {
        bare_simple_name_is_ambiguous(
            target,
            self.qualified_definitions.keys().map(String::as_str),
        )
    }

    /// Resolves a `crate::` / `self::` / `super::` qualifier on an impl's
    /// trait path to the module-qualified name `qualify` records in
    /// `definitions`, so `impl crate::T for X` edge-backs exactly like
    /// `impl T for X` when the trait is defined in this file.
    ///
    /// `crate::` paths are taken as written from the crate root;
    /// `self::` / `super::` resolve against the impl's enclosing module
    /// path. Returns `None` for unqualified paths (already looked up
    /// verbatim) and for `super::` chains that walk above this file's
    /// module scope.
    fn normalize_local_trait_path(target: &str, module_names: &[String]) -> Option<String> {
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

    fn owner_id(&self) -> String {
        self.impl_context.as_ref().map_or_else(
            || {
                self.owner_ids
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.file_id.to_owned())
            },
            |impl_context| impl_context.id.clone(),
        )
    }

    fn owner_name(&self) -> String {
        self.impl_context.as_ref().map_or_else(
            || {
                if self.module_names.is_empty() {
                    self.file.repo_relative_path.clone()
                } else {
                    self.module_names.join("::")
                }
            },
            |impl_context| impl_context.display.clone(),
        )
    }

    fn qualify(&self, local_name: &str) -> String {
        if self.module_names.is_empty() {
            local_name.to_owned()
        } else {
            format!("{}::{local_name}", self.module_names.join("::"))
        }
    }

    fn node_text(&self, node: Node<'_>) -> &'source str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }

    fn is_test_function(&self, node: Node<'_>) -> bool {
        self.node_text(node).contains("#[test]")
            || node.prev_named_sibling().is_some_and(|sibling| {
                sibling.kind() == "attribute_item" && self.node_text(sibling).contains("test")
            })
    }
}

/// The closed debt-marker vocabulary for issue #218, keyed by the lowercase
/// machine-readable category. Matching is case-insensitive on the marker
/// token only and is closed for this slice — no user-configurable
/// vocabularies.
const DEBT_MARKER_CATEGORIES: [&str; 4] = ["fixme", "hack", "todo", "xxx"];

/// One detected debt-marker occurrence inside a comment node's text.
#[derive(Debug, Clone, Eq, PartialEq)]
struct CommentMarker {
    /// Closed lowercase category: `todo` / `fixme` / `hack` / `xxx`.
    category: &'static str,
    /// Trimmed single-line note text following the marker token.
    note: String,
    /// Byte offset of the marker token within the comment text.
    token_start: usize,
    /// Byte offset just past the trimmed single-line note within the comment
    /// text (always past the marker token itself).
    note_end: usize,
    /// Number of newlines in the comment text before the marker token.
    line_offset: usize,
}

/// Scans one comment node's text for debt-marker tokens (issue #218).
///
/// The scan is word-boundary conservative: a candidate token is a maximal
/// ASCII `[A-Za-z0-9_]` run, so `TODOIST`, `fixmeup`, `XXXL`, and `TODO2`
/// never match. Matching against the closed [`DEBT_MARKER_CATEGORIES`] set is
/// case-insensitive on the token only. The note is the text following the
/// marker to the end of its line (or the end of the comment), with a trailing
/// `*/` block terminator removed, one leading `:` or `-` separator dropped,
/// and surrounding whitespace trimmed. One marker is returned per token
/// occurrence, in source order.
fn comment_debt_markers(text: &str) -> Vec<CommentMarker> {
    let bytes = text.as_bytes();
    let mut markers = Vec::new();
    let mut newlines = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'\n' {
            newlines += 1;
            i += 1;
            continue;
        }
        if !byte.is_ascii_alphanumeric() && byte != b'_' {
            i += 1;
            continue;
        }
        let token_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let token = &text[token_start..i];
        let Some(category) = DEBT_MARKER_CATEGORIES
            .iter()
            .find(|category| token.eq_ignore_ascii_case(category))
            .copied()
        else {
            continue;
        };
        let line_end = text[i..].find('\n').map_or(text.len(), |offset| i + offset);
        let raw_note = text[i..line_end].trim_end();
        let without_terminator = raw_note.strip_suffix("*/").unwrap_or(raw_note).trim_end();
        let note_end = i + without_terminator.len();
        let cleaned = without_terminator.trim_start();
        let cleaned = cleaned
            .strip_prefix(':')
            .or_else(|| cleaned.strip_prefix('-'))
            .unwrap_or(cleaned);
        markers.push(CommentMarker {
            category,
            note: cleaned.trim().to_owned(),
            token_start,
            note_end,
            line_offset: newlines,
        });
    }
    markers
}

/// Returns `true` when `symbol_kind` belongs to the issue #124 declaration-
/// surface set: the extracted Rust item kinds that carry `visibility` and
/// `signature` fields (`impl` blocks are excluded — they have no visibility
/// modifier and no declaration contract of their own).
fn carries_declaration_surface(symbol_kind: &str) -> bool {
    matches!(
        symbol_kind,
        "function"
            | "method"
            | "test"
            | "struct"
            | "enum"
            | "trait"
            | "type_alias"
            | "const"
            | "static"
    )
}

/// `true` when the item carries an `unsafe` keyword token in its modifier
/// position (issue #222): an `unsafe` token inside a `function_modifiers`
/// child for callables, or a direct `unsafe` token child for `impl_item`.
///
/// Only direct children (and the modifier list's direct children) are
/// inspected, so `unsafe` constructs nested inside an item's body never mark
/// the item itself.
fn has_unsafe_modifier(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "unsafe" => return true,
            "function_modifiers" => {
                let mut inner = child.walk();
                if child.children(&mut inner).any(|m| m.kind() == "unsafe") {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// True when the nearest enclosing item of a `function_signature_item` is a
/// `trait_item` (mint a Symbol per issue #342), false when it is a
/// `foreign_mod_item` (`extern` block — symbol-less, out of scope). Both wrap
/// the signature in a `declaration_list`; the classification walks the
/// ancestor chain to the first of the two item kinds, so it is robust to
/// grammar nesting.
fn signature_is_trait_method(node: Node<'_>) -> bool {
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        match current.kind() {
            "trait_item" => return true,
            "foreign_mod_item" => return false,
            _ => ancestor = current.parent(),
        }
    }
    false
}

/// Returns the item's `visibility_modifier` child, if any.
fn visibility_modifier(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .find(|child| child.kind() == "visibility_modifier")
}

/// Extracts doc text from one comment node's source text.
///
/// Returns `Some` for rustdoc outer doc comments — `///` line docs (but not
/// `////`) and `/** */` block docs (but not `/***` or the empty `/**/`) — and
/// `None` for every other comment shape.
fn doc_comment_text(text: &str) -> Option<String> {
    let text = text.trim_end();
    if let Some(rest) = text.strip_prefix("///") {
        if rest.starts_with('/') {
            return None;
        }
        return Some(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
    }
    if let Some(rest) = text.strip_prefix("/**") {
        if rest.starts_with('*') || rest == "/" {
            return None;
        }
        let inner = rest.strip_suffix("*/").unwrap_or(rest);
        return Some(block_doc_text(inner));
    }
    None
}

/// Extracts doc text from one `#[doc = ...]` attribute item's source text.
///
/// Returns the decoded text for outer doc attributes carrying a plain or raw
/// string literal. A non-literal value (`#[doc = include_str!(...)]`,
/// `#[doc = concat!(...)]`) still documents the item for rustdoc, so it
/// yields a labeled marker citing the unexpanded expression — presence is
/// recorded, text is never guessed by expanding macros. Returns `None` for
/// every other attribute shape — `#[doc(hidden)]`, `#[doc(alias = "...")]`,
/// and non-`doc` attributes contribute no doc text.
fn doc_attribute_text(text: &str) -> Option<String> {
    let inner = text
        .trim()
        .strip_prefix("#[")?
        .strip_suffix(']')?
        .trim()
        .strip_prefix("doc")?
        .trim_start()
        .strip_prefix('=')?
        .trim();
    if let Some(literal) = string_literal_text(inner) {
        return Some(literal);
    }
    if inner.is_empty() {
        return None;
    }
    Some(format!("[unexpanded doc attribute: {inner}]"))
}

/// Decodes a Rust string literal (`"..."`, `r"..."`, `r#"..."#`, ...) into
/// its text. Raw literals are taken verbatim; plain literals are unescaped
/// via [`unescape_string_literal`]. Returns `None` for anything that is not
/// a single string literal.
fn string_literal_text(literal: &str) -> Option<String> {
    if let Some(raw) = literal.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let quoted = raw.get(hashes..raw.len().checked_sub(hashes)?)?;
        return Some(quoted.strip_prefix('"')?.strip_suffix('"')?.to_owned());
    }
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    Some(unescape_string_literal(inner))
}

/// Unescapes the interior of a plain Rust string literal: `\n`, `\t`, `\r`,
/// `\0`, `\\`, `\'`, `\"`, `\xNN`, `\u{...}`, and the `\`-newline line
/// continuation (which also swallows the next line's leading whitespace).
///
/// Any escape this decoder cannot decode returns the interior **verbatim**
/// — the recorded text is kept raw rather than partially decoded (a dropped
/// backslash would corrupt the doc fact).
fn unescape_string_literal(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\\') => out.push('\\'),
            Some('\'') => out.push('\''),
            Some('"') => out.push('"'),
            Some('x') => {
                let hex: String = (0..2).filter_map(|_| chars.next()).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(byte) => out.push(char::from(byte)),
                    Err(_) => return inner.to_owned(),
                }
            }
            Some('u') => {
                if chars.next() != Some('{') {
                    return inner.to_owned();
                }
                let mut hex = String::new();
                loop {
                    match chars.next() {
                        Some('}') => break,
                        Some(digit) => hex.push(digit),
                        None => return inner.to_owned(),
                    }
                }
                match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    Some(decoded) => out.push(decoded),
                    None => return inner.to_owned(),
                }
            }
            // Line continuation: `\` before a newline (or CRLF) removes the
            // break and the following leading whitespace.
            Some('\n') => {
                while chars
                    .peek()
                    .is_some_and(|w| matches!(w, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            Some('\r') if chars.peek() == Some(&'\n') => {
                while chars
                    .peek()
                    .is_some_and(|w| matches!(w, ' ' | '\t' | '\n' | '\r'))
                {
                    chars.next();
                }
            }
            // Unknown escape or trailing backslash: keep the text raw.
            _ => return inner.to_owned(),
        }
    }
    out
}

/// Normalizes the interior of a `/** */` block doc: strips the per-line
/// leading `*` gutter and one following space, trims line ends, and drops
/// leading/trailing blank lines.
fn block_doc_text(inner: &str) -> String {
    let lines: Vec<&str> = inner
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix('*')
                .map_or(trimmed, |rest| rest.strip_prefix(' ').unwrap_or(rest))
        })
        .collect();
    lines.join("\n").trim().to_owned()
}

fn import_name(text: &str) -> String {
    text.trim()
        .trim_start_matches("use")
        .trim()
        .trim_end_matches(';')
        .trim()
        .to_owned()
}

/// Returns `true` when a symbol of this kind can be the target of an
/// `IMPLEMENTS` edge: traits and type-defining items. Value-namespace items
/// (`const`, `static`) and callables never qualify.
///
/// Shared with the cross-file IMPLEMENTS resolver so its ambiguity guard and
/// this extractor's target-kind gate never diverge a target-kind at a time.
pub(crate) fn is_impl_target_kind(symbol_kind: &str) -> bool {
    matches!(symbol_kind, "trait" | "struct" | "enum" | "type_alias")
}

/// Reports whether more than one distinct qualified name in `qualified_names`
/// ends in the bare simple name `simple`.
///
/// This is the single COUNTING predicate behind the same-name ambiguity bound
/// on BOTH IMPLEMENTS resolution paths — the local per-file resolver
/// ([`RustExtractor::resolve_impl_trait_locally`]) and the repo-wide cross-file
/// pass (`ImplTargetIndex::bare_simple_name_is_ambiguous`) — so neither guard
/// can drift a target-kind or a counting rule from the other one entry point at
/// a time. Callers pass only impl-target-kind qualified names
/// ([`is_impl_target_kind`]), so a value-namespace collision never triggers it.
///
/// A bare (unqualified) reference whose simple name is ambiguous cannot be
/// disambiguated without import-aware (`use`-decl) resolution, which is outside
/// this slice's documented `local_traits_only` bound (follow-up #393): when it
/// would otherwise bind to a same-named ROOT/outer definition that a `use`
/// alias could actually redirect elsewhere, the caller leaves it unresolved
/// rather than mint a WRONG-target edge.
pub(crate) fn bare_simple_name_is_ambiguous<'a>(
    simple: &str,
    qualified_names: impl IntoIterator<Item = &'a str>,
) -> bool {
    let mut matches = 0usize;
    for qualified in qualified_names {
        let last = qualified.rsplit("::").next().unwrap_or(qualified);
        if last == simple {
            matches += 1;
            if matches > 1 {
                return true;
            }
        }
    }
    false
}

/// Collects the simple names a `use` declaration binds into its enclosing
/// scope, walking the Tree-sitter parse tree (never a text parse of the
/// display string). Handles the plain (`use a::b::T;` → `T`), alias
/// (`use a::b::T as U;` → the bound alias `U`), and grouped
/// (`use a::{B, C::D};` → `B`, `D`, including nested groups) forms. A glob
/// import (`use a::*;`) binds no specific simple name, so it contributes
/// nothing and never vetoes a bare impl-target name (issues #343/#344 round 9).
fn use_bound_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    if let Some(argument) = node.child_by_field_name("argument") {
        collect_use_tree_names(argument, source, &mut names);
    }
    names
}

/// Recursive worker for [`use_bound_names`], appending each bound simple name
/// reachable from a use-tree node. Only the node kinds that introduce a
/// nameable binding contribute; `use_wildcard` (glob) and separator tokens
/// fall through and add nothing.
fn collect_use_tree_names(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "type_identifier" => {
            out.push(node_source(node, source).trim().to_owned());
        }
        "scoped_identifier" => {
            // The bound name is the final `name` segment (`a::b::T` → `T`).
            if let Some(name) = node.child_by_field_name("name")
                && matches!(name.kind(), "identifier" | "type_identifier")
            {
                out.push(node_source(name, source).trim().to_owned());
            }
        }
        "use_as_clause" => {
            // `path as alias` binds the alias, not the path's final segment.
            if let Some(alias) = node.child_by_field_name("alias")
                && matches!(alias.kind(), "identifier" | "type_identifier")
            {
                out.push(node_source(alias, source).trim().to_owned());
            }
        }
        "scoped_use_list" => {
            if let Some(list) = node.child_by_field_name("list") {
                collect_use_tree_names(list, source, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_tree_names(child, source, out);
            }
        }
        // `use_wildcard` (glob) names no specific simple name; separator tokens
        // (`{`, `,`, `}`) carry none either.
        _ => {}
    }
}

/// Collects the `(bound simple name, import path as written)` pairs a `use`
/// declaration introduces into its enclosing scope, walking the Tree-sitter
/// parse tree (issue #393). The import PATH is what the cross-file resolver
/// resolves so a bare imported trait/type name binds its true target
/// (`use crate::a::T;` → `("T", "crate::a::T")`). Handles the plain, alias
/// (`use a::b::T as U;` → `("U", "a::b::T")`), grouped (`use a::{B, C::D};` →
/// `("B", "a::B")`, `("D", "a::C::D")`, including nested groups), and glob
/// (`use a::*;` binds no simple name, contributes nothing) forms — the same
/// surface [`use_bound_names`] covers, kept in lock-step so the veto's bound
/// names and the resolver's import paths never disagree.
fn use_bound_import_paths(node: Node<'_>, source: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(argument) = node.child_by_field_name("argument") {
        collect_use_tree_paths(argument, source, "", &mut out);
    }
    out
}

/// Joins a use-tree prefix with a trailing segment path (`::`-separated),
/// dropping an empty prefix.
fn join_use_path(prefix: &str, tail: &str) -> String {
    if prefix.is_empty() {
        tail.to_owned()
    } else {
        format!("{prefix}::{tail}")
    }
}

/// Recursive worker for [`use_bound_import_paths`], accumulating the path prefix
/// as it descends group nodes. Mirrors [`collect_use_tree_names`] node-for-node
/// so the bound-name set stays identical.
fn collect_use_tree_paths(
    node: Node<'_>,
    source: &str,
    prefix: &str,
    out: &mut Vec<(String, String)>,
) {
    match node.kind() {
        "identifier" | "type_identifier" => {
            let name = node_source(node, source).trim().to_owned();
            let full = join_use_path(prefix, &name);
            out.push((name, full));
        }
        "scoped_identifier" => {
            // `path::name`: the whole scoped identifier is the import path; its
            // final `name` segment is the bound simple name.
            if let Some(name) = node.child_by_field_name("name")
                && matches!(name.kind(), "identifier" | "type_identifier")
            {
                let simple = node_source(name, source).trim().to_owned();
                let written = node_source(node, source).trim().to_owned();
                let full = join_use_path(prefix, &written);
                out.push((simple, full));
            }
        }
        "use_as_clause" => {
            // `path as alias` binds the alias to the path (not its final
            // segment): `use a::b::T as U;` → `("U", "a::b::T")`.
            if let Some(alias) = node.child_by_field_name("alias")
                && matches!(alias.kind(), "identifier" | "type_identifier")
                && let Some(path) = node.child_by_field_name("path")
            {
                let simple = node_source(alias, source).trim().to_owned();
                let written = node_source(path, source).trim().to_owned();
                let full = join_use_path(prefix, &written);
                out.push((simple, full));
            }
        }
        "scoped_use_list" => {
            let new_prefix = node.child_by_field_name("path").map_or_else(
                || prefix.to_owned(),
                |path| join_use_path(prefix, node_source(path, source).trim()),
            );
            if let Some(list) = node.child_by_field_name("list") {
                collect_use_tree_paths(list, source, &new_prefix, out);
            }
        }
        "use_list" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_use_tree_paths(child, source, prefix, out);
            }
        }
        // `use_wildcard` (glob) and separator tokens name nothing.
        _ => {}
    }
}

/// Reports whether a `use_declaration` is a MODULE ITEM — a direct child of the
/// `source_file` root or of a `mod_item`'s `declaration_list` body — as opposed
/// to a `use` nested inside a function body, `block`, or expression. Only a
/// module-item `use` is visible to module-level impls, so only it feeds the
/// import-shadow veto index (round-10 Codex finding: a block-local
/// `fn helper() { use std::fmt::Display; }` must never shadow a module-level
/// `impl Display for Foo`). A block-local `use`'s parent is a `block`; a
/// module-item `use`'s parent is `source_file` or `declaration_list`.
fn is_module_item_use(node: Node<'_>) -> bool {
    node.parent()
        .is_some_and(|parent| matches!(parent.kind(), "source_file" | "declaration_list"))
}

/// Reports whether a module-item `use` import in the impl's OWN module scope
/// binds the bare simple name `bare`. Rust's `use` visibility is NOT inherited
/// by child modules: an import declared in an ancestor/root scope cannot shadow
/// a bare name inside `mod m`, so the veto consults ONLY the impl's exact own
/// module-scope key — never prefix/ancestor scopes (round-10 Codex finding). A
/// block-local `use` never reaches this index at all (see [`is_module_item_use`]).
/// This is the SINGLE shared predicate behind the import-shadow veto on BOTH
/// IMPLEMENTS resolution paths: the local per-file resolver consults it directly,
/// and the deferred cross-file pass reads the same answer off the
/// `PendingImplFact::shadowed_by_use` boolean this predicate sets at extraction
/// time — so the two paths can never diverge (issues #343/#344; correct
/// import-aware resolution is follow-up #393).
fn scope_imports_bare_name(
    imports_by_scope: &BTreeMap<Vec<String>, BTreeSet<String>>,
    module_names: &[String],
    bare: &str,
) -> bool {
    imports_by_scope
        .get(module_names)
        .is_some_and(|names| names.contains(bare))
}

/// The IMPLEMENTS-resolution decision for one impl display header
/// (issue #343).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ImplTargetDecision {
    /// Resolve this bare trait/type name through the trait-scope walk. Generic
    /// binders and trait-segment generic args are already stripped.
    Resolve(String),
    /// Preserve the legacy verbatim self-referential edge — a generic inherent
    /// impl (`impl<T> Type<T>`) with no `for` clause names no trait.
    Verbatim,
    /// Mint no edge: a blanket impl (`impl<T> Trait for T`) whose `for` target
    /// is a bare binder type parameter.
    NoEdge,
}

/// Reads a node's source text without borrowing an extractor, for the
/// AST-driven impl-decision helpers below.
fn node_source<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

/// Derives the IMPLEMENTS-resolution decision for an `impl_item` directly from
/// its Tree-sitter fields (issue #343/#344), never by string-scanning the
/// header. `impl_item` exposes three named fields: `type_parameters` (the
/// `<...>` binder, present only on generic impls), `trait` (the implemented
/// trait, present only on trait impls), and `type` (the implementing type).
/// Reading them structurally means a return arrow in a binder bound
/// (`impl<T: Fn() -> u32> Target for Wrapper<T>`), a spaced binder, or a
/// reference/pointer blanket target can never leak across the trait/`for` split
/// the way a character scan can.
///
/// - No `trait` field: an inherent impl. A generic inherent impl
///   (`impl<T> Type<T>`) keeps its verbatim self-referential edge; a
///   non-generic inherent impl resolves its type name verbatim (pre-#343
///   behavior).
/// - A `trait` field: resolve the bare trait path (generic args stripped via
///   the AST), unless the implementing type is a blanket bare binder parameter,
///   a reference/pointer to one, or a non-nominal type (slice, array, tuple,
///   trait object, `impl Trait`) — those mint no edge.
///
/// Turbofish trait syntax (`impl GenP::<u32> for Plain`) is not valid Rust in
/// trait position; Tree-sitter cannot represent it and emits an `ERROR` node
/// that swallows the trait/`for` split, so there is no reliable AST to read.
/// That single pathological-but-supported form is recovered through the
/// header-string normalizer ([`impl_trait_target`]), keeping its output
/// byte-identical to the pre-refactor path.
fn impl_target_decision(node: Node<'_>, source: &str, display: &str) -> ImplTargetDecision {
    let trait_field = node.child_by_field_name("trait");
    let type_field = node.child_by_field_name("type");

    // Turbofish `::<>` in trait position produces an `ERROR` node under the
    // implementing-type field with no `trait` field: fall back to the
    // header-string normalizer for that invalid-Rust-but-supported form. A
    // `type` field that is entirely absent is likewise unreadable.
    match type_field {
        Some(type_node) if trait_field.is_none() && type_node.has_error() => {
            return impl_trait_target(display);
        }
        None => return impl_trait_target(display),
        _ => {}
    }

    let type_parameters = node.child_by_field_name("type_parameters");
    let binder_params = type_parameters
        .map(|params| binder_type_params(params, source))
        .unwrap_or_default();

    let Some(trait_node) = trait_field else {
        // Inherent impl (no `for` clause). A generic inherent impl names no
        // trait to reach: keep the recorded self-referential edge. A
        // non-generic inherent impl resolves its type name verbatim.
        return if type_parameters.is_some() {
            ImplTargetDecision::Verbatim
        } else {
            type_field.map_or(ImplTargetDecision::Verbatim, |type_node| {
                ImplTargetDecision::Resolve(node_source(type_node, source).trim().to_owned())
            })
        };
    };

    // A negative impl (`impl !Trait for Foo`) asserts that the type explicitly
    // does NOT implement the trait. Tree-sitter keeps the `!` as an unnamed
    // child token BEFORE the `trait` field (the field itself reads as the bare
    // trait name), so reading the trait field alone would resolve it like a
    // positive impl and mint a wrong IMPLEMENTS edge. Detect the `!` and mint no
    // edge — a negative impl never implements the trait it names.
    if is_negative_impl(node) {
        return ImplTargetDecision::NoEdge;
    }

    let bare_trait = bare_trait_path(trait_node, source);
    if bare_trait.is_empty() {
        return ImplTargetDecision::NoEdge;
    }
    // A blanket impl (`for T`, or `for &T` / `*const T` around a bare binder
    // parameter) or a non-nominal `for` target (slice, array, tuple, trait
    // object, `impl Trait`) has no single concrete implementing-type record:
    // mint no edge rather than fabricate one.
    if type_field.is_some_and(|type_node| !is_nominal_target(type_node, source, &binder_params)) {
        return ImplTargetDecision::NoEdge;
    }
    ImplTargetDecision::Resolve(bare_trait)
}

/// `true` when an `impl_item` is a negative impl (`impl !Trait for Foo`).
/// Tree-sitter parses the leading `!` as an unnamed `!` child token sitting
/// between the `impl` keyword and the `trait` field, so the field itself carries
/// only the bare trait name. Scanning the impl node's direct children for that
/// `!` token is the reliable AST signal; the string header never has to be
/// consulted.
fn is_negative_impl(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|child| child.kind() == "!")
}

/// Collects the bare type-parameter identifiers declared by a `type_parameters`
/// binder node, reading each parameter's `name` field from the AST. Only
/// `type_parameter` names (a `type_identifier`) are kept: lifetime parameters
/// (`'a`) and const parameters (`const N`) carry no type identifier that could
/// appear as a bare `for` target, so they are skipped. Bounds (`T: Fn() ->
/// u32`) live in a sibling `bounds` field and never reach the name, so a return
/// arrow in a bound cannot poison the set.
fn binder_type_params(type_parameters: Node<'_>, source: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut cursor = type_parameters.walk();
    for child in type_parameters.named_children(&mut cursor) {
        if let Some(name) = child.child_by_field_name("name")
            && name.kind() == "type_identifier"
        {
            params.push(node_source(name, source).to_owned());
        }
    }
    params
}

/// Extracts the bare trait path from an `impl_item` `trait` field node,
/// dropping generic arguments via the AST (never string `<>`/`::` work):
/// `type_identifier` -> its text (`Trait`); `generic_type` -> its base `type`
/// child recursively, so `GenP<u32>` -> `GenP` and `crate::Target<u32>` ->
/// `crate::Target`; `scoped_type_identifier` -> its full path text (`crate::T`
/// / `super::T` / `some::path::GenP`), preserved exactly for cross-file
/// resolution (issue #344).
fn bare_trait_path(trait_node: Node<'_>, source: &str) -> String {
    if trait_node.kind() == "generic_type"
        && let Some(base) = trait_node.child_by_field_name("type")
    {
        return bare_trait_path(base, source);
    }
    node_source(trait_node, source).trim().to_owned()
}

/// `true` when a trait impl's implementing-`type` field names a single concrete
/// nominal type that can carry an IMPLEMENTS edge. Reference and pointer
/// wrappers are peeled first (`&T` / `&mut T` / `&'a T` / `*const T` / `&&T` /
/// `&[T]`), then the core is classified: a bare `type_identifier` that is one
/// of the binder's type parameters is a blanket target (`impl<T> Trait for T`);
/// a slice/array (`[T]` / `[T; N]`), tuple (`(T, U)`), trait object (`dyn
/// Foo`), `impl Trait` opaque type, or unit type is non-nominal. Both cases
/// lack a concrete implementing-type record and return `false` (mint no edge).
/// Everything else — a concrete nominal type (`Wrapper<T>`, `crate::Thing`,
/// `&Wrapper<T>`) — is nominal.
fn is_nominal_target(type_node: Node<'_>, source: &str, binder_params: &[String]) -> bool {
    let mut core = type_node;
    while matches!(core.kind(), "reference_type" | "pointer_type") {
        let Some(inner) = core.child_by_field_name("type") else {
            break;
        };
        core = inner;
    }
    match core.kind() {
        "type_identifier" => !binder_params
            .iter()
            .any(|param| param == node_source(core, source).trim()),
        "array_type" | "tuple_type" | "dynamic_type" | "abstract_type" | "unit_type" => false,
        _ => true,
    }
}

/// Parses an impl display header (`impl ...`, `impl<T> ...`, or an
/// `unsafe `-prefixed form) into its IMPLEMENTS-resolution decision.
///
/// Trait impls (headers with a ` for ` clause) resolve their trait segment
/// with any generic binder and trait-segment generic args stripped, so
/// `impl<T> GenT for Wrapper<T>` -> `GenT` and `impl GenP<u32> for Plain` ->
/// `GenP`. A blanket impl whose `for` target is a bare binder type parameter
/// (`impl<T> Trait for T`) is bounded out with no edge. Inherent impls (no
/// ` for ` clause) preserve the pre-#343 behavior: a non-generic inherent impl
/// resolves its type name verbatim, and a generic inherent impl keeps its
/// recorded self-referential edge.
fn impl_trait_target(display: &str) -> ImplTargetDecision {
    let header = display.strip_prefix("unsafe ").unwrap_or(display).trim();
    let Some(after_impl) = header.strip_prefix("impl") else {
        return ImplTargetDecision::Verbatim;
    };
    // `impl` must be followed by a space or `<` to open a real header; an
    // identifier that merely starts with `impl` (`implement_service`) is not.
    match after_impl.chars().next() {
        Some(' ' | '<') => {}
        _ => return ImplTargetDecision::Verbatim,
    }
    // Skip an optional `<...>` binder whether or not a space precedes it: a
    // source-level space (`impl <T> Trait for T`) survives `impl_display` as
    // `impl <T> ...`, so the binder must be stripped after trimming the space,
    // not left to poison the trait segment.
    let trimmed = after_impl.trim_start();
    let (binder_params, remainder) = if trimmed.starts_with('<') {
        let (params, rest) = split_generic_binder(trimmed);
        (params, rest.trim_start())
    } else {
        (Vec::new(), trimmed)
    };
    match remainder.split_once(" for ") {
        Some((trait_seg, for_target)) => {
            let bare_trait = strip_trait_generics(trait_seg.trim());
            if bare_trait.is_empty() {
                return ImplTargetDecision::NoEdge;
            }
            // Blanket impl: the `for` target reduces to a bare binder type
            // parameter, even through reference/pointer sigils and lifetimes
            // (`impl<T> Trait for &T` / `&mut T` / `&'a T` / `*const T` are as
            // blanket as `impl<T> Trait for T`), or it is a non-nominal type
            // (slice, tuple, trait object) that names no single local type.
            // Either way there is no concrete implementing-type record, so mint
            // no edge rather than fabricate one.
            let for_core = for_target_core(for_target);
            if binder_params.iter().any(|p| p == for_core) || is_non_nominal_target(for_core) {
                return ImplTargetDecision::NoEdge;
            }
            ImplTargetDecision::Resolve(bare_trait.to_owned())
        }
        None if binder_params.is_empty() => {
            // Non-generic inherent impl: resolve the type name verbatim (no
            // generic stripping), matching the pre-#343 path exactly.
            ImplTargetDecision::Resolve(remainder.trim().to_owned())
        }
        None => ImplTargetDecision::Verbatim,
    }
}

/// Splits a `<...>` generic binder at the front of `s` (which must start with
/// `<`), returning its top-level type-parameter identifiers and the text after
/// the balanced binder. An unbalanced binder yields no params and empty rest.
fn split_generic_binder(s: &str) -> (Vec<String>, &str) {
    let mut depth = 0usize;
    let mut end = None;
    for (i, c) in s.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return (Vec::new(), "");
    };
    (parse_binder_params(&s[1..end]), &s[end + 1..])
}

/// Parses the top-level type-parameter identifiers from a binder's inner text
/// (`T: Into<String>, U, const N: usize, 'a` -> `[T, U, N]`). Lifetimes carry
/// no type identifier and are skipped; `const` and bound clauses are dropped.
fn parse_binder_params(inner: &str) -> Vec<String> {
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut segments = Vec::new();
    for (i, c) in inner.char_indices() {
        match c {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                segments.push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    segments.push(&inner[start..]);
    let mut params = Vec::new();
    for seg in segments {
        let seg = seg.trim();
        if seg.is_empty() || seg.starts_with('\'') {
            continue;
        }
        let seg = seg.strip_prefix("const ").map_or(seg, str::trim_start);
        let ident_end = seg
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(seg.len());
        let ident = &seg[..ident_end];
        if is_simple_ident(ident) {
            params.push(ident.to_owned());
        }
    }
    params
}

/// Strips generic args from an impl trait segment: `GenP<u32>` -> `GenP`,
/// `foo::Bar<T>` -> `foo::Bar`, `Plain` -> `Plain`.
///
/// Turbofish trait syntax (`GenP::<u32>`, valid Rust in type position) leaves a
/// trailing `::` separator once the `<...>` args are removed; that trailing
/// separator is stripped too so the bare name matches the trait symbol
/// (`GenP::<u32>` -> `GenP`, `some::path::GenP::<u32>` -> `some::path::GenP`).
/// Only a TRAILING `::` is removed — internal path separators are preserved, so
/// a non-turbofish qualified path (`crate::T`) is unchanged.
fn strip_trait_generics(trait_seg: &str) -> &str {
    let base = trait_seg.split('<').next().unwrap_or(trait_seg).trim();
    base.strip_suffix("::").map_or(base, str::trim_end)
}

/// Reduces a trait impl's `for` target to the core type text used for the
/// blanket / non-nominal bound-out check. Drops a trailing where clause, then
/// repeatedly strips leading reference and pointer sigils with their optional
/// lifetimes and `mut` (`&`, `&mut`, `&'a`, `&'a mut`, `*const`, `*mut`, in any
/// combination), returning the innermost wrapped type text. A bare binder param
/// stays itself (`T` -> `T`), a reference to one reduces to it (`&'a mut T` ->
/// `T`), and a concrete nominal target is preserved for the caller's resolve
/// path (`Wrapper<T>` -> `Wrapper<T>`, `&Wrapper<T>` -> `Wrapper<T>`).
fn for_target_core(for_target: &str) -> &str {
    let mut core = for_target
        .split_once(" where ")
        .map_or(for_target, |(lhs, _)| lhs)
        .trim();
    loop {
        let before = core;
        if let Some(rest) = core.strip_prefix('&') {
            let rest = rest.trim_start();
            // Optional lifetime (`'a`), then optional `mut`.
            let rest = rest.strip_prefix('\'').map_or(rest, |after_tick| {
                let end = after_tick
                    .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .unwrap_or(after_tick.len());
                after_tick[end..].trim_start()
            });
            core = rest.strip_prefix("mut ").map_or(rest, str::trim_start);
        } else if let Some(rest) = core.strip_prefix("*const ") {
            core = rest.trim_start();
        } else if let Some(rest) = core.strip_prefix("*mut ") {
            core = rest.trim_start();
        }
        if core == before {
            break;
        }
    }
    core
}

/// `true` when a trait impl's sigil-stripped `for` target core cannot name a
/// single local nominal type: a slice/array (`[T]`), a tuple (`(T, U)`), or a
/// trait object / opaque type (`dyn Foo`, `impl Foo`). Such targets have no
/// concrete implementing-type record, so they mint no IMPLEMENTS edge.
fn is_non_nominal_target(core: &str) -> bool {
    core.starts_with('[')
        || core.starts_with('(')
        || core == "dyn"
        || core == "impl"
        || core.starts_with("dyn ")
        || core.starts_with("impl ")
}

fn impl_display(text: &str) -> String {
    text.split('{')
        .next()
        .unwrap_or(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn method_owner(display: &str) -> String {
    display
        .strip_prefix("impl ")
        .unwrap_or(display)
        .trim()
        .to_owned()
}

/// Normalizes an impl owner display to the implementing type's simple name
/// for cross-file call matching (issue #152).
///
/// `Runner for Widget` → `Widget`; `impl<T> MyStruct<T>` → `MyStruct`;
/// `crate::foo::Bar` → `Bar`. Returns `None` for owner forms that do not
/// reduce to a simple identifier (references, trait objects, tuples).
fn normalize_impl_owner(owner: &str) -> Option<String> {
    let owner = owner.rsplit(" for ").next()?.trim();
    // A leftover `impl` / `impl<T>` prefix survives `method_owner` when the
    // impl has generic parameters; strip it before reading the type.
    let owner = strip_impl_prefix(owner).trim();
    let owner = owner.split('<').next()?.trim();
    let owner = owner.rsplit("::").next()?.trim();
    (is_simple_ident(owner)).then(|| owner.to_owned())
}

/// Normalizes an impl owner display to the implementing type's PATH AS WRITTEN,
/// stopping BEFORE [`normalize_impl_owner`]'s final leaf reduction (issue #414,
/// Codex P2 on #420): `T for std::string::String` → `std::string::String`;
/// `U for String` → `String`; `Runner for crate::foo::Bar` → `crate::foo::Bar`;
/// `impl<T> Trait for Wrapper<T>` → `Wrapper`. The retained path lets the
/// repo-wide `ImplTargetIndex` decide whether the implementing type resolves to
/// a UNIQUE LOCAL type def — an external `std::string::String` resolves to
/// nothing and its relation is dropped, so it never pollutes a local `String`'s
/// implemented-trait set under the bare-leaf map key. Returns `None` for empty
/// forms.
fn normalize_impl_owner_path(owner: &str) -> Option<String> {
    let owner = owner.rsplit(" for ").next()?.trim();
    let owner = strip_impl_prefix(owner).trim();
    let owner = owner.split('<').next()?.trim();
    (!owner.is_empty()).then(|| owner.to_owned())
}

/// Reduces a type AST node to a matchable nominal type path for the #441
/// receiver-type environment, or `None` when it does not name a plain nominal
/// type. Reference/pointer wrappers (`&T`, `&mut T`, `&'a T`, `*const T`) are
/// peeled, and a single-level generic (`Wrapper<T>`) reduces to its base type;
/// a `type_identifier` yields its text and a `scoped_type_identifier` its full
/// path text. A trait object (`dyn Device`), `impl Trait`, slice, array, tuple,
/// unit, or any other non-nominal form yields `None`, so trait-typed and
/// non-nominal receivers never enter the environment.
fn reduce_receiver_type(type_node: Node<'_>, source: &str) -> Option<String> {
    let mut core = type_node;
    loop {
        match core.kind() {
            // Peel reference/pointer wrappers and reduce a single-level generic
            // to its base `type` child (all three expose the core via the same
            // `type` field).
            "reference_type" | "pointer_type" | "generic_type" => {
                core = core.child_by_field_name("type")?;
            }
            "type_identifier" | "scoped_type_identifier" => {
                let text = node_source(core, source).trim();
                return (!text.is_empty()).then(|| text.to_owned());
            }
            _ => return None,
        }
    }
}

/// Reduces a type ascription to its dispatch-trait path (issue #267): returns
/// `Some` when the ascribed type is provably a trait object or a single-bound
/// generic wrapper around one — the trait path as written in source. Peels
/// reference/pointer wrappers (`&dyn Trait`, `&mut dyn Trait`, `*const dyn
/// Trait`) and recurses into the type arguments of the std Deref smart
/// pointers `Box`/`Rc`/`Arc` (`Box<dyn Trait>`), because method calls on those
/// always deref to the trait object (inherent methods on these foreign types
/// are impossible, so no static method set can be shadowed). Any other shape
/// — nominal types, other generic wrappers, multi-trait `dyn A + B` — yields
/// `None`: a MISSING dispatch hint beats a WRONG one.
fn reduce_dispatch_trait(type_node: Node<'_>, source: &str) -> Option<String> {
    let mut core = type_node;
    loop {
        match core.kind() {
            "reference_type" | "pointer_type" => {
                core = core.child_by_field_name("type")?;
            }
            "generic_type" => {
                let base = core.child_by_field_name("type")?;
                let base_leaf = node_source(base, source)
                    .trim()
                    .rsplit("::")
                    .next()
                    .unwrap_or_default();
                if !matches!(base_leaf, "Box" | "Rc" | "Arc") {
                    return None;
                }
                let args = core.child_by_field_name("type_arguments")?;
                let mut cursor = args.walk();
                let mut found: Option<String> = None;
                for arg in args.named_children(&mut cursor) {
                    if let Some(trait_path) = reduce_dispatch_trait(arg, source) {
                        found = Some(trait_path);
                        break;
                    }
                }
                return found;
            }
            "dynamic_type" => {
                return dispatch_trait_of_dynamic(core, source);
            }
            _ => return None,
        }
    }
}

/// The trait path of a `dyn Trait` node (issue #267): the `trait` field's
/// as-written path for a simple `type_identifier`/`scoped_type_identifier`;
/// `None` for anything else (generic trait objects, `dyn A + B` shapes the
/// grammar does not expose as a plain trait path).
fn dispatch_trait_of_dynamic(dynamic_node: Node<'_>, source: &str) -> Option<String> {
    let trait_node = dynamic_node.child_by_field_name("trait")?;
    match trait_node.kind() {
        "type_identifier" | "scoped_type_identifier" => {
            let text = node_source(trait_node, source).trim();
            (!text.is_empty()).then(|| text.to_owned())
        }
        _ => None,
    }
}

/// The trait path when a `trait_bounds` node carries exactly one simple trait
/// path and no other trait bounds (issue #267); `None` otherwise. Lifetime
/// bounds (`'static`) are ignored — they don't affect method dispatch.
/// Multi-bound parameters refuse to guess: dispatch through one of several
/// bounds is not provable from syntax alone.
fn single_trait_bound_path(bounds: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = bounds.walk();
    let mut trait_paths: Vec<String> = Vec::new();
    for child in bounds.named_children(&mut cursor) {
        match child.kind() {
            "lifetime" => {}
            "type_identifier" | "scoped_type_identifier" => {
                let text = node_source(child, source).trim();
                if text.is_empty() {
                    return None;
                }
                trait_paths.push(text.to_owned());
            }
            "generic_type" => {
                // `T: Into<String>` — the dispatch trait is the base path.
                let base = child.child_by_field_name("type")?;
                if !matches!(base.kind(), "type_identifier" | "scoped_type_identifier") {
                    return None;
                }
                let text = node_source(base, source).trim();
                if text.is_empty() {
                    return None;
                }
                trait_paths.push(text.to_owned());
            }
            _ => return None,
        }
    }
    (trait_paths.len() == 1)
        .then(|| trait_paths.into_iter().next())
        .flatten()
}

/// Collects every binding identifier a pattern introduces (issue #441), for the
/// shadowing veto. Pushes `identifier` and `shorthand_field_identifier` nodes;
/// for `tuple_struct_pattern`/`struct_pattern` the constructor/type path (the
/// `type` field) is skipped so the enum/struct NAME is never miscounted as a
/// binder. Type names inside patterns are `type_identifier`/`field_identifier`
/// nodes (never `identifier`), so they are naturally excluded elsewhere.
fn collect_pattern_idents(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    match node.kind() {
        "identifier" | "shorthand_field_identifier" => {
            out.push(node_source(node, source).trim().to_owned());
        }
        "tuple_struct_pattern" | "struct_pattern" => {
            let type_field = node.child_by_field_name("type");
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                if Some(child) == type_field {
                    continue;
                }
                collect_pattern_idents(child, source, out);
            }
        }
        _ => {
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
            for child in children {
                collect_pattern_idents(child, source, out);
            }
        }
    }
}

/// Strips a leading `impl` keyword and its generic parameter list, if any.
fn strip_impl_prefix(owner: &str) -> &str {
    let Some(rest) = owner.strip_prefix("impl") else {
        return owner;
    };
    let rest = rest.trim_start();
    let Some(generics) = rest.strip_prefix('<') else {
        return rest;
    };
    let mut depth = 1usize;
    let mut end = generics.len();
    for (idx, c) in generics.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    end = idx + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    generics.get(end..).unwrap_or("")
}

/// True when `text` is a plain identifier (letters, digits, underscores).
fn is_simple_ident(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// `true` when a `struct_expression`'s body carries a `..base` functional-record
/// -update tail (a `base_field_initializer`), issue #443. Such a literal is NOT
/// the E0063-breakable exhaustive form: `..base` supplies any fields the literal
/// omits, so adding a required field does not break it. A literal with no base is
/// exhaustive and breaks when a required field is added.
fn struct_literal_has_base(node: Node<'_>) -> bool {
    let Some(body) = node.child_by_field_name("body") else {
        return false;
    };
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .any(|child| child.kind() == "base_field_initializer")
}

/// Decodes one `#[deprecated]` payload string literal: verbatim decode via
/// [`string_literal_text`], bounded to [`MAX_DEPRECATION_STRING_LEN`]
/// characters (a verbatim prefix — no truncation marker is synthesized),
/// then redaction policy v1 like issue #124 doc facts. Returns `None` for
/// anything that is not a single string literal.
fn deprecation_literal_text(literal: Node<'_>, source: &str) -> Option<String> {
    let decoded = string_literal_text(node_source(literal, source).trim())?;
    let bounded: String = decoded.chars().take(MAX_DEPRECATION_STRING_LEN).collect();
    Some(crate::redaction::redact_value(&bounded))
}

/// Parses one `attribute_item` into a [`DeprecationMark`] when its attribute
/// name leaf is exactly `deprecated` (issue #249). The three supported forms
/// are the bare `#[deprecated]`, the note shorthand `#[deprecated = "..."]`,
/// and the meta form `#[deprecated(since = "...", note = "...")]`. Key/value
/// pairs are read from the attribute's `arguments` token tree by walking
/// named children — Tree-sitter node walking only, never regex.
///
/// Unknown keys, non-literal values, and unnamed literals contribute
/// nothing: the mark's *presence* is the fact either way. Returns `None` for
/// every other attribute.
fn deprecation_from_attribute(attribute_item: Node<'_>, source: &str) -> Option<DeprecationMark> {
    let attribute = first_descendant_of_kind(attribute_item, "attribute")?;
    let name_node = attribute.named_child(0)?;
    let raw_name = node_source(name_node, source).trim();
    let leaf = raw_name
        .rsplit("::")
        .next()
        .map_or_else(|| raw_name.trim(), str::trim);
    if leaf != "deprecated" {
        return None;
    }
    let mut since: Option<String> = None;
    let mut note: Option<String> = None;
    if let Some(value) = attribute.child_by_field_name("value") {
        // `#[deprecated = "..."]`: the value expression is the note shorthand.
        if value.kind() == "string_literal" {
            note = deprecation_literal_text(value, source);
        }
    } else if let Some(arguments) = attribute.child_by_field_name("arguments") {
        // `#[deprecated(since = "...", note = "...")]`: pair each `since` /
        // `note` identifier key with the string literal that follows it.
        let mut cursor = arguments.walk();
        let named: Vec<Node<'_>> = arguments.named_children(&mut cursor).collect();
        for (index, child) in named.iter().enumerate() {
            if child.kind() != "identifier" {
                continue;
            }
            let key = node_source(*child, source).trim();
            if !matches!(key, "since" | "note") {
                continue;
            }
            let Some(literal) = named.get(index + 1) else {
                continue;
            };
            if literal.kind() != "string_literal" {
                continue;
            }
            let Some(text) = deprecation_literal_text(*literal, source) else {
                continue;
            };
            if key == "since" {
                since = Some(text);
            } else {
                note = Some(text);
            }
        }
    }
    Some(DeprecationMark { since, note })
}

/// Splits `text` on `delimiter` at the top nesting level only, ignoring
/// delimiters inside `(`/`[`/`{` pairs, string/character literals (with
/// backslash escapes), and line/block comments. Yields the segments in order;
/// deterministic. Used to isolate the predicate of a
/// `#[cfg_attr(predicate, …)]` gate from its attribute payload without
/// mis-splitting on commas inside nested predicates or string literals
/// (issue #190).
fn split_top_level(text: &str, delimiter: char) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth = 0usize;
    let mut segment_start = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '"' | '\'' => {
                // Skip the literal body; a backslash escapes the next char.
                let quote = ch;
                let mut escaped = false;
                for (_, literal_ch) in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if literal_ch == '\\' {
                        escaped = true;
                    } else if literal_ch == quote {
                        break;
                    }
                }
            }
            '/' => {
                // Skip `//…` and `/*…*/` comments so a delimiter inside one
                // never splits.
                match chars.peek() {
                    Some((_, '/')) => {
                        for (_, comment_ch) in chars.by_ref() {
                            if comment_ch == '\n' {
                                break;
                            }
                        }
                    }
                    Some((_, '*')) => {
                        chars.next();
                        let mut previous = '\0';
                        for (_, comment_ch) in chars.by_ref() {
                            if previous == '*' && comment_ch == '/' {
                                break;
                            }
                            previous = comment_ch;
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                if ch == delimiter && depth == 0 {
                    segments.push(&text[segment_start..index]);
                    segment_start = index + ch.len_utf8();
                }
            }
        }
    }
    segments.push(&text[segment_start..]);
    segments
}

/// Normalizes a `#[cfg(...)]` / `#[cfg_attr(...)]` predicate (issue #190):
/// comment trivia is stripped first, then runs of whitespace collapse to a
/// single space. AC8 demands the recorded gate never include comments — a
/// predicate like `feature = /* why */ "x"` records as `feature = "x"`.
/// String and character literals are respected by the stripper: a `//`
/// inside `"a//b"` is predicate text, not a comment.
fn normalize_cfg_predicate(predicate: &str) -> String {
    collapse_whitespace(&strip_comments_keep_newlines(predicate))
}

/// Parses an `attribute` node (the inner node of a `#[…]` / `#![…]`
/// attribute item) into its normalized cfg predicate when the attribute's own
/// name leaf is exactly `cfg` or `cfg_attr` (issue #190). Tree-sitter node
/// walking only, never regex.
///
/// The predicate is the verbatim source text of the attribute's first
/// top-level argument — `#[cfg(feature = "embedded-aletheiadb")]` yields
/// `feature = "embedded-aletheiadb"`, and
/// `#[cfg_attr(feature = "x", allow(dead_code))]` yields `feature = "x"` —
/// with comment trivia stripped and interior whitespace collapsed
/// deterministically. It is recorded, never evaluated, satisfied, or
/// expanded. Returns `None` for every other attribute and for malformed
/// gates with no predicate text (`#[cfg]`, `#[cfg()]`).
fn cfg_predicate_from_attribute_node(attribute: Node<'_>, source: &str) -> Option<String> {
    let name_node = attribute.named_child(0)?;
    let raw_name = node_source(name_node, source).trim();
    let leaf = raw_name
        .rsplit("::")
        .next()
        .map_or_else(|| raw_name.trim(), str::trim);
    if !matches!(leaf, "cfg" | "cfg_attr") {
        return None;
    }
    let arguments = attribute.child_by_field_name("arguments")?;
    let arguments_text = node_source(arguments, source);
    let inner = arguments_text
        .strip_prefix('(')
        .and_then(|text| text.strip_suffix(')'))?;
    split_top_level(inner, ',')
        .into_iter()
        .next()
        .map(normalize_cfg_predicate)
        .filter(|text| !text.is_empty())
}

/// Parses one `attribute_item` (`#[…]`) into its normalized cfg predicate
/// when it is a `#[cfg(...)]` / `#[cfg_attr(...)]` gate (issue #190).
/// Inner `#![…]` items never match here — they apply to the enclosing item,
/// not the following one; file-level inner attributes are collected by
/// [`file_level_cfg_gates`].
fn cfg_predicate_from_attribute(attribute_item: Node<'_>, source: &str) -> Option<String> {
    if attribute_item.kind() != "attribute_item" {
        return None;
    }
    let attribute = first_descendant_of_kind(attribute_item, "attribute")?;
    cfg_predicate_from_attribute_node(attribute, source)
}

/// Collects the normalized cfg predicates from the `attribute_item` siblings
/// immediately preceding `node`, in source order (issue #190). Comments are
/// skipped; any other sibling terminates the scan — the same lexical rule the
/// doc/visibility/deprecation extractors use, so one attribute block never
/// leaks onto a neighboring item. Inner `#![…]` attributes are NOT collected
/// here: they apply to the enclosing item, not the following one — except for
/// an inline module's own body, whose inner attributes gate the module itself
/// ([`module_inner_cfg_gates`]) and are appended after the outer attributes.
fn own_cfg_gates(node: Node<'_>, source: &str) -> Vec<String> {
    let mut gates = Vec::new();
    let mut current = node.prev_sibling();
    while let Some(sibling) = current {
        match sibling.kind() {
            "attribute_item" => {
                if let Some(predicate) = cfg_predicate_from_attribute(sibling, source) {
                    gates.push(predicate);
                }
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        current = sibling.prev_sibling();
    }
    gates.reverse();
    gates.extend(module_inner_cfg_gates(node, source));
    gates
}

/// Collects the cfg predicates of every lexically enclosing item, outermost
/// first (issue #190). Walks the Tree-sitter parent chain: each ancestor's own
/// `#[cfg(...)]` / `#[cfg_attr(...)]` attributes contribute their predicates,
/// so a symbol under `#[cfg(feature = "a")] mod m` inherits
/// `feature = "a"` before its own gates. Ancestor order is reversed, but each
/// ancestor's own attributes stay in source order — reversing the flat vector
/// would scramble per-ancestor attribute order. Pure function of the AST —
/// deterministic and independent of walk order.
fn enclosing_cfg_gates(node: Node<'_>, source: &str) -> Vec<String> {
    let mut per_ancestor: Vec<Vec<String>> = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        per_ancestor.push(own_cfg_gates(parent, source));
        current = parent.parent();
    }
    per_ancestor.into_iter().rev().flatten().collect()
}

/// Collects the normalized cfg predicates from `#![cfg(...)]` /
/// `#![cfg_attr(...)]` inner attributes in an inline module's body (issue
/// #190). They apply to the enclosing module — `mod m { #![cfg(feature =
/// "x")] … }` gates `m` itself — so they belong to the module's own gate
/// chain and are inherited by its members through the parent-chain walk.
/// Returns empty for every other node kind: inner attributes in function
/// bodies apply to the block expression, a different semantic, and stay out
/// of scope.
fn module_inner_cfg_gates(node: Node<'_>, source: &str) -> Vec<String> {
    if node.kind() != "mod_item" {
        return Vec::new();
    }
    let mut gates = Vec::new();
    let Some(body) = node.child_by_field_name("body") else {
        return gates;
    };
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if child.kind() != "inner_attribute_item" {
            continue;
        }
        let Some(attribute) = first_descendant_of_kind(child, "attribute") else {
            continue;
        };
        if let Some(predicate) = cfg_predicate_from_attribute_node(attribute, source) {
            gates.push(predicate);
        }
    }
    gates
}

/// Collects the normalized cfg predicates from `#![cfg(...)]` /
/// `#![cfg_attr(...)]` inner attributes that are direct children of the file
/// root (issue #190). They gate the whole file: the extractor leads every
/// symbol's gate chain with them and exports them on `FileFacts` for the
/// `File` node. `#![…]` attributes nested in an inline module's body gate
/// that module, not the file — they are collected by
/// [`module_inner_cfg_gates`]. Inner attributes anywhere else (a function
/// body) apply to the block expression, a different semantic, and stay out
/// of scope.
fn file_level_cfg_gates(root: Node<'_>, source: &str) -> Vec<String> {
    debug_assert_eq!(root.kind(), "source_file");
    let mut gates = Vec::new();
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() != "inner_attribute_item" {
            continue;
        }
        let Some(attribute) = first_descendant_of_kind(child, "attribute") else {
            continue;
        };
        if let Some(predicate) = cfg_predicate_from_attribute_node(attribute, source) {
            gates.push(predicate);
        }
    }
    gates
}

/// Parses one `attribute_item` (`#[...]`) or `inner_attribute_item`
/// (`#![...]`) into [`LintSuppressionFacts`] when the attribute's own name
/// leaf is exactly `allow` (issue #227). Tree-sitter node walking only,
/// never regex.
///
/// Only the attribute's own name is compared: `#[cfg_attr(test,
/// allow(dead_code))]` never matches (the name leaf is `cfg_attr`), and the
/// bare forms `#[allow]` / `#[allow()]` carry no lint names, so they emit no
/// suppression fact. Returns `None` for every other attribute.
fn lint_suppression_from_attribute(node: Node<'_>, source: &str) -> Option<LintSuppressionFacts> {
    if !matches!(node.kind(), "attribute_item" | "inner_attribute_item") {
        return None;
    }
    let attribute = first_descendant_of_kind(node, "attribute")?;
    let name_node = attribute.named_child(0)?;
    let raw_name = node_source(name_node, source).trim();
    let leaf = raw_name
        .rsplit("::")
        .next()
        .map_or_else(|| raw_name.trim(), str::trim);
    if leaf != "allow" {
        return None;
    }
    let arguments = attribute.child_by_field_name("arguments")?;
    let mut lints = Vec::new();
    collect_suppression_lint_names(arguments, source, &mut lints);
    if lints.is_empty() {
        return None;
    }
    lints.sort();
    lints.dedup();
    Some(LintSuppressionFacts {
        lints,
        scope: suppression_scope(node),
        has_justification: suppression_has_adjacent_comment(node),
        is_inner: node.kind() == "inner_attribute_item",
    })
}

/// Collects lint-path tokens from an `allow` attribute's `arguments` token
/// tree. Inside a token tree the grammar lexes paths flat — `clippy::all`
/// arrives as `identifier("clippy")`, `::`, `identifier("all")`, not as one
/// `scoped_identifier` — so consecutive `identifier (:: identifier)*`
/// runs are re-joined into a single lint path (`clippy::too_many_arguments`);
/// any other token (`,`, parens, literals, …) terminates the current path.
/// A `scoped_identifier` node, where the grammar does produce one, still
/// contributes as a whole. Recurses into nested token trees. Non-path tokens
/// contribute nothing.
fn collect_suppression_lint_names(node: Node<'_>, source: &str, out: &mut Vec<String>) {
    fn flush(current: &mut Option<String>, out: &mut Vec<String>) {
        if let Some(path) = current.take() {
            // A trailing `::` with no final segment is not a lint path.
            let path = path.strip_suffix("::").unwrap_or(&path);
            if !path.is_empty() {
                out.push(path.to_owned());
            }
        }
    }

    let mut cursor = node.walk();
    let mut current: Option<String> = None;
    for child in node.children(&mut cursor) {
        match child.kind() {
            "identifier" => {
                let name = node_source(child, source).trim();
                if name.is_empty() {
                    continue;
                }
                match current.as_mut() {
                    Some(path) if path.ends_with("::") => path.push_str(name),
                    // Defensive: an identifier that does not continue a `::`
                    // run starts a fresh path rather than gluing onto one.
                    _ => {
                        flush(&mut current, out);
                        current = Some(name.to_owned());
                    }
                }
            }
            "::" => {
                if let Some(path) = current.as_mut()
                    && !path.ends_with("::")
                {
                    path.push_str("::");
                }
                // A stray leading `::` with no path under construction is
                // not a lint name; ignore it.
            }
            "scoped_identifier" => {
                flush(&mut current, out);
                let name = node_source(child, source).trim();
                if !name.is_empty() {
                    out.push(name.to_owned());
                }
            }
            "token_tree" => {
                flush(&mut current, out);
                collect_suppression_lint_names(child, source, out);
            }
            _ => flush(&mut current, out),
        }
    }
    flush(&mut current, out);
}

/// Resolves the closed [`LintSuppressionScope`] for an allow-attribute node
/// (issue #227): an outer `attribute_item` always annotates the following
/// item (`item`); an `inner_attribute_item`'s scope is the item that owns the
/// block it opens — the module when that item is a `mod_item` (`module`),
/// the crate root (`crate`), or any other enclosing item (`item`, e.g. a
/// function body carrying `#![allow(..)]`).
fn suppression_scope(node: Node<'_>) -> LintSuppressionScope {
    if node.kind() == "attribute_item" {
        return LintSuppressionScope::Item;
    }
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "source_file" => return LintSuppressionScope::Crate,
            "mod_item" => return LintSuppressionScope::Module,
            "function_item" | "closure_expression" | "const_item" | "static_item"
            | "struct_item" | "enum_item" | "trait_item" | "impl_item" | "type_item"
            | "foreign_mod_item" => return LintSuppressionScope::Item,
            _ => current = parent.parent(),
        }
    }
    // Unreachable for a well-formed parse (every inner attribute sits inside
    // a `source_file`); a closed fallback keeps the lane total.
    LintSuppressionScope::Item
}

/// `true` when a line or block comment (doc comments included) sits
/// immediately adjacent to the attribute node: a comment ending on the line
/// directly above it (preceding justification), or a comment starting on the
/// same line the attribute ends on (trailing justification). Tree-sitter
/// points only — no text scanning — so a comment separated by a blank line
/// is not adjacent.
fn suppression_has_adjacent_comment(node: Node<'_>) -> bool {
    let attr_start_row = node.start_position().row;
    let attr_end_row = node.end_position().row;
    if let Some(prev) = node.prev_sibling()
        && matches!(prev.kind(), "line_comment" | "block_comment")
        && prev.end_position().row + 1 == attr_start_row
    {
        return true;
    }
    if let Some(next) = node.next_sibling()
        && matches!(next.kind(), "line_comment" | "block_comment")
        && next.start_position().row == attr_end_row
    {
        return true;
    }
    false
}

/// Maps one attribute item's source text onto a closed entry-point class
/// (issue #240): `#[test]` / `#[bench]`, or a path attribute ending in
/// `::test` / `::bench` (e.g. `#[tokio::test]`), marks a test-harness
/// entry; `#[no_mangle]` / `#[export_name]` marks an FFI export.
/// Configuration attributes that merely mention these tokens —
/// `#[cfg(test)]`, `#[cfg_attr(test, ...)]` — never match: only the
/// attribute's own name is compared, exactly like [`attribute_is_test`].
fn entry_point_kind_from_attribute(text: &str) -> Option<EntryPointKind> {
    let stripped: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let inner = stripped.strip_prefix("#[")?.strip_suffix(']')?;
    let name = inner.split(['(', '=']).next().unwrap_or(inner);
    if name == "test" || name.ends_with("::test") || name == "bench" || name.ends_with("::bench") {
        Some(EntryPointKind::Test)
    } else if name == "no_mangle" || name == "export_name" {
        Some(EntryPointKind::FfiExport)
    } else {
        None
    }
}

/// `true` when one of `node`'s ancestors is a `trait_item` (issue #240).
/// Trait-method signatures and default-bodied trait methods arrive at
/// `add_symbol` with `symbol_kind == "function"`, but a `fn main` declared
/// in a trait is not the binary's entry point.
fn has_trait_item_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "trait_item" {
            return true;
        }
        node = parent;
    }
    false
}

/// `true` when the repo-relative path is a binary crate root: `src/main.rs`
/// or anything under `src/bin/`. A free `fn main` there is the binary's
/// entry point; a `fn main` in a library root (`src/lib.rs`) is an ordinary
/// function, not an entry point. Mirrors the `src/`-rooted scope of the
/// issue #213 library-crate rule — workspace-member prefixes are out of
/// this slice's scope.
fn is_binary_crate_path(path: &str) -> bool {
    let mut segments = path.split(['/', '\\']).filter(|s| !s.is_empty());
    if segments.next() != Some("src") {
        return false;
    }
    match segments.next() {
        Some("main.rs") => segments.next().is_none(),
        Some("bin") => true,
        _ => false,
    }
}
/// `#[test]` or a path attribute whose name ends in `::test` (such as
/// `true` when one attribute item's source text is a dedicated test attribute:
/// `#[test]` or a path attribute whose name ends in `::test` (such as
/// `#[tokio::test]`), with or without arguments. Configuration attributes that
/// merely mention `test` — `#[cfg(test)]`, `#[cfg(not(test))]`,
/// `#[cfg_attr(test, ...)]` — never match.
fn attribute_is_test(text: &str) -> bool {
    let stripped: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(inner) = stripped
        .strip_prefix("#[")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    let name = inner.split('(').next().unwrap_or(inner);
    name == "test" || name.ends_with("::test")
}

/// Maps a routing-attribute name onto its uppercased HTTP method when the name
/// is in the closed method set (case-insensitive), else `None` (issue #445).
/// The set is deliberately closed — a generic attribute shape, no
/// framework-specific hardcoding beyond the method vocabulary.
fn http_method_from_attribute_name(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "get" => Some("GET"),
        "post" => Some("POST"),
        "put" => Some("PUT"),
        "delete" => Some("DELETE"),
        "patch" => Some("PATCH"),
        "head" => Some("HEAD"),
        "options" => Some("OPTIONS"),
        _ => None,
    }
}

/// `true` when a macro name is in the closed route-registration set (issue
/// #445). Only `routes` today (the Rocket/autumn `routes![…]` shape); the set is
/// closed — no arbitrary-macro parsing.
fn is_route_registration_macro(name: &str) -> bool {
    name == "routes"
}

/// Returns the first descendant of `node` (pre-order, source order) whose kind
/// equals `kind`, or `None`. Deterministic first match.
fn first_descendant_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = first_descendant_of_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

/// Computes the structural complexity score (issue #162) for a Rust
/// `function_item` or `function_signature_item` node: 1 plus one per
/// decision point in the item's own body.
///
/// A decision point is one of:
/// - an `if_expression` (each `else if` is its own `if_expression` node),
/// - a `for_expression`, `while_expression`, or `loop_expression`,
/// - a `match_arm` (every arm, including catch-alls),
/// - a `try_expression` (the `?` operator),
/// - a `binary_expression` whose operator token is `&&` or `||`
///   (anonymous token kinds are the literal operator text).
///
/// Closure bodies are part of the enclosing item's expression tree and are
/// counted. Nested `function_item` / `function_signature_item` subtrees are
/// NOT descended into: a nested `fn` gets its own `Symbol` node and its own
/// score. Items without a body (signature-only trait method declarations)
/// score the minimum 1.
///
/// The walk is over the Tree-sitter AST — no regexes, no text heuristics —
/// so the score is a deterministic function of the parsed source.
pub(crate) fn control_flow_complexity(item: Node<'_>) -> u32 {
    let Some(body) = item.child_by_field_name("body") else {
        return 1;
    };
    let mut score = 1u32;
    let mut stack = vec![body];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // A nested `fn` gets its own symbol: skip its whole subtree so
            // its decisions never inflate the enclosing callable's score.
            // The `body` root is a `block`, never a `function_item`, so the
            // outer item's own body is always walked.
            "function_item" | "function_signature_item" => continue,
            "if_expression" | "for_expression" | "while_expression" | "loop_expression"
            | "match_arm" | "try_expression" => score += 1,
            "binary_expression" => {
                if node
                    .child_by_field_name("operator")
                    .is_some_and(|op| matches!(op.kind(), "&&" | "||"))
                {
                    score += 1;
                }
            }
            _ => {}
        }
        let mut cursor = node.walk();
        stack.extend(node.children(&mut cursor));
    }
    score
}

fn macro_invocation_name(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .split_once('!')
        .map_or(trimmed, |(name, _)| name.trim())
        .trim_end_matches(';')
        .to_owned()
        + "!"
}

/// Maps a repo-relative Rust file path onto its crate-relative module path
/// (e.g. `src/api/inner.rs` → `["api", "inner"]`). `src/lib.rs`, `src/main.rs`
/// and `mod.rs` files map onto their containing directory's module path.
/// Non-`src/`-rooted paths return an empty path.
///
/// Shared with the public-API reachability query (issue #213) so query-time
/// module attribution matches extraction-time symbol qualification exactly.
pub(crate) fn file_module_path(repo_relative_path: &str) -> Vec<String> {
    // Compute the module path against the crate-relative remainder, so a
    // workspace member crate (`crates/foo/src/mod_b.rs`) yields `["mod_b"]`
    // rather than an empty path (issue #440). A single-crate `src/...` layout
    // has an empty prefix, so `owned` is byte-identical to the pre-#440 split.
    let (_, owned) = crate::languages::cross_file::split_crate_prefix(repo_relative_path);
    let parts: Vec<&str> = owned.iter().map(String::as_str).collect();
    if parts.first() != Some(&"src") {
        return Vec::new();
    }

    if parts.get(1) == Some(&"bin") {
        return binary_module_path(&parts);
    }

    module_path_from_file_parts(&parts[1..])
}

fn binary_module_path(parts: &[&str]) -> Vec<String> {
    let Some(parts_after_bin) = parts.get(2..) else {
        return Vec::new();
    };
    if parts_after_bin.len() <= 1 {
        return Vec::new();
    }

    let parts_after_target = &parts_after_bin[1..];
    if matches!(parts_after_target, ["main.rs" | "mod.rs"]) {
        return Vec::new();
    }

    module_path_from_file_parts(parts_after_target)
}

fn module_path_from_file_parts(parts: &[&str]) -> Vec<String> {
    let mut module_parts = parts.to_vec();
    let Some(last) = module_parts.pop() else {
        return Vec::new();
    };
    match last {
        "lib.rs" | "main.rs" | "mod.rs" => {}
        file_name => {
            if let Some(stem) = file_name.strip_suffix(".rs") {
                module_parts.push(stem);
            }
        }
    }

    module_parts.into_iter().map(ToOwned::to_owned).collect()
}

#[allow(dead_code)]
fn _path_for_error(path: &std::path::Path) -> PathBuf {
    path.to_path_buf()
}

/// Compact BLAKE3 content signature over the normalized source body (issue #206).
///
/// Used to stamp `Module` / `Import` nodes whose display `summary` is name-only
/// so a body change with an unchanged name stays content-detectable by
/// evidence-freshness drift. Deterministic: [`normalize_code`] is byte-stable,
/// so CRLF and LF checkouts yield the same handle.
#[must_use]
pub fn content_signature(body: &str) -> String {
    let normalized = normalize_code(body);
    format!("blake3:{}", blake3::hash(normalized.as_bytes()).to_hex())
}

/// Normalizes source code by stripping comments and collapsing whitespace.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn normalize_code(code: &str) -> String {
    let mut result = String::new();
    let mut in_line_comment = false;
    let mut block_comment_depth = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut in_raw_string = false;
    let mut raw_string_hashes = 0;
    let mut escaped = false;

    let mut pending_space = false;
    let mut last_pushed: Option<char> = None;

    let mut push_char = |c: char, in_literal: bool| {
        if c.is_whitespace() {
            if in_literal {
                result.push(c);
                last_pushed = Some(c);
            } else {
                pending_space = true;
            }
        } else {
            if pending_space {
                pending_space = false;
                if !in_literal {
                    let is_current_ident = c.is_alphanumeric() || c == '_';
                    let is_last_ident =
                        last_pushed.is_some_and(|last| last.is_alphanumeric() || last == '_');
                    if is_current_ident && is_last_ident {
                        result.push(' ');
                    }
                }
            }
            result.push(c);
            last_pushed = Some(c);
        }
    };

    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                push_char('\n', false);
            }
        } else if block_comment_depth > 0 {
            if i + 1 < chars.len() && c == '*' && chars[i + 1] == '/' {
                block_comment_depth -= 1;
                if block_comment_depth == 0 {
                    push_char(' ', false); // Preserve a separator to prevent token concatenation
                }
                i += 1;
            } else if i + 1 < chars.len() && c == '/' && chars[i + 1] == '*' {
                block_comment_depth += 1;
                i += 1;
            }
        } else if in_raw_string {
            let mut is_end = false;
            if c == '"' {
                let mut matches = true;
                for k in 0..raw_string_hashes {
                    if i + 1 + k >= chars.len() || chars[i + 1 + k] != '#' {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    is_end = true;
                }
            }

            push_char(c, true);
            if is_end {
                for _ in 0..raw_string_hashes {
                    push_char('#', true);
                }
                in_raw_string = false;
                i += raw_string_hashes;
            }
        } else if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            push_char(c, true);
        } else if in_char {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                in_char = false;
            }
            push_char(c, true);
        } else if i + 1 < chars.len() && c == '/' && chars[i + 1] == '/' {
            in_line_comment = true;
            i += 1;
        } else if i + 1 < chars.len() && c == '/' && chars[i + 1] == '*' {
            block_comment_depth = 1;
            i += 1;
        } else if let Some((p_len, h_count)) = {
            // Check for raw string literal start
            let mut prefix_len = 0;
            if c == 'r' {
                prefix_len = 1;
            } else if (c == 'b' || c == 'c') && i + 1 < chars.len() && chars[i + 1] == 'r' {
                prefix_len = 2;
            }
            let mut is_raw_str = false;
            let mut hashes_count = 0;
            if prefix_len > 0 {
                // Must not be preceded by alphanumeric/underscore (identifier part)
                let preceded_by_ident = if i > 0 {
                    let prev = chars[i - 1];
                    prev.is_alphanumeric() || prev == '_'
                } else {
                    false
                };
                if !preceded_by_ident {
                    let mut temp_idx = i + prefix_len;
                    while temp_idx < chars.len() && chars[temp_idx] == '#' {
                        temp_idx += 1;
                    }
                    if temp_idx < chars.len() && chars[temp_idx] == '"' {
                        is_raw_str = true;
                        hashes_count = temp_idx - (i + prefix_len);
                    }
                }
            }
            if is_raw_str {
                Some((prefix_len, hashes_count))
            } else {
                None
            }
        } {
            in_raw_string = true;
            raw_string_hashes = h_count;
            for &item in &chars[i..=(i + p_len + h_count)] {
                push_char(item, true);
            }
            i += p_len + h_count;
        } else if c == '"' {
            in_string = true;
            push_char(c, true);
        } else if c == '\'' {
            // Check if this is likely a character literal rather than a lifetime.
            let mut is_char_lit = false;
            let mut j = i + 1;
            while j < chars.len() && j <= i + 10 && chars[j] != '\n' {
                if chars[j] == '\'' {
                    is_char_lit = true;
                    break;
                }
                if chars[j].is_whitespace() && (j != i + 1 || chars.get(i + 2) != Some(&'\'')) {
                    break;
                }
                j += 1;
            }
            if is_char_lit {
                in_char = true;
                push_char(c, true);
            } else {
                push_char(c, false);
            }
        } else {
            push_char(c, false);
        }
        i += 1;
    }
    result.trim().to_owned()
}

#[allow(clippy::too_many_lines)]
fn strip_comments_keep_newlines(code: &str) -> String {
    let mut result = String::new();
    let mut in_line_comment = false;
    let mut block_comment_depth = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut in_raw_string = false;
    let mut raw_string_hashes = 0;
    let mut escaped = false;

    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                result.push('\n');
            }
        } else if block_comment_depth > 0 {
            if c == '\n' {
                result.push('\n');
            } else if i + 1 < chars.len() && c == '*' && chars[i + 1] == '/' {
                block_comment_depth -= 1;
                if block_comment_depth == 0 {
                    result.push(' ');
                }
                i += 1;
            } else if i + 1 < chars.len() && c == '/' && chars[i + 1] == '*' {
                block_comment_depth += 1;
                i += 1;
            }
        } else if in_raw_string {
            let mut is_end = false;
            if c == '"' {
                let mut matches = true;
                for k in 0..raw_string_hashes {
                    if i + 1 + k >= chars.len() || chars[i + 1 + k] != '#' {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    is_end = true;
                }
            }
            result.push(c);
            if is_end {
                for _ in 0..raw_string_hashes {
                    result.push('#');
                }
                in_raw_string = false;
                i += raw_string_hashes;
            }
        } else if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            result.push(c);
        } else if in_char {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                in_char = false;
            }
            result.push(c);
        } else if i + 1 < chars.len() && c == '/' && chars[i + 1] == '/' {
            in_line_comment = true;
            i += 1;
        } else if i + 1 < chars.len() && c == '/' && chars[i + 1] == '*' {
            block_comment_depth = 1;
            i += 1;
        } else if let Some((p_len, h_count)) = {
            let mut prefix_len = 0;
            if c == 'r' {
                prefix_len = 1;
            } else if (c == 'b' || c == 'c') && i + 1 < chars.len() && chars[i + 1] == 'r' {
                prefix_len = 2;
            }
            let mut is_raw_str = false;
            let mut hashes_count = 0;
            if prefix_len > 0 {
                let preceded_by_ident = if i > 0 {
                    let prev = chars[i - 1];
                    prev.is_alphanumeric() || prev == '_'
                } else {
                    false
                };
                if !preceded_by_ident {
                    let mut temp_idx = i + prefix_len;
                    while temp_idx < chars.len() && chars[temp_idx] == '#' {
                        temp_idx += 1;
                    }
                    if temp_idx < chars.len() && chars[temp_idx] == '"' {
                        is_raw_str = true;
                        hashes_count = temp_idx - (i + prefix_len);
                    }
                }
            }
            if is_raw_str {
                Some((prefix_len, hashes_count))
            } else {
                None
            }
        } {
            in_raw_string = true;
            raw_string_hashes = h_count;
            result.extend(chars[i..=(i + p_len + h_count)].iter());
            i += p_len + h_count;
        } else if c == '"' {
            in_string = true;
            result.push(c);
        } else if c == '\'' {
            let mut is_char_lit = false;
            let mut j = i + 1;
            while j < chars.len() && j <= i + 10 && chars[j] != '\n' {
                if chars[j] == '\'' {
                    is_char_lit = true;
                    break;
                }
                if chars[j].is_whitespace() && (j != i + 1 || chars.get(i + 2) != Some(&'\'')) {
                    break;
                }
                j += 1;
            }
            if is_char_lit {
                in_char = true;
            }
            result.push(c);
        } else {
            result.push(c);
        }
        i += 1;
    }
    result
}

/// Normalizes file content by removing top-level `use` import declarations, comments, and collapsing whitespace.
#[must_use]
#[allow(clippy::too_many_lines)]
fn parse_use_statement(chars: &[char], mut idx: usize) -> Option<usize> {
    // Helper to skip whitespace
    let skip_whitespace = |chars: &[char], mut i: usize| -> usize {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        i
    };

    idx = skip_whitespace(chars, idx);

    // Loop to parse zero or more attributes
    loop {
        if idx < chars.len() && chars[idx] == '#' {
            let mut j = idx + 1;
            j = skip_whitespace(chars, j);
            if j < chars.len() && chars[j] == '[' {
                // Parse matching ']'
                let mut bracket_depth = 1;
                let mut in_str = false;
                let mut in_ch = false;
                let mut esc = false;
                j += 1;
                while j < chars.len() && bracket_depth > 0 {
                    let c2 = chars[j];
                    if in_str {
                        if esc {
                            esc = false;
                        } else if c2 == '\\' {
                            esc = true;
                        } else if c2 == '"' {
                            in_str = false;
                        }
                    } else if in_ch {
                        if esc {
                            esc = false;
                        } else if c2 == '\\' {
                            esc = true;
                        } else if c2 == '\'' {
                            in_ch = false;
                        }
                    } else {
                        match c2 {
                            '"' => in_str = true,
                            '\'' => in_ch = true,
                            '[' => bracket_depth += 1,
                            ']' => bracket_depth -= 1,
                            _ => {}
                        }
                    }
                    j += 1;
                }
                if bracket_depth == 0 {
                    idx = skip_whitespace(chars, j);
                    continue;
                }
                return None; // Malformed attribute
            }
        }
        break;
    }

    // Parse visibility
    if idx + 2 < chars.len() && chars[idx] == 'p' && chars[idx + 1] == 'u' && chars[idx + 2] == 'b'
    {
        let after_pub = idx + 3;
        // Ensure "pub" is a whole word
        if after_pub == chars.len()
            || (!chars[after_pub].is_alphanumeric() && chars[after_pub] != '_')
        {
            idx = skip_whitespace(chars, after_pub);
            if idx < chars.len() && chars[idx] == '(' {
                // Parse matching ')'
                let mut paren_depth = 1;
                let mut in_str = false;
                let mut in_ch = false;
                let mut esc = false;
                let mut j = idx + 1;
                while j < chars.len() && paren_depth > 0 {
                    let c2 = chars[j];
                    if in_str {
                        if esc {
                            esc = false;
                        } else if c2 == '\\' {
                            esc = true;
                        } else if c2 == '"' {
                            in_str = false;
                        }
                    } else if in_ch {
                        if esc {
                            esc = false;
                        } else if c2 == '\\' {
                            esc = true;
                        } else if c2 == '\'' {
                            in_ch = false;
                        }
                    } else {
                        match c2 {
                            '"' => in_str = true,
                            '\'' => in_ch = true,
                            '(' => paren_depth += 1,
                            ')' => paren_depth -= 1,
                            _ => {}
                        }
                    }
                    j += 1;
                }
                if paren_depth == 0 {
                    idx = skip_whitespace(chars, j);
                } else {
                    return None; // Malformed visibility
                }
            }
        }
    }

    // Now we must see the "use" keyword
    if idx + 2 < chars.len() && chars[idx] == 'u' && chars[idx + 1] == 's' && chars[idx + 2] == 'e'
    {
        let after_use = idx + 3;
        if after_use == chars.len()
            || (!chars[after_use].is_alphanumeric() && chars[after_use] != '_')
        {
            // Yes! It's a use statement. Now find the matching semicolon ';' at brace_depth 0 (relative to the use statement's body)
            let mut j = after_use;
            let mut use_brace_depth: usize = 0;
            let mut in_str = false;
            let mut in_ch = false;
            let mut in_raw_str = false;
            let mut raw_str_hashes = 0;
            let mut esc = false;

            while j < chars.len() {
                let c2 = chars[j];
                if in_raw_str {
                    let mut is_end = false;
                    if c2 == '"' {
                        let mut matches = true;
                        for k in 0..raw_str_hashes {
                            if j + 1 + k >= chars.len() || chars[j + 1 + k] != '#' {
                                matches = false;
                                break;
                            }
                        }
                        if matches {
                            is_end = true;
                        }
                    }
                    if is_end {
                        in_raw_str = false;
                        j += raw_str_hashes;
                    }
                } else if in_str {
                    if esc {
                        esc = false;
                    } else if c2 == '\\' {
                        esc = true;
                    } else if c2 == '"' {
                        in_str = false;
                    }
                } else if in_ch {
                    if esc {
                        esc = false;
                    } else if c2 == '\\' {
                        esc = true;
                    } else if c2 == '\'' {
                        in_ch = false;
                    }
                } else {
                    // Check for raw string start
                    let is_raw_str_start = {
                        let mut prefix_len = 0;
                        if c2 == 'r' {
                            prefix_len = 1;
                        } else if (c2 == 'b' || c2 == 'c')
                            && j + 1 < chars.len()
                            && chars[j + 1] == 'r'
                        {
                            prefix_len = 2;
                        }
                        let mut is_raw = false;
                        let mut hashes_count = 0;
                        if prefix_len > 0 {
                            let preceded_by_ident = if j > 0 {
                                let prev = chars[j - 1];
                                prev.is_alphanumeric() || prev == '_'
                            } else {
                                false
                            };
                            if !preceded_by_ident {
                                let mut temp_idx = j + prefix_len;
                                while temp_idx < chars.len() && chars[temp_idx] == '#' {
                                    temp_idx += 1;
                                }
                                if temp_idx < chars.len() && chars[temp_idx] == '"' {
                                    is_raw = true;
                                    hashes_count = temp_idx - (j + prefix_len);
                                }
                            }
                        }
                        if is_raw {
                            Some((prefix_len, hashes_count))
                        } else {
                            None
                        }
                    };

                    if let Some((p_len, h_count)) = is_raw_str_start {
                        in_raw_str = true;
                        raw_str_hashes = h_count;
                        j += p_len + h_count;
                    } else if c2 == '"' {
                        in_str = true;
                    } else if c2 == '\'' {
                        let mut is_char_lit = false;
                        let mut k = j + 1;
                        while k < chars.len() && k <= j + 10 && chars[k] != '\n' {
                            if chars[k] == '\'' {
                                is_char_lit = true;
                                break;
                            }
                            if chars[k].is_whitespace()
                                && (k != j + 1 || chars.get(j + 2) != Some(&'\''))
                            {
                                break;
                            }
                            k += 1;
                        }
                        if is_char_lit {
                            in_ch = true;
                        }
                    } else if c2 == '{' {
                        use_brace_depth += 1;
                    } else if c2 == '}' {
                        use_brace_depth = use_brace_depth.saturating_sub(1);
                    } else if c2 == ';' && use_brace_depth == 0 {
                        return Some(j);
                    }
                }
                j += 1;
            }
        }
    }

    None
}

/// Normalizes file content by removing top-level `use` import declarations, comments, and collapsing whitespace.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn normalize_file_code(code: &str) -> String {
    let clean_code = strip_comments_keep_newlines(code);
    let chars: Vec<char> = clean_code.chars().collect();

    let mut import_lines = Vec::new();
    let mut other_code = String::new();

    let mut brace_depth: usize = 0;
    let mut in_string = false;
    let mut in_char = false;
    let mut in_raw_string = false;
    let mut raw_string_hashes = 0;
    let mut escaped = false;

    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let parsed_use = Some(())
            .filter(|()| brace_depth == 0 && !in_string && !in_char && !in_raw_string)
            .filter(|()| i == 0 || (!chars[i - 1].is_alphanumeric() && chars[i - 1] != '_'))
            .and_then(|()| parse_use_statement(&chars, i));
        if let Some(end_idx) = parsed_use {
            let import_stmt: String = chars[i..=end_idx].iter().collect();
            let trimmed = import_stmt.trim().to_owned();
            if !trimmed.is_empty() {
                import_lines.push(trimmed);
            }
            i = end_idx + 1;
            continue;
        }

        other_code.push(c);
        if in_raw_string {
            let mut is_end = false;
            if c == '"' {
                let mut matches = true;
                for k in 0..raw_string_hashes {
                    if i + 1 + k >= chars.len() || chars[i + 1 + k] != '#' {
                        matches = false;
                        break;
                    }
                }
                if matches {
                    is_end = true;
                }
            }
            if is_end {
                for _ in 0..raw_string_hashes {
                    other_code.push('#');
                }
                in_raw_string = false;
                i += raw_string_hashes;
            }
        } else if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
        } else if in_char {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                in_char = false;
            }
        } else if let Some((p_len, h_count)) = {
            let mut prefix_len = 0;
            if c == 'r' {
                prefix_len = 1;
            } else if (c == 'b' || c == 'c') && i + 1 < chars.len() && chars[i + 1] == 'r' {
                prefix_len = 2;
            }
            let mut is_raw_str = false;
            let mut hashes_count = 0;
            if prefix_len > 0 {
                let preceded_by_ident = if i > 0 {
                    let prev = chars[i - 1];
                    prev.is_alphanumeric() || prev == '_'
                } else {
                    false
                };
                if !preceded_by_ident {
                    let mut temp_idx = i + prefix_len;
                    while temp_idx < chars.len() && chars[temp_idx] == '#' {
                        temp_idx += 1;
                    }
                    if temp_idx < chars.len() && chars[temp_idx] == '"' {
                        is_raw_str = true;
                        hashes_count = temp_idx - (i + prefix_len);
                    }
                }
            }
            if is_raw_str {
                Some((prefix_len, hashes_count))
            } else {
                None
            }
        } {
            in_raw_string = true;
            raw_string_hashes = h_count;
            other_code.extend(chars[(i + 1)..=(i + p_len + h_count)].iter());
            i += p_len + h_count;
        } else if c == '"' {
            in_string = true;
        } else if c == '\'' {
            let mut is_char_lit = false;
            let mut j = i + 1;
            while j < chars.len() && j <= i + 10 && chars[j] != '\n' {
                if chars[j] == '\'' {
                    is_char_lit = true;
                    break;
                }
                if chars[j].is_whitespace() && (j != i + 1 || chars.get(i + 2) != Some(&'\'')) {
                    break;
                }
                j += 1;
            }
            if is_char_lit {
                in_char = true;
            }
        } else if c == '{' {
            brace_depth += 1;
        } else if c == '}' {
            brace_depth = brace_depth.saturating_sub(1);
        }
        i += 1;
    }

    import_lines.sort_unstable();
    let mut combined = import_lines.join("\n");
    if !combined.is_empty() {
        combined.push('\n');
    }
    combined.push_str(&other_code);

    normalize_code(&combined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve_target(display: &str) -> Option<String> {
        match impl_trait_target(display) {
            ImplTargetDecision::Resolve(name) => Some(name),
            ImplTargetDecision::Verbatim | ImplTargetDecision::NoEdge => None,
        }
    }

    /// Locates the first `impl_item` node in a parsed tree, for the AST-based
    /// decision tests below.
    fn find_impl_item(node: Node<'_>) -> Option<Node<'_>> {
        if node.kind() == "impl_item" {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = find_impl_item(child) {
                return Some(found);
            }
        }
        None
    }

    /// Parses one impl header from real Rust source and returns the
    /// AST-derived IMPLEMENTS decision, exercising `impl_target_decision`
    /// exactly as the extractor does at walk time (never the display-string
    /// parser, except for the turbofish recovery it internally delegates to).
    fn ast_decision(source: &str) -> ImplTargetDecision {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        let tree = parser.parse(source, None).expect("parse source");
        let impl_node = find_impl_item(tree.root_node()).expect("impl_item present");
        let display = impl_display(node_source(impl_node, source));
        impl_target_decision(impl_node, source, &display)
    }

    /// The AST decision reduced to the resolved trait/type name, mirroring
    /// [`resolve_target`] but driven by a real parse.
    fn ast_resolve(source: &str) -> Option<String> {
        match ast_decision(source) {
            ImplTargetDecision::Resolve(name) => Some(name),
            ImplTargetDecision::Verbatim | ImplTargetDecision::NoEdge => None,
        }
    }

    /// Parses `source` and returns its root node's tree (issue #445 route tests).
    fn parse_tree(source: &str) -> tree_sitter::Tree {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        parser.parse(source, None).expect("parse source")
    }

    /// Reconstructs `route_annotation_from_attribute` over a real parse using
    /// the same free helpers the extractor method uses, so the Tree-sitter
    /// walking path is exercised without a full `RustExtractor` instance.
    fn route_attr(source: &str) -> Option<(String, String)> {
        let tree = parse_tree(source);
        let attribute = first_descendant_of_kind(tree.root_node(), "attribute")?;
        let name_node = attribute.named_child(0)?;
        let raw = source[name_node.byte_range()].trim().to_owned();
        let leaf = raw.rsplit("::").next().unwrap_or(&raw).trim();
        let method = http_method_from_attribute_name(leaf)?;
        let literal = first_descendant_of_kind(attribute, "string_literal")?;
        let path = string_literal_text(source[literal.byte_range()].trim())?;
        Some((method.to_owned(), path))
    }

    /// Collects the bare handler identifiers a route-registration macro would
    /// register, gated by `is_route_registration_macro`, mirroring
    /// `collect_route_registrations`.
    fn route_macro_idents(source: &str) -> Option<Vec<String>> {
        let tree = parse_tree(source);
        let macro_node = first_descendant_of_kind(tree.root_node(), "macro_invocation")?;
        let name_node = macro_node.child_by_field_name("macro")?;
        let raw = source[name_node.byte_range()].trim().to_owned();
        let leaf = raw.rsplit("::").next().unwrap_or(&raw).trim();
        if !is_route_registration_macro(leaf) {
            return None;
        }
        let mut macro_cursor = macro_node.walk();
        let token_tree = macro_node
            .children(&mut macro_cursor)
            .find(|child| child.kind() == "token_tree")?;
        let mut cursor = token_tree.walk();
        Some(
            token_tree
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "identifier")
                .map(|child| source[child.byte_range()].trim().to_owned())
                .collect(),
        )
    }

    #[test]
    fn http_method_attribute_name_maps_closed_set_case_insensitively() {
        assert_eq!(http_method_from_attribute_name("get"), Some("GET"));
        assert_eq!(http_method_from_attribute_name("POST"), Some("POST"));
        assert_eq!(http_method_from_attribute_name("Put"), Some("PUT"));
        assert_eq!(http_method_from_attribute_name("delete"), Some("DELETE"));
        assert_eq!(http_method_from_attribute_name("patch"), Some("PATCH"));
        assert_eq!(http_method_from_attribute_name("head"), Some("HEAD"));
        assert_eq!(http_method_from_attribute_name("options"), Some("OPTIONS"));
        // Not an HTTP method — no route fact.
        assert_eq!(http_method_from_attribute_name("inline"), None);
        assert_eq!(http_method_from_attribute_name("test"), None);
    }

    #[test]
    fn route_registration_macro_set_is_closed() {
        assert!(is_route_registration_macro("routes"));
        assert!(!is_route_registration_macro("vec"));
        assert!(!is_route_registration_macro("println"));
    }

    /// Runs `deprecation_from_attribute` over the first `attribute_item` in a
    /// real parse, mirroring the extractor's prev-sibling walk input shape
    /// (issue #249).
    fn dep_attr(source: &str) -> Option<DeprecationMark> {
        let tree = parse_tree(source);
        let attribute_item = first_descendant_of_kind(tree.root_node(), "attribute_item")?;
        deprecation_from_attribute(attribute_item, source)
    }

    /// Runs `entry_point_kind_from_attribute` over one attribute item's
    /// source text, mirroring the extractor's prev-sibling walk input shape
    /// (issue #240).
    fn entry_attr(source: &str) -> Option<EntryPointKind> {
        let tree = parse_tree(source);
        let attribute_item = first_descendant_of_kind(tree.root_node(), "attribute_item")?;
        entry_point_kind_from_attribute(&source[attribute_item.byte_range()])
    }

    #[test]
    fn entry_point_test_attributes_are_recognized() {
        assert_eq!(
            entry_attr("#[test]\nfn check() {}"),
            Some(EntryPointKind::Test)
        );
        assert_eq!(
            entry_attr("#[tokio::test]\nasync fn check() {}"),
            Some(EntryPointKind::Test),
            "path attributes ending in ::test are test-harness entries"
        );
        assert_eq!(
            entry_attr("#[bench]\nfn check() {}"),
            Some(EntryPointKind::Test)
        );
    }

    #[test]
    fn entry_point_ffi_attributes_are_recognized() {
        assert_eq!(
            entry_attr("#[no_mangle]\npub extern \"C\" fn f() {}"),
            Some(EntryPointKind::FfiExport)
        );
        assert_eq!(
            entry_attr("#[export_name = \"real_name\"]\nfn f() {}"),
            Some(EntryPointKind::FfiExport)
        );
    }

    #[test]
    fn entry_point_config_attributes_are_not_entry_points() {
        // Configuration attributes that merely mention the tokens — never
        // the attribute's own name — must not mark the item.
        assert_eq!(entry_attr("#[cfg(test)]\nfn check() {}"), None);
        assert_eq!(entry_attr("#[cfg_attr(test, no_mangle)]\nfn f() {}"), None);
        assert_eq!(entry_attr("#[allow(dead_code)]\nfn f() {}"), None);
        assert_eq!(entry_attr("#[inline]\nfn f() {}"), None);
    }

    // ── Test vs. production symbol roles (issue #238, RED) ────────────────

    /// Extracts `source` as the repo-relative `path` and returns every
    /// `Symbol` and `Module` record's `(qualified_name, role)`. Module
    /// declarations are included because a `#[cfg(test)] mod` declaration is
    /// itself the lexical gate (issue #238): its record carries the role.
    fn symbol_roles_at(source: &str, path: &str) -> Vec<(String, Option<SymbolRole>)> {
        let file = SourceFile {
            path: PathBuf::from(path),
            repo_relative_path: path.to_owned(),
        };
        let mut graph = Graph::default();
        extract_file_source(&file, source, "file-id", "repo-id", &mut graph)
            .expect("source should parse");
        graph
            .records()
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Node {
                    kind: NodeKind::Symbol | NodeKind::Module,
                    name: Some(name),
                    ..
                } => Some((name.clone(), r.role().copied())),
                _ => None,
            })
            .collect()
    }

    fn role_of(roles: &[(String, Option<SymbolRole>)], name: &str) -> Option<SymbolRole> {
        roles.iter().find(|(n, _)| n == name).map_or_else(
            || panic!("symbol `{name}` should have been extracted"),
            |(_, role)| *role,
        )
    }

    #[test]
    fn role_test_attribute_fn_is_test() {
        let roles = symbol_roles_at("#[test]\nfn check() {}\n", "src/lib.rs");
        assert_eq!(role_of(&roles, "check"), Some(SymbolRole::Test));
    }

    #[test]
    fn role_test_family_attributes_are_test() {
        let roles = symbol_roles_at(
            "#[tokio::test]\nasync fn async_check() {}\n#[bench]\nfn bench_check() {}\n",
            "src/lib.rs",
        );
        assert_eq!(role_of(&roles, "async_check"), Some(SymbolRole::Test));
        assert_eq!(role_of(&roles, "bench_check"), Some(SymbolRole::Test));
    }

    #[test]
    fn role_cfg_test_module_members_are_test() {
        // Ancestry composition (issue #238 signal b): a symbol nested in a
        // `#[cfg(test)]` module is test, however deep the nesting.
        let roles = symbol_roles_at(
            "#[cfg(test)]\nmod tests {\n    fn helper() {}\n    mod inner {\n        fn deep() {}\n    }\n}\n",
            "src/lib.rs",
        );
        assert_eq!(role_of(&roles, "tests::helper"), Some(SymbolRole::Test));
        assert_eq!(
            role_of(&roles, "tests::inner::deep"),
            Some(SymbolRole::Test)
        );
    }

    #[test]
    fn role_cfg_test_module_declaration_is_test() {
        // The gated module's OWN declaration is the lexical gate (issue
        // #238): it is test even though the scope counter is entered only
        // for its children.
        let roles = symbol_roles_at(
            "#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n",
            "src/lib.rs",
        );
        assert_eq!(role_of(&roles, "tests"), Some(SymbolRole::Test));
        assert_eq!(role_of(&roles, "tests::helper"), Some(SymbolRole::Test));
    }

    #[test]
    fn role_cfg_test_out_of_line_module_declaration_is_test() {
        // Out-of-line form: the `mod name;` declaration carries the gate.
        let roles = symbol_roles_at("#[cfg(test)]\nmod out;\n", "src/lib.rs");
        assert_eq!(role_of(&roles, "out"), Some(SymbolRole::Test));
    }

    #[test]
    fn role_ungated_module_declaration_is_production() {
        // A module without the gate stays production — the gate is the
        // `#[cfg(test)]` attribute, not module-hood.
        let roles = symbol_roles_at("mod plain {\n    fn helper() {}\n}\n", "src/lib.rs");
        assert_eq!(role_of(&roles, "plain"), Some(SymbolRole::Production));
        assert_eq!(
            role_of(&roles, "plain::helper"),
            Some(SymbolRole::Production)
        );
    }

    #[test]
    fn role_production_fn_is_production() {
        let roles = symbol_roles_at("pub fn ship() {}\n", "src/lib.rs");
        assert_eq!(role_of(&roles, "ship"), Some(SymbolRole::Production));
    }

    #[test]
    fn role_tests_dir_file_is_test() {
        // Signal (c): a plain fn in an integration-test root is test.
        let roles = symbol_roles_at("fn integration_check() {}\n", "tests/integration.rs");
        assert_eq!(role_of(&roles, "integration_check"), Some(SymbolRole::Test));
    }

    #[test]
    fn role_benches_dir_file_is_test() {
        let roles = symbol_roles_at("fn bench_main() {}\n", "benches/bench.rs");
        assert_eq!(role_of(&roles, "bench_main"), Some(SymbolRole::Test));
    }

    #[test]
    fn role_windows_separators_still_classify_test_roots() {
        // Separator-agnostic (issue #238): a Windows checkout reports
        // `tests\integration.rs`; the first segment is still `tests`.
        let roles = symbol_roles_at("fn integration_check() {}\n", "tests\\integration.rs");
        assert_eq!(role_of(&roles, "integration_check"), Some(SymbolRole::Test));
        let roles = symbol_roles_at("fn bench_main() {}\n", "benches\\bench.rs");
        assert_eq!(role_of(&roles, "bench_main"), Some(SymbolRole::Test));
        // ... and a production file keeps forward slashes working too.
        let roles = symbol_roles_at("pub fn ship() {}\n", "src\\lib.rs");
        assert_eq!(role_of(&roles, "ship"), Some(SymbolRole::Production));
    }

    #[test]
    fn role_cfg_test_on_non_module_item_is_not_a_signal() {
        // The closed signal set (issue #238): `#[cfg(test)]` on a bare fn is
        // not a test-family attribute (a) and not module gating (b), so the
        // symbol stays production. Documents the boundary; a later slice may
        // extend the vocabulary.
        let roles = symbol_roles_at("#[cfg(test)]\nfn helper() {}\n", "src/lib.rs");
        assert_eq!(role_of(&roles, "helper"), Some(SymbolRole::Production));
    }

    #[test]
    fn role_every_symbol_carries_a_role() {
        let roles = symbol_roles_at(
            "pub fn ship() {}\n#[test]\nfn check() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n}\n",
            "src/lib.rs",
        );
        assert_eq!(roles.len(), 4, "all four symbols extracted");
        assert!(
            roles.iter().all(|(_, role)| role.is_some()),
            "every Symbol record carries a role, got {roles:?}"
        );
    }

    #[test]
    fn binary_crate_paths_are_recognized() {
        assert!(is_binary_crate_path("src/main.rs"));
        assert!(is_binary_crate_path("src/bin/tool.rs"));
        assert!(is_binary_crate_path("src/bin/nested/tool.rs"));
        assert!(!is_binary_crate_path("src/lib.rs"));
        assert!(!is_binary_crate_path("src/main.rs.bak"));
        assert!(!is_binary_crate_path("tests/main.rs"));
        // Workspace-member prefixes are out of this slice's scope.
        assert!(!is_binary_crate_path("crates/foo/src/main.rs"));
    }

    #[test]
    fn deprecated_bare_form_records_mark_without_payload() {
        let mark = dep_attr("#[deprecated]\nfn old() {}").expect("bare deprecated must parse");
        assert_eq!(mark.since, None, "absent since is None, never fabricated");
        assert_eq!(mark.note, None, "absent note is None, never fabricated");
    }

    #[test]
    fn deprecated_note_shorthand_is_verbatim() {
        let mark = dep_attr("#[deprecated = \"use new() instead\"]\nfn old() {}")
            .expect("note shorthand must parse");
        assert_eq!(mark.since, None);
        assert_eq!(mark.note.as_deref(), Some("use new() instead"));
    }

    #[test]
    fn deprecated_since_and_note_meta_form() {
        let mark = dep_attr("#[deprecated(since = \"1.2.0\", note = \"use new()\")]\nfn old() {}")
            .expect("meta form must parse");
        assert_eq!(mark.since.as_deref(), Some("1.2.0"));
        assert_eq!(mark.note.as_deref(), Some("use new()"));
    }

    #[test]
    fn deprecated_meta_form_key_order_is_free() {
        let mark = dep_attr("#[deprecated(note = \"use new()\", since = \"1.2.0\")]\nfn old() {}")
            .expect("reversed key order must parse");
        assert_eq!(mark.since.as_deref(), Some("1.2.0"));
        assert_eq!(mark.note.as_deref(), Some("use new()"));
    }

    #[test]
    fn deprecated_other_attributes_are_ignored() {
        assert_eq!(dep_attr("#[test]\nfn old() {}"), None);
        assert_eq!(dep_attr("#[allow(dead_code)]\nfn old() {}"), None);
        assert_eq!(
            dep_attr("#[doc = \"not a deprecation\"]\nfn old() {}"),
            None,
            "a doc attribute with a deprecation-shaped payload is not a mark"
        );
    }

    #[test]
    fn deprecated_non_literal_values_are_not_captured() {
        // A non-literal `note` value cannot be decoded, so the mark records
        // presence with the payload absent — never a fabricated string.
        let mark = dep_attr("#[deprecated(note = NOTE_CONST)]\nfn old() {}")
            .expect("attribute name still marks the item");
        assert_eq!(mark.since, None);
        assert_eq!(mark.note, None);
    }

    #[test]
    fn deprecated_unknown_meta_keys_are_ignored() {
        let mark = dep_attr("#[deprecated(foo = \"bar\")]\nfn old() {}")
            .expect("unknown keys still mark the item deprecated");
        assert_eq!(mark.since, None);
        assert_eq!(mark.note, None);
    }

    #[test]
    fn deprecated_note_is_bounded() {
        let long_note = "n".repeat(MAX_DEPRECATION_STRING_LEN + 40);
        let source = format!("#[deprecated = \"{long_note}\"]\nfn old() {{}}");
        let mark = dep_attr(&source).expect("long note must parse");
        let note = mark.note.expect("note captured");
        assert_eq!(
            note.len(),
            MAX_DEPRECATION_STRING_LEN,
            "note is bounded to the documented length"
        );
        assert!(
            long_note.starts_with(&note),
            "the bound is a verbatim prefix of the attribute text"
        );
    }

    #[test]
    fn route_attribute_parses_to_method_and_path() {
        assert_eq!(
            route_attr("#[get(\"/api/v1/contacts\")]\nfn list() {}\n"),
            Some(("GET".to_owned(), "/api/v1/contacts".to_owned()))
        );
        assert_eq!(
            route_attr("#[post(\"/api/v1/contacts\")]\nfn create() {}\n"),
            Some(("POST".to_owned(), "/api/v1/contacts".to_owned()))
        );
    }

    #[test]
    fn non_method_attribute_yields_no_route_fact() {
        assert_eq!(route_attr("#[inline]\nfn plain() {}\n"), None);
        assert_eq!(route_attr("#[cfg(test)]\nfn gated() {}\n"), None);
    }

    #[test]
    fn routes_macro_yields_registered_handler_identifier_set() {
        assert_eq!(
            route_macro_idents("fn build() { let _ = routes![list, get_one, create]; }"),
            Some(vec![
                "list".to_owned(),
                "get_one".to_owned(),
                "create".to_owned()
            ])
        );
    }

    #[test]
    fn non_routes_macro_yields_no_registrations() {
        assert_eq!(
            route_macro_idents("fn build() { let _ = vec![list, create]; }"),
            None
        );
    }

    #[test]
    fn bare_simple_name_ambiguity_counts_distinct_same_simple_name_targets() {
        // Two distinct qualified names sharing the simple name `T` (root `T`
        // and `a::T`) — the round-8 collision — is ambiguous.
        assert!(bare_simple_name_is_ambiguous(
            "T",
            ["T", "a::T", "m::Foo"].into_iter()
        ));
        // A lone same-simple-name target is unambiguous — the guard must not
        // over-suppress a legitimate single-name resolve.
        assert!(!bare_simple_name_is_ambiguous(
            "T",
            ["T", "m::Bar"].into_iter()
        ));
        // A single deeply-nested definition is still unambiguous.
        assert!(!bare_simple_name_is_ambiguous(
            "T",
            ["a::b::T", "a::Foo"].into_iter()
        ));
        // Two nested same-simple-name definitions with no root are ambiguous
        // (the type-alias-fallback wrong-edge class).
        assert!(bare_simple_name_is_ambiguous(
            "T",
            ["a::T", "b::T"].into_iter()
        ));
        // A name absent from the index is unambiguous (zero matches).
        assert!(!bare_simple_name_is_ambiguous(
            "Missing",
            ["T", "a::T"].into_iter()
        ));
    }

    /// Locates the first `use_declaration` node in a parsed tree, for the
    /// import-shadow-veto helper tests below.
    fn find_use_declaration(node: Node<'_>) -> Option<Node<'_>> {
        if node.kind() == "use_declaration" {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(found) = find_use_declaration(child) {
                return Some(found);
            }
        }
        None
    }

    /// Parses one `use` declaration from real Rust source and returns the
    /// simple names it binds into scope, driving [`use_bound_names`] exactly as
    /// the extractor does at walk time.
    fn parse_use_bound_names(source: &str) -> Vec<String> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        let tree = parser.parse(source, None).expect("parse source");
        let use_node = find_use_declaration(tree.root_node()).expect("use_declaration present");
        use_bound_names(use_node, source)
    }

    #[test]
    fn use_bound_names_covers_plain_alias_grouped_and_glob() {
        // Plain `use a::b::T;` binds the final segment `T`.
        assert_eq!(parse_use_bound_names("use a::b::T;"), vec!["T".to_owned()]);
        // A bare `use T;` binds `T`.
        assert_eq!(parse_use_bound_names("use T;"), vec!["T".to_owned()]);
        // `use a::b::T as U;` binds the ALIAS `U`, never the path segment `T`.
        assert_eq!(
            parse_use_bound_names("use a::b::T as U;"),
            vec!["U".to_owned()]
        );
        // A grouped `use a::{B, C::D};` binds each leaf simple name.
        assert_eq!(
            parse_use_bound_names("use a::{B, C::D};"),
            vec!["B".to_owned(), "D".to_owned()]
        );
        // A grouped import with an inner alias binds the alias.
        assert_eq!(
            parse_use_bound_names("use a::{B, C::D as E};"),
            vec!["B".to_owned(), "E".to_owned()]
        );
        // A glob `use a::*;` names no specific simple name — it must contribute
        // nothing, so it never vetoes a bare impl-target name.
        assert!(parse_use_bound_names("use a::*;").is_empty());
        // A grouped glob leaf contributes nothing either.
        assert_eq!(
            parse_use_bound_names("use a::{B, c::*};"),
            vec!["B".to_owned()]
        );
    }

    /// Parses one `use` declaration and returns the `(simple name, import path)`
    /// pairs it binds, driving [`use_bound_import_paths`] as the extractor does.
    fn parse_use_import_paths(source: &str) -> Vec<(String, String)> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");
        let tree = parser.parse(source, None).expect("parse source");
        let use_node = find_use_declaration(tree.root_node()).expect("use_declaration present");
        use_bound_import_paths(use_node, source)
    }

    #[test]
    fn use_bound_import_paths_capture_resolved_paths(/* issue #393 */) {
        // Plain `use crate::a::T;` binds `T` to the full import path.
        assert_eq!(
            parse_use_import_paths("use crate::a::T;"),
            vec![("T".to_owned(), "crate::a::T".to_owned())]
        );
        // A bare `use T;` binds `T` to `T`.
        assert_eq!(
            parse_use_import_paths("use T;"),
            vec![("T".to_owned(), "T".to_owned())]
        );
        // `use crate::a::T as U;` binds the alias `U` to the aliased PATH, never
        // the alias text.
        assert_eq!(
            parse_use_import_paths("use crate::a::T as U;"),
            vec![("U".to_owned(), "crate::a::T".to_owned())]
        );
        // Grouped `use crate::a::{T};` distributes the group prefix.
        assert_eq!(
            parse_use_import_paths("use crate::a::{T};"),
            vec![("T".to_owned(), "crate::a::T".to_owned())]
        );
        // Grouped with nested path + inner alias.
        assert_eq!(
            parse_use_import_paths("use crate::a::{B, C::D as E};"),
            vec![
                ("B".to_owned(), "crate::a::B".to_owned()),
                ("E".to_owned(), "crate::a::C::D".to_owned()),
            ]
        );
        // A glob binds no simple name, so it contributes no path — it stays
        // bounded out of import-aware resolution.
        assert!(parse_use_import_paths("use crate::a::*;").is_empty());
        // `super::`/`self::` prefixes are preserved verbatim for the resolver to
        // normalize against the impl's module scope.
        assert_eq!(
            parse_use_import_paths("use super::a::T;"),
            vec![("T".to_owned(), "super::a::T".to_owned())]
        );
    }

    #[test]
    fn scope_imports_bare_name_consults_only_the_impls_own_scope() {
        // Rust `use` visibility is NOT inherited by child modules: the veto
        // consults ONLY the impl's exact own module-scope key (round-10 Codex
        // finding). A file-top `use` (scope `[]`) binding `Display` shadows a
        // bare name at file top, but NOT one inside a descendant module.
        let mut imports: BTreeMap<Vec<String>, BTreeSet<String>> = BTreeMap::new();
        imports.insert(Vec::new(), BTreeSet::from(["Display".to_owned()]));
        assert!(scope_imports_bare_name(&imports, &[], "Display"));
        // Ancestor/root `use` does NOT match a child scope.
        assert!(!scope_imports_bare_name(
            &imports,
            &["m".to_owned(), "n".to_owned()],
            "Display"
        ));
        // A name it does not bind is never shadowed.
        assert!(!scope_imports_bare_name(&imports, &[], "Other"));

        // A `use` inside `mod m` (scope `["m"]`) shadows a bare name in `m`
        // only, never at file top, in a descendant, or in a sibling module.
        let mut nested: BTreeMap<Vec<String>, BTreeSet<String>> = BTreeMap::new();
        nested.insert(vec!["m".to_owned()], BTreeSet::from(["Display".to_owned()]));
        assert!(scope_imports_bare_name(
            &nested,
            &["m".to_owned()],
            "Display"
        ));
        // A descendant `mod m::inner` does NOT inherit `mod m`'s import.
        assert!(!scope_imports_bare_name(
            &nested,
            &["m".to_owned(), "inner".to_owned()],
            "Display"
        ));
        // File top cannot see the import declared inside `mod m`.
        assert!(!scope_imports_bare_name(&nested, &[], "Display"));
        // An unrelated sibling module `mod other` cannot see `mod m`'s import.
        assert!(!scope_imports_bare_name(
            &nested,
            &["other".to_owned()],
            "Display"
        ));
    }

    #[test]
    fn is_module_item_use_distinguishes_module_and_block_scope() {
        // A module-item `use` (file-top or inside a `mod` body) feeds the veto;
        // a block-local `use` inside a function body does NOT (round-10 Codex
        // finding: a block-local `use` is invisible to module-level impls).
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .expect("load rust grammar");

        // File-top `use` — a direct child of `source_file`.
        let top = "use std::fmt::Display;\n";
        let tree = parser.parse(top, None).expect("parse");
        let node = find_use_declaration(tree.root_node()).expect("use present");
        assert!(is_module_item_use(node), "file-top use is a module item");

        // `use` inside a `mod m { ... }` body — a child of `declaration_list`.
        let in_mod = "mod m { use std::fmt::Display; }\n";
        let tree = parser.parse(in_mod, None).expect("parse");
        let node = find_use_declaration(tree.root_node()).expect("use present");
        assert!(
            is_module_item_use(node),
            "use inside a mod body is a module item"
        );

        // Block-local `use` inside a function body — a child of `block`.
        let in_fn = "fn helper() { use std::fmt::Display; let _ = 0; }\n";
        let tree = parser.parse(in_fn, None).expect("parse");
        let node = find_use_declaration(tree.root_node()).expect("use present");
        assert!(
            !is_module_item_use(node),
            "block-local use is NOT a module item"
        );
    }

    #[test]
    fn ast_impl_decision_resolves_generic_and_plain_trait_impls() {
        // Generic binder, non-parameter RHS: resolve the bare trait name.
        assert_eq!(
            ast_resolve("impl<T> GenT for Wrapper<T> {}"),
            Some("GenT".to_owned())
        );
        // Trait-segment generic args are stripped via the AST `type_arguments`.
        assert_eq!(
            ast_resolve("impl GenP<u32> for Plain {}"),
            Some("GenP".to_owned())
        );
        // Qualified generic trait path keeps the scoped path, drops the args.
        assert_eq!(
            ast_resolve("impl<T> foo::Bar<T> for Wrapper<T> {}"),
            Some("foo::Bar".to_owned())
        );
        assert_eq!(
            ast_resolve("impl<T> crate::Target<u32> for Wrapper<T> {}"),
            Some("crate::Target".to_owned())
        );
        // Non-generic trait impl on a concrete instantiation.
        assert_eq!(
            ast_resolve("impl MyTrait for MyStruct<i32> {}"),
            Some("MyTrait".to_owned())
        );
        // `unsafe` keyword prefix is transparent (a child token, not a field).
        assert_eq!(
            ast_resolve("unsafe impl<T> GenT for Wrapper<T> {}"),
            Some("GenT".to_owned())
        );
        // Multi-bound binder: the trait still resolves.
        assert_eq!(
            ast_resolve("impl<T: Into<String>> GenT for Wrapper<T> {}"),
            Some("GenT".to_owned())
        );
        // A qualified path WITHOUT turbofish keeps every `::`.
        assert_eq!(
            ast_resolve("impl crate::T for Foo {}"),
            Some("crate::T".to_owned())
        );
    }

    #[test]
    fn ast_impl_decision_resolves_arrow_bound_binder() {
        // The regression case: a function-trait bound with a return arrow in
        // the binder. The Tree-sitter `type_parameters` field bounds the binder
        // structurally, so the `->` never leaks into the trait segment the way
        // the pre-AST char-scan binder split did (it closed depth on the `>` of
        // `->`, leaving `u32> Target ...`).
        assert_eq!(
            ast_resolve("impl<T: Fn() -> u32> Target for Wrapper<T> {}"),
            Some("Target".to_owned())
        );
        assert_eq!(
            ast_resolve("impl<T: Fn(u8) -> u32> crate::Target<u32> for Wrapper<T> {}"),
            Some("crate::Target".to_owned())
        );
        // A binder param used bare with an arrow bound is still a blanket impl.
        assert_eq!(
            ast_decision("impl<T: Fn() -> u32> Blanket for T {}"),
            ImplTargetDecision::NoEdge
        );
    }

    #[test]
    fn ast_impl_decision_resolves_turbofish_via_recovery() {
        // Turbofish `::<>` in trait position is invalid Rust; Tree-sitter emits
        // an ERROR node, so `impl_target_decision` recovers through the
        // header-string normalizer and still resolves the bare trait.
        assert_eq!(
            ast_resolve("impl GenP::<u32> for Plain {}"),
            Some("GenP".to_owned())
        );
        assert_eq!(
            ast_resolve("impl some::path::GenP::<u32> for Plain {}"),
            Some("some::path::GenP".to_owned())
        );
        assert_eq!(
            ast_resolve("impl crate::GenP::<u32> for Foo {}"),
            Some("crate::GenP".to_owned())
        );
    }

    #[test]
    fn ast_impl_decision_handles_spaced_binder() {
        // A source-level space between `impl` and the `<T>` binder parses into
        // the same fields, so the trait segment resolves natively.
        assert_eq!(
            ast_resolve("impl <T> GenT for Wrapper<T> {}"),
            Some("GenT".to_owned())
        );
        assert_eq!(
            ast_decision("impl <T> Blanket for T {}"),
            ImplTargetDecision::NoEdge
        );
        assert_eq!(
            ast_decision("impl <T> MyStruct<T> {}"),
            ImplTargetDecision::Verbatim
        );
    }

    #[test]
    fn ast_impl_decision_bounds_out_blanket_and_non_nominal() {
        // Bare binder-parameter `for` target: no edge.
        for header in [
            "impl<T> Blanket for T {}",
            "impl<'a, T> Blanket for T {}",
            "impl<T: Clone> Blanket for T {}",
        ] {
            assert_eq!(ast_decision(header), ImplTargetDecision::NoEdge, "{header}");
        }
        // Reference/pointer wrappers around a bare binder param: still blanket.
        for header in [
            "impl<T> Blanket for &T {}",
            "impl<T> Blanket for &mut T {}",
            "impl<'a, T> Blanket for &'a T {}",
            "impl<'a, T> Blanket for &'a mut T {}",
            "impl<T> Blanket for *const T {}",
            "impl<T> Blanket for *mut T {}",
            "impl<T> Blanket for &&T {}",
            "impl<T> Blanket for &*const T {}",
        ] {
            assert_eq!(ast_decision(header), ImplTargetDecision::NoEdge, "{header}");
        }
        // Non-nominal targets (slice, array, tuple, trait object, opaque type).
        for header in [
            "impl<T> Blanket for [T] {}",
            "impl<T> Blanket for [T; 4] {}",
            "impl<T> Blanket for (T, T) {}",
            "impl<T> Blanket for &[T] {}",
            "impl Blanket for dyn Other {}",
            "impl Blanket for impl Other {}",
        ] {
            assert_eq!(ast_decision(header), ImplTargetDecision::NoEdge, "{header}");
        }
        // A concrete type sharing the param spelling but carrying generics, or a
        // reference to a concrete nominal type, still resolves.
        assert_eq!(
            ast_resolve("impl<T> Blanket for Wrapper<T> {}"),
            Some("Blanket".to_owned())
        );
        assert_eq!(
            ast_resolve("impl<T> Blanket for &Wrapper<T> {}"),
            Some("Blanket".to_owned())
        );
    }

    #[test]
    fn ast_impl_decision_negative_impl_mints_no_edge() {
        // A negative impl asserts the type does NOT implement the trait; the `!`
        // token lives outside the `trait` field, so reading the field alone
        // would wrongly resolve `LocalAuto`. It must mint no edge.
        assert_eq!(
            ast_decision("impl !LocalAuto for Foo {}"),
            ImplTargetDecision::NoEdge
        );
        assert_eq!(
            ast_decision("impl<T> !LocalAuto for Wrapper<T> {}"),
            ImplTargetDecision::NoEdge
        );
        assert_eq!(
            ast_decision("unsafe impl !Send for Foo {}"),
            ImplTargetDecision::NoEdge
        );
    }

    #[test]
    fn ast_impl_decision_preserves_inherent_impls() {
        // Generic inherent impl (no `for`): verbatim self edge preserved.
        assert_eq!(
            ast_decision("impl<T> MyStruct<T> {}"),
            ImplTargetDecision::Verbatim
        );
        // Non-generic inherent impl: resolve the type name verbatim.
        assert_eq!(ast_resolve("impl Plain {}"), Some("Plain".to_owned()));
        assert_eq!(
            ast_resolve("impl MyStruct<i32> {}"),
            Some("MyStruct<i32>".to_owned())
        );
    }

    #[test]
    fn impl_trait_target_resolves_generic_trait_impls() {
        // Generic binder, non-parameter RHS: resolve the bare trait name.
        assert_eq!(
            resolve_target("impl<T> GenT for Wrapper<T>"),
            Some("GenT".to_owned())
        );
        // Trait-segment generic args are stripped.
        assert_eq!(
            resolve_target("impl GenP<u32> for Plain"),
            Some("GenP".to_owned())
        );
        // Qualified generic trait path keeps the qualifier, drops the args.
        assert_eq!(
            resolve_target("impl<T> foo::Bar<T> for Wrapper<T>"),
            Some("foo::Bar".to_owned())
        );
        // Non-generic trait impls are unchanged.
        assert_eq!(
            resolve_target("impl MyTrait for MyStruct<i32>"),
            Some("MyTrait".to_owned())
        );
        // `unsafe` keyword prefix is transparent.
        assert_eq!(
            resolve_target("unsafe impl<T> GenT for Wrapper<T>"),
            Some("GenT".to_owned())
        );
        // Multi-bound binder: still resolves the trait, params parsed past
        // the bounds.
        assert_eq!(
            resolve_target("impl<T: Into<String>> GenT for Wrapper<T>"),
            Some("GenT".to_owned())
        );
    }

    #[test]
    fn impl_trait_target_resolves_turbofish_trait_impls() {
        // Turbofish trait syntax `GenP::<u32>` is valid Rust in type position.
        // After stripping the generic args, the trailing `::` separator must
        // also be dropped so the bare trait name matches the `GenP` symbol.
        assert_eq!(
            resolve_target("impl GenP::<u32> for Plain"),
            Some("GenP".to_owned())
        );
        // Turbofish on a qualified path strips the args and the trailing `::`
        // while preserving the internal path separators.
        assert_eq!(
            resolve_target("impl some::path::GenP::<u32> for Plain"),
            Some("some::path::GenP".to_owned())
        );
        // A qualified path WITHOUT turbofish keeps every `::` — only a
        // trailing separator left by turbofish stripping is removed.
        assert_eq!(
            resolve_target("impl crate::T::<u32> for Foo"),
            Some("crate::T".to_owned())
        );
        assert_eq!(
            resolve_target("impl crate::T for Foo"),
            Some("crate::T".to_owned())
        );
        // Spaced turbofish (valid Rust, survives `impl_display` as `GenP ::
        // <u32>`) normalizes the same way.
        assert_eq!(
            resolve_target("impl GenP :: <u32> for Plain"),
            Some("GenP".to_owned())
        );
        // Generic binder with a turbofish trait: bare trait still resolves.
        assert_eq!(
            resolve_target("impl<T> GenT::<T> for Wrapper<T>"),
            Some("GenT".to_owned())
        );
    }

    #[test]
    fn impl_trait_target_resolves_spaced_generic_binder() {
        // A source-level space between `impl` and the `<T>` binder is valid
        // Rust and survives `impl_display` normalization as `impl <T> ...`.
        // The binder must still be skipped so the trait segment resolves.
        assert_eq!(
            resolve_target("impl <T> GenT for Wrapper<T>"),
            Some("GenT".to_owned())
        );
        // Spaced blanket impl is still bounded out.
        assert!(matches!(
            impl_trait_target("impl <T> Blanket for T"),
            ImplTargetDecision::NoEdge
        ));
        // Spaced generic inherent impl keeps its verbatim self edge.
        assert!(matches!(
            impl_trait_target("impl <T> MyStruct<T>"),
            ImplTargetDecision::Verbatim
        ));
        // `unsafe` + spaced binder is transparent too.
        assert_eq!(
            resolve_target("unsafe impl <T> GenT for Wrapper<T>"),
            Some("GenT".to_owned())
        );
    }

    #[test]
    fn impl_trait_target_bounds_out_blanket_impls() {
        // `for` target is a bare binder parameter: no edge.
        assert!(matches!(
            impl_trait_target("impl<T> Blanket for T"),
            ImplTargetDecision::NoEdge
        ));
        assert!(matches!(
            impl_trait_target("impl<'a, T> Blanket for T"),
            ImplTargetDecision::NoEdge
        ));
        // A bounded binder param used bare is still blanket.
        assert!(matches!(
            impl_trait_target("impl<T: Clone> Blanket for T"),
            ImplTargetDecision::NoEdge
        ));
        // A concrete type sharing the param's spelling but carrying generics
        // is NOT a bare param -> still resolves.
        assert_eq!(
            resolve_target("impl<T> Blanket for Wrapper<T>"),
            Some("Blanket".to_owned())
        );
    }

    #[test]
    fn impl_trait_target_bounds_out_reference_blanket_impls() {
        // A reference/pointer around a bare binder parameter is still a
        // blanket impl: strip the sigils and lifetimes, recognize the binder
        // param, mint no edge. Before the fix the `&` broke the bare-`T`
        // filter and the header fell through to `Resolve`, fabricating an
        // IMPLEMENTS edge with no concrete implementing-type record.
        for header in [
            "impl<T> Blanket for &T",
            "impl<T> Blanket for &mut T",
            "impl<'a, T> Blanket for &'a T",
            "impl<'a, T> Blanket for &'a mut T",
            "impl<T> Blanket for *const T",
            "impl<T> Blanket for *mut T",
            // Combined / repeated sigils still reduce to the binder param.
            "impl<T> Blanket for &&T",
            "impl<T> Blanket for &*const T",
        ] {
            assert!(
                matches!(impl_trait_target(header), ImplTargetDecision::NoEdge),
                "{header} must be bounded out"
            );
        }
        // Non-nominal `for` targets (slice, tuple, trait object, opaque type)
        // name no local nominal type and mint no edge either.
        for header in [
            "impl<T> Blanket for [T]",
            "impl<T> Blanket for (T, T)",
            "impl<T> Blanket for &[T]",
            "impl Blanket for dyn Other",
            "impl Blanket for impl Other",
        ] {
            assert!(
                matches!(impl_trait_target(header), ImplTargetDecision::NoEdge),
                "{header} must be bounded out"
            );
        }
        // A reference to a CONCRETE nominal type is not a binder-param blanket
        // impl -> still resolves the trait (implementing type `&Wrapper`).
        assert_eq!(
            resolve_target("impl<T> Blanket for &Wrapper<T>"),
            Some("Blanket".to_owned())
        );
    }

    #[test]
    fn impl_trait_target_preserves_inherent_impls() {
        // Generic inherent impl (no `for`): verbatim self edge preserved.
        assert!(matches!(
            impl_trait_target("impl<T> MyStruct<T>"),
            ImplTargetDecision::Verbatim
        ));
        // Non-generic inherent impl: resolve the type name verbatim, exactly
        // as the pre-#343 path did.
        assert_eq!(resolve_target("impl Plain"), Some("Plain".to_owned()));
    }

    #[test]
    fn test_doc_attribute_text_extracts_string_forms() {
        assert_eq!(
            doc_attribute_text(r#"#[doc = "Plain doc."]"#),
            Some("Plain doc.".to_owned())
        );
        assert_eq!(
            doc_attribute_text(r##"#[doc = r#"Raw doc."#]"##),
            Some("Raw doc.".to_owned())
        );
        assert_eq!(
            doc_attribute_text(r#"#[doc="escaped \"quote\" and\nnewline"]"#),
            Some("escaped \"quote\" and\nnewline".to_owned())
        );
    }

    #[test]
    fn test_string_literal_text_decodes_all_escape_forms() {
        assert_eq!(
            string_literal_text(r#""caf\u{e9}""#),
            Some("café".to_owned())
        );
        assert_eq!(string_literal_text(r#""\x41B""#), Some("AB".to_owned()));
        assert_eq!(
            string_literal_text(r#""a\rb\0c\'d\"e""#),
            Some("a\rb\0c'd\"e".to_owned())
        );
        // Line continuation: `\` before a newline swallows the newline and
        // the next line's leading whitespace.
        assert_eq!(
            string_literal_text("\"one \\\n    two\""),
            Some("one two".to_owned())
        );
    }

    #[test]
    fn test_string_literal_text_keeps_undecodable_escapes_raw() {
        // Never partially decode: an escape this decoder does not understand
        // keeps the literal content verbatim instead of dropping backslashes.
        assert_eq!(string_literal_text(r#""caf\q""#), Some(r"caf\q".to_owned()));
        assert_eq!(
            string_literal_text(r#""bad \u{ZZ} escape""#),
            Some(r"bad \u{ZZ} escape".to_owned())
        );
    }

    #[test]
    fn test_doc_attribute_text_decodes_unicode_escapes() {
        assert_eq!(
            doc_attribute_text(r#"#[doc = "caf\u{e9}"]"#),
            Some("café".to_owned())
        );
    }

    #[test]
    fn test_doc_attribute_text_rejects_non_doc_shapes() {
        assert_eq!(doc_attribute_text("#[doc(hidden)]"), None);
        assert_eq!(doc_attribute_text(r#"#[doc(alias = "other")]"#), None);
        assert_eq!(doc_attribute_text("#[derive(Debug)]"), None);
        assert_eq!(doc_attribute_text(r#"#[deprecated = "note"]"#), None);
    }

    #[test]
    fn test_doc_attribute_text_marks_unexpanded_expressions_as_present() {
        // Rustdoc documents an item carrying `#[doc = <expr>]` even when the
        // expression needs macro expansion; the fact recorded is presence
        // with a labeled unexpanded marker, never guessed doc text.
        let included = doc_attribute_text(r#"#[doc = include_str!("../README.md")]"#)
            .expect("include_str! doc attribute must count as documentation");
        assert!(
            included.contains(r#"include_str!("../README.md")"#),
            "marker must cite the unexpanded expression, got {included:?}"
        );
        let concatenated = doc_attribute_text(r#"#[doc = concat!("a", "b")]"#)
            .expect("concat! doc attribute must count as documentation");
        assert!(
            concatenated.contains("concat!"),
            "marker must cite the unexpanded expression, got {concatenated:?}"
        );
    }

    #[test]
    fn test_normalize_raw_strings() {
        // Raw strings should preserve their contents, including comments-like delimiters
        let code = r###"
            let a = r"hello // world";
            let b = r#"foo /* bar */ baz"#;
            let c = r##"nested "quotes" and // comments"##;
        "###;
        let normalized = normalize_code(code);
        assert!(normalized.contains("hello // world"), "Got: {normalized}");
        assert!(
            normalized.contains("foo /* bar */ baz"),
            "Got: {normalized}"
        );
        assert!(
            normalized.contains("nested \"quotes\" and // comments"),
            "Got: {normalized}"
        );
    }

    #[test]
    fn test_normalize_block_comments_preserves_separator() {
        // Block comment stripping should preserve a separator space to avoid token concatenation
        let code = "let x = 1/* comment */+2;";
        let normalized = normalize_code(code);
        assert_eq!(normalized, "let x=1+2;");

        let code2 = "let x = 1 /* comment */ +2;";
        let normalized2 = normalize_code(code2);
        assert_eq!(normalized2, "let x=1+2;");
    }

    #[test]
    fn test_normalize_nested_block_comments() {
        let code = "let x = 1 /* outer /* inner */ changed */ + 2;";
        let normalized = normalize_code(code);
        assert_eq!(normalized, "let x=1+2;");
    }

    #[test]
    fn test_strip_comments_before_sorting_imports() {
        let code = "
            // use old::path;
            use new::path;
        ";
        let normalized = normalize_file_code(code);
        assert_eq!(normalized, "use new::path;");
    }

    #[test]
    fn test_preserve_scoped_imports() {
        let code = "
            use top::level;
            fn foo() {
                use inner::scoped;
            }
        ";
        let normalized = normalize_file_code(code);
        assert!(normalized.contains("use top::level;"), "Got: {normalized}");
        assert!(
            normalized.contains("use inner::scoped;"),
            "Got: {normalized}"
        );
    }

    #[test]
    fn test_normalize_punctuation_adjacent_whitespace() {
        let code1 = "fn f(a: i32) -> i32 { a + 1 }";
        let code2 = "fn f(a:i32)->i32{a+1}";
        let normalized1 = normalize_code(code1);
        let normalized2 = normalize_code(code2);
        assert_eq!(normalized1, normalized2);
        assert_eq!(normalized1, "fn f(a:i32)->i32{a+1}");
    }

    #[test]
    fn test_normalize_file_code_raw_strings() {
        let code = r##"
            let s = r#"x"#; // comment
        "##;
        let normalized = normalize_file_code(code);
        assert_eq!(normalized, "let s=r#\"x\"#;");
    }

    #[test]
    fn test_normalize_file_code_visibility() {
        let code = "
            pub use b::Y;
            use a::X;
            pub(crate) use c::Z;
        ";
        let normalized = normalize_file_code(code);
        // The imports should sort as:
        // pub use b::Y;
        // pub(crate) use c::Z;
        // use a::X;
        // (after normalization, all spacing is stripped/collapsed)
        assert_eq!(normalized, "pub use b::Y;pub(crate)use c::Z;use a::X;");
    }

    #[test]
    fn test_normalize_file_code_abuse() {
        let code = "pub const abuse: i32 = 1;";
        let normalized = normalize_file_code(code);
        assert_eq!(normalized, "pub const abuse:i32=1;");
    }

    // ── Debt-marker comment scanning (issue #218) ─────────────────────────────

    fn categories_and_notes(text: &str) -> Vec<(&'static str, String)> {
        comment_debt_markers(text)
            .into_iter()
            .map(|m| (m.category, m.note))
            .collect()
    }

    #[test]
    fn debt_marker_scan_matches_line_comment_markers() {
        assert_eq!(
            categories_and_notes("// TODO: wire retry logic"),
            vec![("todo", "wire retry logic".to_owned())]
        );
        assert_eq!(
            categories_and_notes("// FIXME - handle empty input"),
            vec![("fixme", "handle empty input".to_owned())]
        );
        assert_eq!(
            categories_and_notes("// HACK bypasses cache"),
            vec![("hack", "bypasses cache".to_owned())]
        );
    }

    #[test]
    fn debt_marker_scan_is_case_insensitive_on_the_token_only() {
        assert_eq!(
            categories_and_notes("// todo lowercase works"),
            vec![("todo", "lowercase works".to_owned())]
        );
        assert_eq!(
            categories_and_notes("// xXx MiXeD"),
            vec![("xxx", "MiXeD".to_owned())],
            "the note text keeps its original case"
        );
    }

    #[test]
    fn debt_marker_scan_never_matches_identifier_substrings() {
        assert_eq!(categories_and_notes("// TODOIST integration"), vec![]);
        assert_eq!(categories_and_notes("// call fixmeup() next"), vec![]);
        assert_eq!(categories_and_notes("// sizes XXXL and up"), vec![]);
        assert_eq!(categories_and_notes("// TODO2 is not a marker"), vec![]);
        assert_eq!(categories_and_notes("// TODO_LIST const"), vec![]);
        assert_eq!(categories_and_notes("// nothing to see here"), vec![]);
    }

    #[test]
    fn debt_marker_scan_strips_block_terminator_and_stays_single_line() {
        assert_eq!(
            categories_and_notes("/* FIXME handle empty input */"),
            vec![("fixme", "handle empty input".to_owned())]
        );
        assert_eq!(
            categories_and_notes("/* first line\n * TODO: second line note\n */"),
            vec![("todo", "second line note".to_owned())],
            "a multi-line block note is cut at the end of the marker's line"
        );
    }

    #[test]
    fn debt_marker_scan_captures_raw_note_and_empty_notes() {
        // Structured metadata is out of scope: the raw text is kept verbatim.
        assert_eq!(
            categories_and_notes("// TODO(alice): assign later"),
            vec![("todo", "(alice): assign later".to_owned())]
        );
        assert_eq!(
            categories_and_notes("// TODO"),
            vec![("todo", String::new())],
            "a bare marker keeps an empty note"
        );
    }

    #[test]
    fn debt_marker_scan_returns_one_marker_per_occurrence_in_source_order() {
        assert_eq!(
            categories_and_notes("// TODO: fix\n// FIXME: later"),
            vec![("todo", "fix".to_owned()), ("fixme", "later".to_owned()),]
        );
        let markers = comment_debt_markers("/* line one\n TODO: on line two */");
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].line_offset, 1, "line offset counts newlines");
        assert!(markers[0].token_start > 0);
        assert!(markers[0].note_end > markers[0].token_start);
    }

    #[test]
    fn debt_marker_extraction_skips_string_literals_via_tree_sitter() {
        let source = "pub fn f() -> &'static str {\n    // TODO: real marker\n    \"TODO: not a marker\"\n}\n";
        let file = SourceFile {
            path: PathBuf::from("src/lib.rs"),
            repo_relative_path: "src/lib.rs".to_owned(),
        };
        let mut graph = Graph::default();
        extract_file_source(&file, source, "file-id", "repo-id", &mut graph)
            .expect("source should parse");
        let markers: Vec<&GraphRecord> = graph
            .records()
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    GraphRecord::Node {
                        kind: NodeKind::DebtMarker,
                        ..
                    }
                )
            })
            .collect();
        assert_eq!(
            markers.len(),
            1,
            "only the comment marker may match; the string literal never does"
        );
        assert_eq!(markers[0].note(), Some("real marker"));
    }

    #[test]
    fn symbol_spans_carry_tree_sitter_columns() {
        // Issue #463: every syntax-derived symbol span records zero-based
        // byte-offset columns, so the SCIP exporter can emit precise ranges
        // without re-reading source. Columns must agree with the byte offsets
        // against the source's newline positions.
        let source = "fn top() {}\n    fn indented() {}\n";
        let graph = extract_records(source);
        let mut spans: Vec<SourceSpan> = graph
            .records()
            .iter()
            .filter_map(|record| match record {
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    span: Some(span),
                    ..
                } => Some(*span),
                _ => None,
            })
            .collect();
        assert_eq!(spans.len(), 2, "two function symbols expected");
        spans.sort_by_key(|span| span.start_byte);
        for span in &spans {
            assert!(
                span.start_column.is_some() && span.end_column.is_some(),
                "extractor must record columns: {span:?}"
            );
        }
        // The indented declaration starts four bytes into its line.
        let indented = spans
            .iter()
            .find(|span| span.start_line == 2)
            .expect("line-2 symbol");
        assert_eq!(indented.start_column, Some(4));

        let mut line_starts = vec![0usize];
        for (index, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(index + 1);
            }
        }
        for span in &spans {
            let start_line_start = line_starts[span.start_line - 1];
            let end_line_start = line_starts[span.end_line - 1];
            assert_eq!(
                span.start_column,
                Some(span.start_byte - start_line_start),
                "start column is the byte offset from line start"
            );
            assert_eq!(
                span.end_column,
                Some(span.end_byte - end_line_start),
                "end column is the byte offset from line start"
            );
        }
    }

    /// Extracts `source` and returns the graph records (issue #206 helpers).
    fn extract_records(source: &str) -> Graph {
        let file = SourceFile {
            path: PathBuf::from("src/lib.rs"),
            repo_relative_path: "src/lib.rs".to_owned(),
        };
        let mut graph = Graph::default();
        extract_file_source(&file, source, "file-id", "repo-id", &mut graph)
            .expect("source should parse");
        graph
    }

    fn find_node(graph: &Graph, want: NodeKind) -> &GraphRecord {
        graph
            .records()
            .iter()
            .find(|r| matches!(r, GraphRecord::Node { kind, .. } if *kind == want))
            .expect("node of requested kind present")
    }

    #[test]
    fn module_carries_deterministic_content_signature() {
        // A module summary is name-only (issue #206) and its stable ID is keyed
        // on the qualified NAME alone, so an inline-body edit keeps the same ID.
        // The extractor stamps a compact BLAKE3 body signature so that
        // same-ID body drift stays content-detectable. It must be present,
        // well-formed, and byte-stable across identical extractions. Imports
        // carry NO signature (their ID already encodes the full declaration).
        let source = "\
use std::collections::BTreeMap;

pub mod inner {
    pub fn helper() -> u32 {
        1
    }
}
";
        let graph = extract_records(source);
        let module = find_node(&graph, NodeKind::Module);
        let import = find_node(&graph, NodeKind::Import);

        let module_sig = module
            .content_signature()
            .expect("module content_signature");
        assert!(module_sig.starts_with("blake3:"));
        // Imports no longer carry a content signature.
        assert_eq!(import.content_signature(), None);

        // Deterministic: re-extracting identical source yields identical handles.
        let graph2 = extract_records(source);
        assert_eq!(
            find_node(&graph2, NodeKind::Module).content_signature(),
            Some(module_sig)
        );
        assert_eq!(
            find_node(&graph2, NodeKind::Import).content_signature(),
            None
        );
    }

    #[test]
    fn content_signature_changes_with_module_body() {
        // A module body edit that leaves the name unchanged keeps the module's
        // stable ID (keyed on qualified name only) but must produce a different
        // content signature — exactly what makes the module body drift
        // detectable as a ContentChange within one ID group (issue #206).
        let base = "\
pub mod inner {
    pub fn helper() -> u32 {
        1
    }
}
";
        let changed_mod = "\
pub mod inner {
    pub fn helper() -> u32 {
        2
    }
}
";
        let base_graph = extract_records(base);
        let base_mod = find_node(&base_graph, NodeKind::Module)
            .content_signature()
            .expect("sig")
            .to_owned();

        let mod_graph = extract_records(changed_mod);
        assert_ne!(
            find_node(&mod_graph, NodeKind::Module).content_signature(),
            Some(base_mod.as_str()),
            "changed module body must change the signature"
        );
    }

    #[test]
    fn import_body_change_is_a_record_identity_change() {
        // An import body change (here adding an `as` alias) mints a DIFFERENT
        // stable record ID, because `import_name` — a hash component of the ID —
        // is the whole trimmed `use ...;` declaration, not the bound leaf. So
        // the two versions never share an ID for evidence-freshness to compare a
        // signature within; the change surfaces as a handle-identity change
        // (`unresolved`/removed), never a ContentChange drift (issue #206).
        let base = "use std::collections::BTreeMap;\n";
        let changed = "use std::collections::BTreeMap as Map;\n";
        let base_graph = extract_records(base);
        let changed_graph = extract_records(changed);
        let base_import = find_node(&base_graph, NodeKind::Import).id();
        let changed_import = find_node(&changed_graph, NodeKind::Import).id();
        assert_ne!(
            base_import, changed_import,
            "an import body change must mint a different record ID"
        );
    }

    #[test]
    fn only_module_nodes_carry_content_signature() {
        // content_signature is scoped to Module (issue #206). Symbols embed the
        // normalized body in their own summary, imports encode the full
        // declaration in their ID, and the File node is minted upstream; none of
        // them carry a signature, so their content hash stays byte-unchanged.
        let source = "use std::collections::BTreeMap;\n\npub fn f() -> u32 {\n    1\n}\n";
        let graph = extract_records(source);
        assert_eq!(
            find_node(&graph, NodeKind::Symbol).content_signature(),
            None
        );
        assert_eq!(
            find_node(&graph, NodeKind::Import).content_signature(),
            None
        );
    }

    // ── Trait-dispatch extraction (issue #267) ───────────────────────────

    /// Extracts `source` and returns the `(caller_simple_name,
    /// dispatch_trait)` pairs stamped on method call sites, in source order.
    fn dispatch_traits_in(source: &str) -> Vec<(String, Option<String>)> {
        let file = SourceFile {
            path: PathBuf::from("src/lib.rs"),
            repo_relative_path: "src/lib.rs".to_owned(),
        };
        let mut graph = Graph::default();
        let facts = extract_file_source(&file, source, "file-id", "repo-id", &mut graph)
            .expect("source should parse");
        let by_id: std::collections::BTreeMap<&str, &str> = facts
            .definitions
            .iter()
            .map(|d| (d.id.as_str(), d.simple_name.as_str()))
            .collect();
        facts
            .call_sites
            .iter()
            .filter(|call| call.call_kind == CallKind::Method)
            .map(|call| {
                (
                    by_id
                        .get(call.caller_id.as_str())
                        .unwrap_or(&"?")
                        .to_string(),
                    call.dispatch_trait.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn dyn_receiver_stamps_the_dispatch_trait() {
        let stamped = dispatch_traits_in("fn draw(item: &dyn Renderable) { item.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), Some("Renderable".to_owned()))],
            "a &dyn Trait receiver must stamp its trait on the method call"
        );
    }

    #[test]
    fn boxed_dyn_receiver_stamps_the_dispatch_trait() {
        let stamped = dispatch_traits_in("fn draw(item: Box<dyn Renderable>) { item.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), Some("Renderable".to_owned()))],
            "a Box<dyn Trait> receiver must stamp its trait on the method call"
        );
    }

    #[test]
    fn generic_bound_receiver_stamps_the_dispatch_trait() {
        let stamped = dispatch_traits_in("fn draw<T: Renderable>(item: T) { item.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), Some("Renderable".to_owned()))],
            "a T: Trait bound receiver must stamp its trait on the method call"
        );
    }

    #[test]
    fn where_clause_bound_receiver_stamps_the_dispatch_trait() {
        let stamped =
            dispatch_traits_in("fn draw<T>(item: T) where T: Renderable { item.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), Some("Renderable".to_owned()))],
            "a where-clause bound receiver must stamp its trait on the method call"
        );
    }

    #[test]
    fn let_dyn_ascription_stamps_the_dispatch_trait() {
        let stamped = dispatch_traits_in(
            "fn draw() { let item: &dyn Renderable = make(); item.render(); }\n",
        );
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), Some("Renderable".to_owned()))],
            "a let-bound &dyn Trait receiver must stamp its trait on the method call"
        );
    }

    #[test]
    fn multi_bound_receiver_stamps_nothing() {
        // Two trait bounds: dispatch is ambiguous by construction, so the
        // call keeps the honest legacy fan-out instead of guessing.
        let stamped =
            dispatch_traits_in("fn draw<T: Renderable + Clone>(item: T) { item.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), None)],
            "a multi-bound receiver must not stamp any dispatch trait"
        );
    }

    #[test]
    fn conflicting_inline_and_where_bounds_stamp_nothing() {
        // One bound inline and a DIFFERENT bound in the where clause is two
        // distinct bounds: dispatch is ambiguous, so nothing is stamped.
        let stamped = dispatch_traits_in(
            "fn draw<T: Renderable>(item: T) where T: Clone { item.render(); }\n",
        );
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), None)],
            "conflicting inline/where bounds must not stamp any dispatch trait"
        );
    }

    #[test]
    fn repeated_same_bound_stamps_the_trait() {
        // The same bound written inline and in the where clause is still one
        // distinct bound.
        let stamped = dispatch_traits_in(
            "fn draw<T: Renderable>(item: T) where T: Renderable { item.render(); }\n",
        );
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), Some("Renderable".to_owned()))],
            "a repeated identical bound is still a single dispatch trait"
        );
    }

    #[test]
    fn concrete_receiver_stamps_no_dispatch_trait() {
        let stamped = dispatch_traits_in("fn draw(c: &Circle) { c.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), None)],
            "a concrete receiver stays on the receiver_type path (issue #441)"
        );
    }

    #[test]
    fn shadowed_dyn_binding_stamps_nothing() {
        // The second `item` shadows the first: the shadowing veto drops both
        // env entries, so the call carries no dispatch metadata.
        let stamped =
            dispatch_traits_in("fn draw(item: &dyn Renderable) { let item = 5; item.render(); }\n");
        assert_eq!(
            stamped,
            vec![("draw".to_owned(), None)],
            "a shadowed binding must not carry stale dispatch metadata"
        );
    }

    // ── Lint-suppression inventory (issue #227, RED) ────────────────────────

    /// Seeded fixture exercising every acceptance decoy: `#[allow(` inside a
    /// line comment, a doc comment, and a block comment; inside a string
    /// literal; an inner `#![allow]` at crate scope and at module scope; a
    /// multi-lint form; and adjacent justification comments (preceding for
    /// the module suppression, trailing for the item suppression).
    const LINT_SUPPRESSION_FIXTURE: &str = "#![allow(unused_imports)]\n\n// Line-comment decoy: #[allow(dead_code)] is never extracted.\n/// Doc-comment decoy: #[allow(dead_code)] is never extracted.\n/** Block-comment decoy: #[allow(dead_code)] is never extracted. */\n\nmod inner {\n    // Silence the noisy lint while the API settles.\n    #![allow(clippy::too_many_arguments, dead_code)]\n\n    // This string is not a suppression: \"#[allow(dead_code)]\".\n    const MARKER: &str = \"#[allow(dead_code)]\";\n\n    #[allow(unused_variables)] // trailing justification for the fn\n    fn helper() {}\n}\n";

    /// Extracts every `LintSuppression` payload in `source` as
    /// `(sorted lints, scope, has_justification, is_inner)`, in walk order.
    fn lint_suppressions_at(source: &str, path: &str) -> Vec<(Vec<String>, String, bool, bool)> {
        let file = SourceFile {
            path: PathBuf::from(path),
            repo_relative_path: path.to_owned(),
        };
        let mut graph = Graph::default();
        extract_file_source(&file, source, "file-id", "repo-id", &mut graph)
            .expect("source should parse");
        graph
            .records()
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Node {
                    kind: NodeKind::LintSuppression,
                    lint_suppression: Some(facts),
                    ..
                } => Some((
                    facts.lints.clone(),
                    facts.scope.as_str().to_owned(),
                    facts.has_justification,
                    facts.is_inner,
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn lint_suppression_fixture_yields_exactly_three_suppressions() {
        let rows = lint_suppressions_at(LINT_SUPPRESSION_FIXTURE, "src/lib.rs");
        assert_eq!(
            rows.len(),
            3,
            "comment, doc-comment, and string-literal decoys must never extract: {rows:?}"
        );
    }

    #[test]
    fn lint_suppression_crate_scope_without_justification() {
        let rows = lint_suppressions_at(LINT_SUPPRESSION_FIXTURE, "src/lib.rs");
        assert!(
            rows.contains(&(
                vec!["unused_imports".to_owned()],
                "crate".to_owned(),
                false,
                true
            )),
            "inner #![allow] at the crate root is crate scope: {rows:?}"
        );
    }

    #[test]
    fn lint_suppression_module_scope_multi_lint_sorted_with_justification() {
        let rows = lint_suppressions_at(LINT_SUPPRESSION_FIXTURE, "src/lib.rs");
        assert!(
            rows.contains(&(
                vec![
                    "clippy::too_many_arguments".to_owned(),
                    "dead_code".to_owned()
                ],
                "module".to_owned(),
                true,
                true
            )),
            "multi-lint forms sort and dedup; the preceding comment justifies: {rows:?}"
        );
    }

    #[test]
    fn lint_suppression_item_scope_trailing_comment_justifies() {
        let rows = lint_suppressions_at(LINT_SUPPRESSION_FIXTURE, "src/lib.rs");
        assert!(
            rows.contains(&(
                vec!["unused_variables".to_owned()],
                "item".to_owned(),
                true,
                false
            )),
            "outer #[allow] is item scope; the same-line comment justifies: {rows:?}"
        );
    }

    #[test]
    fn lint_suppression_dedups_and_sorts_lint_names() {
        let rows = lint_suppressions_at(
            "#[allow(dead_code, clippy::all, dead_code)]\nfn f() {}\n",
            "src/lib.rs",
        );
        assert_eq!(
            rows,
            vec![(
                vec!["clippy::all".to_owned(), "dead_code".to_owned()],
                "item".to_owned(),
                false,
                false
            )],
            "duplicate lint names collapse; rustc and clippy lints sort together"
        );
    }

    #[test]
    fn lint_suppression_ignores_cfg_attr_wrapped_allows() {
        let rows = lint_suppressions_at(
            "#[cfg_attr(test, allow(dead_code))]\n#[derive(Debug)]\nstruct S;\n",
            "src/lib.rs",
        );
        assert!(
            rows.is_empty(),
            "only the attribute's own name may match: {rows:?}"
        );
    }

    #[test]
    fn lint_suppression_bare_and_empty_allow_emit_nothing() {
        for source in ["#[allow]\nfn f() {}\n", "#[allow()]\nfn f() {}\n"] {
            let rows = lint_suppressions_at(source, "src/lib.rs");
            assert!(
                rows.is_empty(),
                "an allow silencing no lints is no suppression fact: {rows:?}"
            );
        }
    }

    #[test]
    fn lint_suppression_blank_line_breaks_comment_adjacency() {
        let rows = lint_suppressions_at(
            "// A comment with a blank line between it and the attribute.\n\n#[allow(dead_code)]\nfn f() {}\n",
            "src/lib.rs",
        );
        assert_eq!(
            rows,
            vec![(
                vec!["dead_code".to_owned()],
                "item".to_owned(),
                false,
                false
            )],
            "only an immediately adjacent comment is a justification signal"
        );
    }

    // ── Conditional-compilation gates (issue #190, RED) ────────────────────
    //
    // These tests assert the ABSENT feature: `GraphRecord::Node` carries no
    // `cfg` field on the base tree, so every assertion expecting a gate chain
    // FAILS until the implementation lands. They inspect serialized JSON, so
    // they compile against the base schema. (`cfg_attr` is deliberately absent
    // from the fixtures: its attribute decoding is issue #191's scope; issue
    // #190 records the controlling predicate once it decodes.)

    /// Fixture: a directly-gated fn, a fn inheriting its enclosing module's
    /// gate, and an ungated fn.
    const CFG_GATE_FIXTURE: &str = r#"#[cfg(feature = "embedded-aletheiadb")]
fn directly_gated() {}

#[cfg(feature = "mod-gate")]
mod gated_module {
    fn inherited_fn() {}
}

fn ungated_fn() {}
"#;

    /// Fixture: a file-inner gate, inherited by every symbol in the file.
    const CFG_FILE_INNER_FIXTURE: &str = r#"#![cfg(feature = "file-gate")]

fn file_scoped_fn() {}
"#;

    /// Extracts `source` as `path` and returns every record as serialized
    /// JSON, so the assertions compile against the base schema while probing
    /// for the `cfg` key the implementation adds.
    fn cfg_json_records(source: &str, path: &str) -> Vec<serde_json::Value> {
        let file = SourceFile {
            path: PathBuf::from(path),
            repo_relative_path: path.to_owned(),
        };
        let mut graph = Graph::default();
        extract_file_source(&file, source, "file-id", "repo-id", &mut graph)
            .expect("source should parse");
        graph
            .records()
            .iter()
            .map(|record| serde_json::to_value(record).expect("record serializes"))
            .collect()
    }

    /// Returns the `cfg` string chain for the record named `name`, or `None`
    /// when the record — or the `cfg` key — is absent. Matches the record's
    /// simple name or its qualified name (`mod_path::name`), since nested
    /// symbols are stamped with their qualified name.
    fn cfg_chain_for(records: &[serde_json::Value], name: &str) -> Option<Vec<String>> {
        let qualified_suffix = format!("::{name}");
        records
            .iter()
            .find(|value| {
                value
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|record_name| {
                        record_name == name || record_name.ends_with(qualified_suffix.as_str())
                    })
            })
            .and_then(|value| value.get("cfg"))
            .and_then(serde_json::Value::as_array)
            .map(|gates| {
                gates
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
    }

    /// Serializes one extraction of `source` to JSONL bytes.
    fn cfg_jsonl_bytes(source: &str, path: &str) -> Vec<u8> {
        let mut out = Vec::new();
        for record in cfg_json_records(source, path) {
            out.extend_from_slice(
                serde_json::to_string(&record)
                    .expect("record serializes")
                    .as_bytes(),
            );
            out.push(b'\n');
        }
        out
    }

    #[test]
    fn cfg_direct_gate_is_recorded_on_symbol() {
        let records = cfg_json_records(CFG_GATE_FIXTURE, "src/lib.rs");
        assert_eq!(
            cfg_chain_for(&records, "directly_gated"),
            Some(vec![r#"feature = "embedded-aletheiadb""#.to_owned()]),
            "a #[cfg(...)] gate is recorded verbatim on the symbol"
        );
    }

    #[test]
    fn cfg_module_gate_is_inherited_by_members() {
        let records = cfg_json_records(CFG_GATE_FIXTURE, "src/lib.rs");
        assert_eq!(
            cfg_chain_for(&records, "inherited_fn"),
            Some(vec![r#"feature = "mod-gate""#.to_owned()]),
            "a symbol inherits its enclosing module's gate"
        );
    }

    #[test]
    fn cfg_ungated_symbol_carries_no_cfg_key() {
        let records = cfg_json_records(CFG_GATE_FIXTURE, "src/lib.rs");
        let record = records
            .iter()
            .find(|value| {
                value.get("name").and_then(serde_json::Value::as_str) == Some("ungated_fn")
            })
            .expect("ungated_fn is extracted");
        assert!(
            record.get("cfg").is_none(),
            "an ungated symbol carries no cfg key at all: {record}"
        );
    }

    #[test]
    fn cfg_file_inner_gate_is_inherited_by_symbols() {
        let records = cfg_json_records(CFG_FILE_INNER_FIXTURE, "src/lib.rs");
        assert_eq!(
            cfg_chain_for(&records, "file_scoped_fn"),
            Some(vec![r#"feature = "file-gate""#.to_owned()]),
            "a #![cfg(...)] file-inner gate is inherited by the file's symbols"
        );
    }

    #[test]
    fn cfg_extraction_is_byte_stable_across_five_runs() {
        let baseline = cfg_jsonl_bytes(CFG_GATE_FIXTURE, "src/lib.rs");
        for _ in 1..5 {
            assert_eq!(
                cfg_jsonl_bytes(CFG_GATE_FIXTURE, "src/lib.rs"),
                baseline,
                "repeated extractions are byte-identical"
            );
        }
        // The stability assertion is vacuous without the feature: the chain
        // itself must be present.
        let records = cfg_json_records(CFG_GATE_FIXTURE, "src/lib.rs");
        assert!(
            cfg_chain_for(&records, "directly_gated").is_some(),
            "the stable output actually carries cfg gates"
        );
    }

    #[test]
    fn cfg_gates_survive_crlf_and_path_separators() {
        let lf = cfg_json_records(CFG_GATE_FIXTURE, "src/lib.rs");
        let crlf_source = CFG_GATE_FIXTURE.replace('\n', "\r\n");
        let crlf = cfg_json_records(&crlf_source, "src/lib.rs");
        assert_eq!(
            cfg_chain_for(&crlf, "directly_gated"),
            cfg_chain_for(&lf, "directly_gated"),
            "CRLF line endings yield the same gate chain as LF"
        );
        let backslash = cfg_json_records(CFG_GATE_FIXTURE, r"src\lib.rs");
        assert_eq!(
            cfg_chain_for(&backslash, "directly_gated"),
            cfg_chain_for(&lf, "directly_gated"),
            "backslash and slash paths yield the same gate chain"
        );
        assert!(
            cfg_chain_for(&lf, "directly_gated").is_some(),
            "the portable output actually carries cfg gates"
        );
    }

    // ── Conditional-compilation gates (issue #190, GREEN) ───────────────────
    //
    // Behavior tests for the implementation: `cfg_attr` controlling
    // predicates, comment/string decoys, multi-gate composition order, inline
    // module inner attributes, and per-ancestor attribute order.

    #[test]
    fn cfg_attr_controlling_predicate_is_recorded() {
        let records = cfg_json_records(
            "#[cfg_attr(feature = \"attr-gate\", allow(dead_code))]\nfn attr_fn() {}\n",
            "src/lib.rs",
        );
        assert_eq!(
            cfg_chain_for(&records, "attr_fn"),
            Some(vec![r#"feature = "attr-gate""#.to_owned()]),
            "cfg_attr records its controlling predicate, not the payload"
        );
    }

    #[test]
    fn cfg_comment_and_string_decoys_never_mint_gates() {
        let source = "// #[cfg(feature = \"comment-gate\")]\n/// #[cfg(feature = \"doc-gate\")]\nconst MARKER: &str = \"#[cfg(feature = \\\"string-gate\\\")]\";\nfn decoy_fn() {}\n";
        let records = cfg_json_records(source, "src/lib.rs");
        let record = records
            .iter()
            .find(|value| value.get("name").and_then(serde_json::Value::as_str) == Some("decoy_fn"))
            .expect("decoy_fn is extracted");
        assert!(
            record.get("cfg").is_none(),
            "decoys in comments and strings never mint gates: {record}"
        );
        for value in &records {
            if let Some(gates) = value.get("cfg").and_then(serde_json::Value::as_array) {
                for gate in gates {
                    let text = gate.as_str().unwrap_or_default();
                    assert!(
                        !text.contains("comment-gate")
                            && !text.contains("doc-gate")
                            && !text.contains("string-gate"),
                        "no decoy gate leaks into any record: {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn cfg_doubly_gated_fn_composes_outermost_first() {
        let records = cfg_json_records(
            "#[cfg(feature = \"outer\")]\nmod outer_mod {\n    #[cfg(feature = \"inner\")]\n    fn double_fn() {}\n}\n",
            "src/lib.rs",
        );
        assert_eq!(
            cfg_chain_for(&records, "double_fn"),
            Some(vec![
                r#"feature = "outer""#.to_owned(),
                r#"feature = "inner""#.to_owned()
            ]),
            "own gate composes after the inherited gate, outermost first"
        );
    }

    #[test]
    fn cfg_module_inner_attribute_gates_module_and_members() {
        let records = cfg_json_records(
            "mod inner_gated {\n    #![cfg(feature = \"inner-mod-gate\")]\n    fn member_fn() {}\n}\n",
            "src/lib.rs",
        );
        assert_eq!(
            cfg_chain_for(&records, "member_fn"),
            Some(vec![r#"feature = "inner-mod-gate""#.to_owned()]),
            "a member inherits its module's #![cfg] inner attribute"
        );
        assert_eq!(
            cfg_chain_for(&records, "inner_gated"),
            Some(vec![r#"feature = "inner-mod-gate""#.to_owned()]),
            "the module record carries its own #![cfg] inner attribute"
        );
    }

    #[test]
    fn cfg_ancestor_attribute_order_is_preserved() {
        // Regression: the ancestor walk must reverse ancestor order only —
        // reversing the flat vector scrambles each ancestor's own attribute
        // order.
        let records = cfg_json_records(
            "#[cfg(feature = \"a\")]\n#[cfg(feature = \"b\")]\nmod outer {\n    #[cfg(feature = \"c\")]\n    #[cfg(feature = \"d\")]\n    mod inner {\n        fn deep_fn() {}\n    }\n}\n",
            "src/lib.rs",
        );
        assert_eq!(
            cfg_chain_for(&records, "deep_fn"),
            Some(vec![
                r#"feature = "a""#.to_owned(),
                r#"feature = "b""#.to_owned(),
                r#"feature = "c""#.to_owned(),
                r#"feature = "d""#.to_owned(),
            ]),
            "each ancestor's attributes stay in source order, outermost first"
        );
    }

    #[test]
    fn cfg_inline_comments_are_stripped_from_predicates() {
        // AC8: the recorded gate is the predicate as written, minus comments —
        // comment text never reaches the output. A `//` inside a string
        // literal is predicate text, not a comment, and survives.
        let records = cfg_json_records(
            "#[cfg(feature = /* why gated */ \"comment-gate\")]\nfn block_commented_fn() {}\n\
             #[cfg(\n    feature = // line comment inside the predicate\n    \"line-gate\",\n)]\nfn line_commented_fn() {}\n\
             #[cfg(feature = \"slash//literal\")]\nfn slash_fn() {}\n",
            "src/lib.rs",
        );
        assert_eq!(
            cfg_chain_for(&records, "block_commented_fn"),
            Some(vec![r#"feature = "comment-gate""#.to_owned()]),
            "an inline block comment inside the predicate is stripped"
        );
        assert_eq!(
            cfg_chain_for(&records, "line_commented_fn"),
            Some(vec![r#"feature = "line-gate""#.to_owned()]),
            "an inline line comment inside the predicate is stripped"
        );
        assert_eq!(
            cfg_chain_for(&records, "slash_fn"),
            Some(vec![r#"feature = "slash//literal""#.to_owned()]),
            "// inside a string literal is predicate text, not a comment"
        );
    }

    // ── Issue #162: structural complexity counting rules ──────────────────
    //
    // RED: `control_flow_complexity` does not exist yet.

    /// Drives [`control_flow_complexity`] over a real parse of `source`,
    /// taking the first `function_item` — the same node kind the extractor
    /// hands to `add_symbol`.
    fn complexity_of_fn(source: &str) -> u32 {
        let tree = parse_tree(source);
        let item = first_descendant_of_kind(tree.root_node(), "function_item")
            .expect("source should contain a function_item");
        control_flow_complexity(item)
    }

    #[test]
    fn complexity_straight_line_scores_minimum() {
        assert_eq!(complexity_of_fn("fn f() -> i32 { 42 }"), 1);
        assert_eq!(complexity_of_fn("fn f(a: i32, b: i32) -> i32 { a + b }"), 1);
    }

    #[test]
    fn complexity_counts_branches_and_else_if() {
        assert_eq!(
            complexity_of_fn("fn f(x: bool) -> i32 { if x { 1 } else { 0 } }"),
            2
        );
        // `else if` is a second `if_expression` in the AST: a second decision.
        assert_eq!(
            complexity_of_fn(
                "fn f(x: i32) -> i32 { if x > 0 { 1 } else if x < 0 { 2 } else { 0 } }"
            ),
            3
        );
    }

    #[test]
    fn complexity_counts_loops() {
        assert_eq!(complexity_of_fn("fn f() { loop { break; } }"), 2);
        assert_eq!(complexity_of_fn("fn f(x: bool) { while x { break; } }"), 2);
        assert_eq!(complexity_of_fn("fn f() { for _ in 0..1 {} }"), 2);
        assert_eq!(
            complexity_of_fn("fn f() { for _ in 0..1 {} while true { break; } loop { break; } }"),
            4
        );
    }

    #[test]
    fn complexity_counts_every_match_arm() {
        // Three arms (including the `_` catch-all): 1 + 3 = 4.
        assert_eq!(
            complexity_of_fn("fn f(x: i32) -> i32 { match x { 0 => 1, 1..=5 => 2, _ => 3 } }"),
            4
        );
    }

    #[test]
    fn complexity_counts_try_and_short_circuit_operators() {
        assert_eq!(
            complexity_of_fn("fn f(x: Option<i32>) -> Option<i32> { Some(x?) }"),
            2
        );
        assert_eq!(
            complexity_of_fn("fn f(a: bool, b: bool) -> bool { a && b }"),
            2
        );
        assert_eq!(
            complexity_of_fn("fn f(a: bool, b: bool) -> bool { a || b }"),
            2
        );
        assert_eq!(
            complexity_of_fn("fn f(a: bool, b: bool, c: bool) -> bool { a && b || c }"),
            3
        );
    }

    #[test]
    fn complexity_counts_closure_bodies_but_not_nested_fns() {
        // Closure bodies are part of the enclosing function's expression
        // tree: the `if` inside the closure counts.
        assert_eq!(
            complexity_of_fn("fn f() -> i32 { let g = |x: i32| if x > 0 { x } else { -x }; g(1) }"),
            2
        );
        // A nested `fn` gets its own symbol: its `if` must not inflate the
        // outer score. Outer: 1 + 1 (its own `if`) = 2.
        let outer = complexity_of_fn(
            "fn outer() -> i32 { fn inner(x: bool) -> i32 { if x { 1 } else { 0 } } if true { inner(true) } else { 0 } }",
        );
        assert_eq!(outer, 2);
    }

    #[test]
    fn complexity_is_monotone_in_decision_points() {
        // Each snippet adds exactly one decision point over the previous:
        // the scores must be strictly increasing.
        let snippets = [
            "fn f() -> i32 { 0 }",
            "fn f(x: bool) -> i32 { if x { 1 } else { 0 } }",
            "fn f(x: bool) -> i32 { if x && x { 1 } else { 0 } }",
            "fn f(x: bool) -> i32 { if x && x || x { 1 } else { 0 } }",
            "fn f(x: bool) -> i32 { if x && x || x { if x { 1 } else { 0 } } else { 0 } }",
            "fn f(x: bool) -> i32 { while x { break; } if x && x || x { if x { 1 } else { 0 } } else { 0 } }",
            "fn f(x: bool) -> i32 { for _ in 0..1 {} while x { break; } if x && x || x { if x { 1 } else { 0 } } else { 0 } }",
        ];
        let mut previous = 0u32;
        for (i, snippet) in snippets.iter().enumerate() {
            let score = complexity_of_fn(snippet);
            let expected = u32::try_from(i + 1).expect("snippet index fits in u32");
            assert_eq!(score, expected, "snippet {i} should score {}", i + 1);
            assert!(score > previous, "snippet {i} must score strictly higher");
            previous = score;
        }
    }
}
