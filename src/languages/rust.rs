//! Rust Tree-sitter extraction.

use std::{collections::BTreeMap, path::PathBuf};

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, stable_id},
    languages::{
        common::{
            SymbolBody, add_graph_edge, emit_reference_edges, next_symbol_ordinal, node_name,
            path_segments, reference_text, span,
        },
        cross_file::{CallKind, CallSiteFact, DefinitionFact, FileFacts, OutOfLineModFact},
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

#[derive(Debug, Clone)]
struct ImplContext {
    display: String,
    method_owner: String,
    id: String,
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
    definitions: BTreeMap<String, String>,
    symbol_bodies: Vec<SymbolBody>,
    symbol_ordinals: BTreeMap<(String, String), u64>,
    diagnostic_ordinals: BTreeMap<String, u64>,
    facts: FileFacts,
    panic_risk_ordinals: BTreeMap<String, u64>,
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
            definitions: BTreeMap::new(),
            symbol_bodies: Vec::new(),
            symbol_ordinals: BTreeMap::new(),
            diagnostic_ordinals: BTreeMap::new(),
            facts: FileFacts::default(),
            panic_risk_ordinals: BTreeMap::new(),
            inline_module_stack: Vec::new(),
            inline_path_override_depth: 0,
            test_scope_depth: 0,
            file_in_tests_dir: path_segments(&file.repo_relative_path)
                .first()
                .is_some_and(|segment| segment == "tests"),
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
            .with_declaration_surface(
                Some(self.symbol_visibility(node).to_owned()),
                None,
                None,
            ),
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
        let name = import_name(self.node_text(node));
        let id = stable_id(&[
            "node",
            "import",
            self.repository_id,
            &self.file.repo_relative_path,
            &name,
        ]);
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

    fn extract_named_symbol(&mut self, node: Node<'_>, symbol_kind: &str) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.definitions.insert(local_name, id.clone());
        self.definitions.insert(qualified_name.clone(), id);
        self.walk_children(node);
    }

    fn extract_function(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let (symbol_kind, qualified_name) = self.impl_context.as_ref().map_or_else(
            || {
                if self.is_test_function(node) {
                    ("test", self.qualify(&local_name))
                } else {
                    ("function", self.qualify(&local_name))
                }
            },
            |impl_context| {
                (
                    "method",
                    self.qualify(&format!("{}::{local_name}", impl_context.method_owner)),
                )
            },
        );

        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());
        self.facts.definitions.push(DefinitionFact {
            id: id.clone(),
            qualified_name: qualified_name.clone(),
            simple_name: local_name.clone(),
            match_segments: self.definition_match_segments(&local_name),
            symbol_kind: symbol_kind.to_owned(),
            repo_relative_path: self.file.repo_relative_path.clone(),
        });
        self.collect_call_sites(node, &id, &qualified_name);
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
    /// normalized impl owner for methods, plus the simple name.
    fn definition_match_segments(&self, local_name: &str) -> Vec<String> {
        let mut segments = self.module_names.clone();
        if let Some(impl_context) = &self.impl_context
            && let Some(owner) = normalize_impl_owner(&impl_context.method_owner)
        {
            segments.push(owner);
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
        let (display, segments, call_kind, receiver_owner) = match function.kind() {
            "identifier" => {
                let name = self.node_text(function).trim().to_owned();
                (name.clone(), vec![name], CallKind::Direct, None)
            }
            "scoped_identifier" => {
                let display = self.node_text(function).trim().to_owned();
                let segments = self.normalize_call_path(&display)?;
                (display, segments, CallKind::Path, None)
            }
            "field_expression" => {
                let field = function.child_by_field_name("field")?;
                if field.kind() != "field_identifier" {
                    return None;
                }
                let name = self.node_text(field).trim().to_owned();
                let receiver_is_self = function
                    .child_by_field_name("value")
                    .is_some_and(|value| value.kind() == "self");
                let owner = receiver_is_self
                    .then(|| {
                        self.impl_context.as_ref().and_then(|impl_context| {
                            normalize_impl_owner(&impl_context.method_owner)
                        })
                    })
                    .flatten();
                let call_kind = if owner.is_some() {
                    CallKind::SelfMethod
                } else {
                    CallKind::Method
                };
                (name.clone(), vec![name], call_kind, owner)
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
            receiver_owner,
            span: span(node),
        })
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
            }
        }
        (!segments.is_empty()).then_some(segments)
    }

    fn extract_impl(&mut self, node: Node<'_>) {
        let display = impl_display(self.node_text(node));
        let qualified_name = self.qualify(&display);
        let id = self.add_symbol(node, "impl", &qualified_name);
        self.definitions.insert(qualified_name, id.clone());

        if let Some(target) = self.impl_target_id(&display) {
            self.add_edge(
                EdgeLabel::Implements,
                id.clone(),
                target,
                format!("{display} implementation relationship"),
            );
        }

        let previous = self.impl_context.replace(ImplContext {
            method_owner: method_owner(&display),
            display,
            id,
        });
        self.walk_children(node);
        self.impl_context = previous;
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

    fn impl_target_id(&self, display: &str) -> Option<String> {
        let target = display
            .strip_prefix("impl ")
            .and_then(|rest| rest.split(" for ").next())
            .unwrap_or(display)
            .trim();
        self.definitions.get(target).cloned()
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
    let owned = path_segments(repo_relative_path);
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
}
