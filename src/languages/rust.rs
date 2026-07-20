//! Rust Tree-sitter extraction.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, SourceSpan, stable_id},
    languages::{
        common::{
            SymbolBody, add_graph_edge, emit_reference_edges, next_symbol_ordinal, node_name,
            path_segments, reference_text, span,
        },
        cross_file::{
            CallKind, CallPathRoot, CallSiteFact, ConstructSiteFact, DefinitionFact, FileFacts,
            ImplTargetFact, ImplTraitRelationFact, OutOfLineModFact, PendingImplFact,
            UseImportFact, crate_root_id,
        },
    },
    redaction::REDACTION_POLICY_VERSION,
};

/// Extracts Rust syntax records from one source file.
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
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| CodegraphError::ParserLanguage(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| CodegraphError::Parse {
            path: file.path.clone(),
        })?;

    let mut extractor = RustExtractor::new(file, file_id, repository_id, graph, source);
    extractor.walk(tree.root_node());
    extractor.resolve_pending_impl_edges();
    extractor.finalize_use_imports();
    extractor.emit_reference_edges();
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
    /// `true` when the whole file lives under a top-level `tests/` directory.
    file_in_tests_dir: bool,
    /// Per-function receiver-type environment (issue #441): a receiver binding
    /// identifier -> its reduced nominal type path, built from typed fn params
    /// and `let x: T` ascriptions whose binding is UNSHADOWED in the function
    /// body. Set on entering a function body (before its call sites are
    /// collected) and restored on exit, so `call_site_fact` can stamp a
    /// provable `receiver_type` on `x.method()` when `x` is in this map.
    type_env: BTreeMap<String, String>,
}

impl<'graph, 'source> RustExtractor<'graph, 'source> {
    fn new(
        file: &'source SourceFile,
        file_id: &'source str,
        repository_id: &'source str,
        graph: &'graph mut Graph,
        source: &'source str,
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
            inline_module_stack: Vec::new(),
            inline_path_override_depth: 0,
            test_scope_depth: 0,
            file_in_tests_dir: path_segments(&file.repo_relative_path)
                .first()
                .is_some_and(|segment| segment == "tests"),
            type_env: BTreeMap::new(),
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
        self.graph.push(
            GraphRecord::syntax_node(
                id.clone(),
                NodeKind::Module,
                self.file.repo_relative_path.clone(),
                span(node),
                qualified_name.clone(),
                "rust",
                format!("Rust module {qualified_name}"),
            )
            .with_declaration_surface(Some(self.symbol_visibility(node).to_owned()), None, None)
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
        // A block-local `fn` (declared inside a function/closure BODY) is
        // lexically unreachable from other scopes (issue #413 round 3, Codex
        // finding A). Its Symbol node keeps the corrected free-function identity
        // (kind `function`, module-qualified name, no false `Owner::fn` method,
        // no owner DEFINES — `add_symbol` above is intentionally NOT gated, so
        // the node + its DEFINES edge and referential closure are preserved).
        // But it MUST NOT enter ANY call-candidate set, or a call from another
        // scope could bind the buried item — a wrong or ambiguous edge (and a
        // confident WRONG edge when the real target is external, leaving the
        // block-local the sole in-repo candidate). Two candidate sets feed call
        // resolution, so both are gated: the per-file name→id map
        // (`self.definitions`, read by the issue #134 `emit_reference_edges`
        // same-file pass) AND the repo-wide `FileFacts::definitions`
        // (cross-file + same-file-labeling). Lexically-scoped recall of
        // intra-block calls is deferred to a follow-up.
        if !Self::is_block_local_fn(node) {
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
        }
        // Build this function's provable receiver-type environment (issue #441)
        // and install it for the duration of the call-site collection, then
        // restore the caller's environment. Nested functions collect their own
        // call sites through their own `extract_function` and build their own
        // environment, so scopes never bleed.
        let type_env = self.build_type_env(node);
        let outer_type_env = std::mem::replace(&mut self.type_env, type_env);
        self.collect_call_sites(node, &id, &qualified_name);
        self.type_env = outer_type_env;
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
                "function_item" | "impl_item" | "trait_item" | "mod_item" | "macro_definition"
                | "macro_invocation" => {}
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
        let (display, segments, call_kind, receiver_owner, path_root, receiver_type) =
            match function.kind() {
                "identifier" => {
                    let name = self.node_text(function).trim().to_owned();
                    (
                        name.clone(),
                        vec![name],
                        CallKind::Direct,
                        None,
                        CallPathRoot::Unqualified,
                        None,
                    )
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
                    (display, segments, CallKind::Path, None, path_root, None)
                }
                "field_expression" => {
                    let field = function.child_by_field_name("field")?;
                    if field.kind() != "field_identifier" {
                        return None;
                    }
                    let name = self.node_text(field).trim().to_owned();
                    let receiver_value = function.child_by_field_name("value");
                    let receiver_is_self =
                        receiver_value.is_some_and(|value| value.kind() == "self");
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
                                .and_then(|impl_context| {
                                    normalize_impl_owner(&impl_context.method_owner)
                                })
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
                    (
                        name.clone(),
                        vec![name],
                        call_kind,
                        owner,
                        CallPathRoot::Unqualified,
                        receiver_type,
                    )
                }
                _ => return None,
            };
        if segments.is_empty() || segments.iter().any(|segment| !is_simple_ident(segment)) {
            return None;
        }
        Some(CallSiteFact {
            caller_id: caller_id.to_owned(),
            caller_name: caller_name.to_owned(),
            callee_display: display,
            callee_segments: segments,
            call_kind,
            path_root,
            receiver_owner,
            receiver_type,
            span: span(node),
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

    /// Builds the per-function provable receiver-type environment (issue #441):
    /// binding identifier -> reduced nominal type path, for a receiver whose
    /// type is syntactically PROVABLE and UNSHADOWED in the function body.
    ///
    /// Entries come from (a) fn params with a simple-identifier pattern and a
    /// nominal type, and (b) `let x: T` declarations with a simple-identifier
    /// pattern, an explicit type ascription, and a nominal type. Reference,
    /// pointer, and single-level generic types reduce to their core nominal
    /// type; anything else yields no entry ([`reduce_receiver_type`]).
    ///
    /// SHADOWING VETO (conservative): every binding occurrence of each
    /// identifier anywhere in the body is counted — additional `let` shadows,
    /// `for x in`, closure params, `if let`/`while let`, and `match` arm
    /// bindings, including nested destructuring. Any identifier bound at MORE
    /// THAN ONE site is non-provable and dropped, so a shadowed receiver falls
    /// back to today's ambiguous fan-out (prefer a MISSING narrowing to a WRONG
    /// one). Only expression-position identifiers (the receiver USE `x.m()`)
    /// are never counted as binders, so a single typed binding survives.
    fn build_type_env(&self, fn_node: Node<'_>) -> BTreeMap<String, String> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let mut binder_counts: BTreeMap<String, usize> = BTreeMap::new();

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
                if pattern.kind() == "identifier"
                    && let Some(type_node) = param.child_by_field_name("type")
                    && let Some(reduced) = reduce_receiver_type(type_node, self.source)
                {
                    env.insert(self.node_text(pattern).trim().to_owned(), reduced);
                }
            }
        }

        if let Some(body) = fn_node.child_by_field_name("body") {
            self.scan_body_binders(body, &mut env, &mut binder_counts);
        }

        env.retain(|name, _| binder_counts.get(name).copied() == Some(1));
        env
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
        env: &mut BTreeMap<String, String>,
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
                        && let Some(reduced) = reduce_receiver_type(type_node, self.source)
                    {
                        env.insert(self.node_text(pattern).trim().to_owned(), reduced);
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
            self.scan_body_binders(child, env, binder_counts);
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
    /// top-level `tests/` directory, a `#[cfg(test)]` module, or a `#[test]`
    /// function. The classification set is closed for issue #223.
    const fn in_test_context(&self) -> bool {
        self.file_in_tests_dir || self.test_scope_depth > 0
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
        emit_reference_edges(self.graph, &self.definitions, &self.symbol_bodies);
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
}
