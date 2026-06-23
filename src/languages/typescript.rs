//! TypeScript and TSX Tree-sitter extraction.

use std::{collections::BTreeMap, path::Path};

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, stable_id},
    languages::common::{
        SymbolBody, add_graph_edge, collapse_whitespace, emit_reference_edges, identifier_text,
        next_symbol_ordinal, node_name, normalize_c_like_code, path_segments, span,
    },
};

/// Extracts TypeScript syntax records from one source file.
///
/// # Errors
///
/// Returns an error when the file cannot be read, the TypeScript grammar cannot
/// be loaded, or Tree-sitter cannot produce a syntax tree.
pub fn extract_file(
    file: &SourceFile,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<()> {
    let source =
        std::fs::read_to_string(&file.path).map_err(|source| CodegraphError::ReadFile {
            path: file.path.clone(),
            source,
        })?;
    extract_file_source(file, &source, file_id, repository_id, graph)
}

/// Extracts TypeScript syntax records from supplied source text.
///
/// # Errors
///
/// Returns an error when the TypeScript grammar cannot be loaded, or
/// Tree-sitter cannot produce a syntax tree.
pub fn extract_file_source(
    file: &SourceFile,
    source: &str,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<()> {
    let mut parser = Parser::new();
    // Use the TSX grammar for .tsx files, TypeScript grammar otherwise.
    let is_tsx = Path::new(&file.repo_relative_path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("tsx"));
    let language = if is_tsx {
        tree_sitter_typescript::LANGUAGE_TSX.into()
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    };
    parser
        .set_language(&language)
        .map_err(|error| CodegraphError::ParserLanguage(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| CodegraphError::Parse {
            path: file.path.clone(),
        })?;

    let mut extractor = TypeScriptExtractor::new(file, file_id, repository_id, graph, source);
    extractor.walk(tree.root_node());
    extractor.emit_pending_heritage_edges();
    extractor.emit_reference_edges();
    Ok(())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ScopeKind {
    Class,
    Function,
    Namespace,
}

#[derive(Debug, Clone)]
struct Scope {
    name: String,
    kind: ScopeKind,
    id: String,
}

struct PendingHeritage {
    source_id: String,
    base_name: String,
    summary: String,
}

struct TypeScriptExtractor<'graph, 'source> {
    file: &'source SourceFile,
    file_id: &'source str,
    repository_id: &'source str,
    graph: &'graph mut Graph,
    source: &'source str,
    module_names: Vec<String>,
    scope_stack: Vec<Scope>,
    definitions: BTreeMap<String, String>,
    symbol_bodies: Vec<SymbolBody>,
    symbol_ordinals: BTreeMap<(String, String), u64>,
    is_test_file: bool,
    pending_heritage: Vec<PendingHeritage>,
}

impl<'graph, 'source> TypeScriptExtractor<'graph, 'source> {
    fn new(
        file: &'source SourceFile,
        file_id: &'source str,
        repository_id: &'source str,
        graph: &'graph mut Graph,
        source: &'source str,
    ) -> Self {
        let is_test_file = is_test_file(&file.repo_relative_path);
        Self {
            file,
            file_id,
            repository_id,
            graph,
            source,
            module_names: typescript_module_path(&file.repo_relative_path),
            scope_stack: Vec::new(),
            definitions: BTreeMap::new(),
            symbol_bodies: Vec::new(),
            symbol_ordinals: BTreeMap::new(),
            is_test_file,
            pending_heritage: Vec::new(),
        }
    }

    fn walk(&mut self, node: Node<'_>) {
        match node.kind() {
            "import_statement" => self.extract_import(node),
            "class_declaration" | "abstract_class_declaration" => self.extract_class(node),
            "interface_declaration" => self.extract_interface(node),
            "type_alias_declaration" => self.extract_type_alias(node),
            "enum_declaration" => self.extract_enum(node),
            "function_declaration" | "generator_function_declaration" => {
                self.extract_function(node);
            }
            "method_definition" => self.extract_method(node),
            "lexical_declaration" | "variable_declaration" => {
                self.extract_variable_or_func(node);
            }
            // namespace / module declarations: push a Namespace scope so nested
            // declarations qualify with the namespace name.
            "internal_module" => self.extract_namespace(node),
            // export_statement and other wrappers: fall through to walk children.
            _ => self.walk_children(node),
        }
    }

    fn walk_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child);
        }
    }

    fn extract_import(&mut self, node: Node<'_>) {
        let name = collapse_whitespace(self.node_text(node));
        if name.is_empty() {
            return;
        }
        let id = stable_id(&[
            "node",
            "import",
            self.repository_id,
            &self.file.repo_relative_path,
            &name,
        ]);
        self.graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Import,
            self.file.repo_relative_path.clone(),
            span(node),
            name.clone(),
            "typescript",
            format!("TypeScript import {name}"),
        ));
        self.add_edge(
            EdgeLabel::Imports,
            self.owner_id(),
            id,
            format!("{} imports {name}", self.owner_name()),
        );
    }

    fn extract_class(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, "class", &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());

        // Defer Implements edges: base may be declared after this class in the
        // same file. emit_pending_heritage_edges resolves them post-walk.
        for base in self.class_base_names(node) {
            self.pending_heritage.push(PendingHeritage {
                source_id: id.clone(),
                base_name: base.clone(),
                summary: format!("{qualified_name} extends/implements {base}"),
            });
        }

        self.scope_stack.push(Scope {
            name: local_name,
            kind: ScopeKind::Class,
            id,
        });
        self.walk_children(node);
        self.scope_stack.pop();
    }

    fn extract_interface(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, "interface", &qualified_name);
        self.definitions.insert(local_name, id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());

        // Defer Implements edges for interface extends (same reason as classes).
        for base in self.interface_extends_names(node) {
            self.pending_heritage.push(PendingHeritage {
                source_id: id.clone(),
                base_name: base.clone(),
                summary: format!("{qualified_name} extends {base}"),
            });
        }
    }

    fn extract_type_alias(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, "type", &qualified_name);
        self.definitions.insert(local_name, id.clone());
        self.definitions.insert(qualified_name, id);
    }

    fn extract_enum(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, "enum", &qualified_name);
        self.definitions.insert(local_name, id.clone());
        self.definitions.insert(qualified_name, id);
    }

    fn extract_function(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let node_text = self.node_text(node);
        let qualified_name = self.qualify(&local_name);
        let symbol_kind = if self.is_test_file {
            "test"
        } else {
            "function"
        };
        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());
        self.symbol_bodies.push(SymbolBody {
            id: id.clone(),
            name: qualified_name,
            text: node_text.to_owned(),
        });

        self.scope_stack.push(Scope {
            name: local_name,
            kind: ScopeKind::Function,
            id,
        });
        self.walk_children(node);
        self.scope_stack.pop();
    }

    fn extract_method(&mut self, node: Node<'_>) {
        // Only surface methods when directly inside a class scope.
        if !self.in_class_scope() {
            self.walk_children(node);
            return;
        }
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let node_text = self.node_text(node);
        let qualified_name = self.qualify(&local_name);
        let symbol_kind = if self.is_test_file { "test" } else { "method" };
        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());
        self.symbol_bodies.push(SymbolBody {
            id: id.clone(),
            name: qualified_name,
            text: node_text.to_owned(),
        });

        self.scope_stack.push(Scope {
            name: local_name,
            kind: ScopeKind::Function,
            id,
        });
        self.walk_children(node);
        self.scope_stack.pop();
    }

    fn extract_variable_or_func(&mut self, node: Node<'_>) {
        // Only module-level and class-level declarations are surfaced (not
        // locals inside a function body).
        if self.in_function_scope() {
            return;
        }
        let mut cursor = node.walk();
        for declarator in node.named_children(&mut cursor) {
            if declarator.kind() != "variable_declarator" {
                continue;
            }
            let Some(name_node) = declarator.child_by_field_name("name") else {
                continue;
            };
            // Only simple identifiers; skip destructuring patterns.
            if name_node.kind() != "identifier" {
                continue;
            }
            let Some(local_name) = identifier_text(name_node, self.source) else {
                continue;
            };
            let qualified_name = self.qualify(&local_name);
            let value_node = declarator.child_by_field_name("value");
            let is_func = value_node
                .is_some_and(|v| matches!(v.kind(), "arrow_function" | "function_expression"));
            if is_func {
                let node_text = self.node_text(declarator);
                let symbol_kind = if self.is_test_file {
                    "test"
                } else {
                    "function"
                };
                let id = self.add_symbol(declarator, symbol_kind, &qualified_name);
                self.definitions.insert(local_name.clone(), id.clone());
                self.definitions.insert(qualified_name.clone(), id.clone());
                self.symbol_bodies.push(SymbolBody {
                    id,
                    name: qualified_name,
                    text: node_text.to_owned(),
                });
            } else {
                let id = self.add_symbol(declarator, "variable", &qualified_name);
                self.definitions.insert(local_name, id.clone());
                self.definitions.insert(qualified_name, id);
            }
        }
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
        let normalized = normalize_code(self.node_text(node));
        self.graph.push(GraphRecord::syntax_symbol(
            id.clone(),
            symbol_kind,
            self.file.repo_relative_path.clone(),
            span(node),
            qualified_name.to_owned(),
            "typescript",
            disambiguator,
            format!("TypeScript {symbol_kind} {qualified_name}\nSource:\n{normalized}"),
        ));
        self.add_edge(
            EdgeLabel::Defines,
            self.owner_id(),
            id.clone(),
            format!("{} defines {qualified_name}", self.owner_name()),
        );
        id
    }

    fn next_symbol_disambiguator(&mut self, symbol_kind: &str, qualified_name: &str) -> u64 {
        next_symbol_ordinal(&mut self.symbol_ordinals, symbol_kind, qualified_name)
    }

    fn add_edge(&mut self, label: EdgeLabel, source: String, target: String, summary: String) {
        add_graph_edge(self.graph, label, source, target, summary);
    }

    fn emit_reference_edges(&mut self) {
        emit_reference_edges(self.graph, &self.definitions, &self.symbol_bodies);
    }

    /// Resolves deferred `Implements` edges now that the full `definitions` map
    /// is available. Handles forward references (subclass declared before base).
    fn emit_pending_heritage_edges(&mut self) {
        let pending = std::mem::take(&mut self.pending_heritage);
        for entry in pending {
            if let Some(target) = self.definitions.get(&entry.base_name).cloned() {
                add_graph_edge(
                    self.graph,
                    EdgeLabel::Implements,
                    entry.source_id,
                    target,
                    entry.summary,
                );
            }
        }
    }

    fn extract_namespace(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(node, "namespace", &qualified_name);
        self.definitions.insert(local_name.clone(), id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());

        self.scope_stack.push(Scope {
            name: local_name,
            kind: ScopeKind::Namespace,
            id,
        });
        self.walk_children(node);
        self.scope_stack.pop();
    }

    /// Returns base class / implemented interface names from class heritage.
    fn class_base_names(&self, node: Node<'_>) -> Vec<String> {
        let mut names = Vec::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "class_heritage" {
                let mut hcursor = child.walk();
                for hchild in child.named_children(&mut hcursor) {
                    if matches!(hchild.kind(), "extends_clause" | "implements_clause") {
                        names.extend(clause_type_names(hchild, self.source));
                    }
                }
            }
        }
        names
    }

    /// Returns extended interface names from an interface declaration.
    fn interface_extends_names(&self, node: Node<'_>) -> Vec<String> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| child.kind() == "extends_clause")
            .flat_map(|extends| clause_type_names(extends, self.source))
            .collect()
    }

    fn owner_id(&self) -> String {
        self.scope_stack
            .last()
            .map_or_else(|| self.file_id.to_owned(), |scope| scope.id.clone())
    }

    fn owner_name(&self) -> String {
        self.scope_stack.last().map_or_else(
            || {
                if self.module_names.is_empty() {
                    self.file.repo_relative_path.clone()
                } else {
                    self.module_names.join(".")
                }
            },
            |scope| scope.name.clone(),
        )
    }

    fn qualify(&self, local_name: &str) -> String {
        let mut parts = self.module_names.clone();
        parts.extend(self.scope_stack.iter().map(|scope| scope.name.clone()));
        parts.push(local_name.to_owned());
        parts.join(".")
    }

    fn in_class_scope(&self) -> bool {
        self.scope_stack
            .last()
            .is_some_and(|scope| scope.kind == ScopeKind::Class)
    }

    fn in_function_scope(&self) -> bool {
        self.scope_stack
            .iter()
            .any(|scope| scope.kind == ScopeKind::Function)
    }

    fn node_text(&self, node: Node<'_>) -> &'source str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }
}

/// Collects identifier names from an `extends_clause` or `implements_clause`.
///
/// Handles simple identifiers and qualified names (`Ns.Type`), extracting only
/// the leaf identifier so it matches single-file definition keys.
fn clause_type_names(node: Node<'_>, source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "identifier" | "type_identifier" => {
                if let Some(name) = identifier_text(child, source) {
                    names.push(name);
                }
            }
            // Qualified name like `mod.Base` — use the attribute (leaf) part.
            "member_expression" | "nested_type_identifier" => {
                let attr = child
                    .child_by_field_name("property")
                    .or_else(|| child.child_by_field_name("type"));
                if let Some(attr) = attr.and_then(|n| identifier_text(n, source)) {
                    names.push(attr);
                }
            }
            // type_arguments wrapper: recurse one level to find the base type name.
            "generic_type" => {
                if let Some(name) = child
                    .child_by_field_name("name")
                    .and_then(|n| identifier_text(n, source))
                {
                    names.push(name);
                }
            }
            _ => {}
        }
    }
    names
}

/// Reads the `name` field of a node as a text string.
///
/// Works for any declaration whose grammar fills the `name` field — the tree-sitter
/// Returns true when the repo-relative path identifies a test file by suffix.
///
/// Covers Jest/Vitest/Mocha conventions: `*.test.ts`, `*.spec.ts`,
/// `*.test.tsx`, `*.spec.tsx`.
#[must_use]
pub fn is_test_file(repo_relative_path: &str) -> bool {
    let p = repo_relative_path;
    p.ends_with(".test.ts")
        || p.ends_with(".spec.ts")
        || p.ends_with(".test.tsx")
        || p.ends_with(".spec.tsx")
}

/// Computes the dotted module path a TypeScript file contributes to qualified names.
///
/// `src/widget.ts` → `["src", "widget"]`; `src/index.ts` → `["src"]` (barrel);
/// `src/models.d.ts` → `["src", "models"]`; top-level `foo.ts` → `["foo"]`.
fn typescript_module_path(repo_relative_path: &str) -> Vec<String> {
    let mut parts = path_segments(repo_relative_path);
    let Some(last) = parts.pop() else {
        return Vec::new();
    };
    // Barrel files collapse to the parent directory.
    for suffix in &["index.ts", "index.tsx", "index.d.ts", "index.d.tsx"] {
        if last == *suffix {
            return parts;
        }
    }
    // Strip the longest matching extension first so `foo.d.ts` → `foo`, not `foo.d`.
    for ext in &[".d.ts", ".d.tsx", ".ts", ".tsx"] {
        if let Some(stem) = last.strip_suffix(ext) {
            parts.push(stem.to_owned());
            return parts;
        }
    }
    // Unknown extension; keep the segment as-is.
    parts.push(last);
    parts
}

/// Normalizes TypeScript source by stripping `//` and `/* */` comments and
/// collapsing whitespace while preserving string-literal contents.
///
/// All quoted delimiters (`"`, `'`, `` ` ``) process backslash escapes. To
/// treat backtick template literals as raw (no escapes), use Go's normalizer.
#[must_use]
pub fn normalize_code(code: &str) -> String {
    normalize_c_like_code(code, &[])
}

/// Normalizes whole-file TypeScript content for the File node summary.
#[must_use]
pub fn normalize_file_code(code: &str) -> String {
    normalize_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_handles_index_and_nested_modules() {
        assert_eq!(typescript_module_path("foo.ts"), vec!["foo".to_owned()]);
        assert_eq!(
            typescript_module_path("src/widget.ts"),
            vec!["src".to_owned(), "widget".to_owned()]
        );
        assert_eq!(
            typescript_module_path("src/index.ts"),
            vec!["src".to_owned()]
        );
        assert_eq!(
            typescript_module_path("src/components/Button.tsx"),
            vec![
                "src".to_owned(),
                "components".to_owned(),
                "Button".to_owned()
            ]
        );
        assert_eq!(typescript_module_path("index.ts"), Vec::<String>::new());
    }

    #[test]
    fn module_path_strips_compound_d_ts_suffix() {
        // .d.ts should strip the whole compound suffix, not just .ts
        assert_eq!(
            typescript_module_path("src/models.d.ts"),
            vec!["src".to_owned(), "models".to_owned()]
        );
        assert_eq!(
            typescript_module_path("src/types.d.tsx"),
            vec!["src".to_owned(), "types".to_owned()]
        );
        // index.d.ts is a barrel
        assert_eq!(
            typescript_module_path("src/index.d.ts"),
            vec!["src".to_owned()]
        );
    }

    #[test]
    fn normalize_handles_unterminated_block_comment() {
        let code = "const x = 1; /* never closed";
        let norm = normalize_code(code);
        assert!(!norm.contains("/*"), "unterminated comment leaked: {norm}");
        assert!(
            norm.contains("const x=1"),
            "code before comment mangled: {norm}"
        );
    }

    #[test]
    fn normalize_strips_line_comments_and_collapses_whitespace() {
        let code = "function  f( a: number ):  void { // comment\n  return; }";
        let norm = normalize_code(code);
        assert!(!norm.contains("//"), "line comment not stripped: {norm}");
        assert!(!norm.contains("comment"), "comment text leaked: {norm}");
        assert!(
            norm.contains("function f("),
            "function decl mangled: {norm}"
        );
    }

    #[test]
    fn normalize_strips_block_comments() {
        let code = "/* block */ const x = 1; /* end */";
        let norm = normalize_code(code);
        assert!(!norm.contains("/*"), "block comment not stripped: {norm}");
        assert!(norm.contains("const x=1"), "code mangled: {norm}");
    }

    #[test]
    fn normalize_preserves_double_quoted_strings() {
        let code = r#"const s = "not // a comment";"#;
        assert!(
            normalize_code(code).contains("not // a comment"),
            "string content was stripped"
        );
    }

    #[test]
    fn normalize_preserves_single_quoted_strings() {
        let code = "const s = 'not /* a comment */';";
        assert!(
            normalize_code(code).contains("not /* a comment */"),
            "string content was stripped"
        );
    }

    #[test]
    fn normalize_preserves_template_literals() {
        let code = "const s = `hello // world`;";
        assert!(
            normalize_code(code).contains("hello // world"),
            "template literal content was stripped"
        );
    }

    #[test]
    fn is_test_file_matches_jest_vitest_conventions() {
        assert!(is_test_file("src/widget.test.ts"));
        assert!(is_test_file("src/widget.spec.ts"));
        assert!(is_test_file("src/widget.test.tsx"));
        assert!(is_test_file("src/widget.spec.tsx"));
        assert!(!is_test_file("src/widget.ts"));
        assert!(!is_test_file("src/widget.tsx"));
        assert!(!is_test_file("test/widget.ts"));
    }

    #[test]
    fn normalize_import_collapses_multiline() {
        assert_eq!(
            collapse_whitespace("import {\n  Foo,\n  Bar,\n} from 'mod'"),
            "import { Foo, Bar, } from 'mod'"
        );
    }
}
