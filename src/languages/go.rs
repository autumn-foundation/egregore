//! Go Tree-sitter extraction.

use std::collections::BTreeMap;

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, stable_id},
    languages::common::{
        SymbolBody, add_graph_edge, collapse_whitespace, descendant_kinds, emit_reference_edges,
        identifier_text, next_symbol_ordinal, node_name, normalize_c_like_code, path_segments,
        reference_text, span,
    },
};

/// Extracts Go syntax records from one source file.
///
/// # Errors
///
/// Returns an error when the file cannot be read, the Go grammar cannot be
/// loaded, or Tree-sitter cannot produce a syntax tree.
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

/// Extracts Go syntax records from supplied source text.
///
/// # Errors
///
/// Returns an error when the Go grammar cannot be loaded, or Tree-sitter cannot
/// produce a syntax tree.
pub fn extract_file_source(
    file: &SourceFile,
    source: &str,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|error| CodegraphError::ParserLanguage(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| CodegraphError::Parse {
            path: file.path.clone(),
        })?;

    let mut extractor = GoExtractor::new(file, file_id, repository_id, graph, source);
    extractor.walk(tree.root_node());
    extractor.emit_pending_heritage_edges();
    extractor.emit_reference_edges();
    Ok(())
}

/// A deferred embedding relationship, resolved to an `Implements` edge after the
/// whole file is walked (the embedded type may be declared later in the file).
struct PendingHeritage {
    source_id: String,
    base_name: String,
    summary: String,
}

/// Tree-sitter node kinds whose text never yields reference edges (issue #134):
/// comment and literal content must not produce `CALLS`/`REFERENCES` matches.
const REFERENCE_EXCLUDED_KINDS: &[&str] = &[
    "comment",
    "interpreted_string_literal",
    "raw_string_literal",
    "rune_literal",
];

struct GoExtractor<'graph, 'source> {
    file: &'source SourceFile,
    file_id: &'source str,
    repository_id: &'source str,
    graph: &'graph mut Graph,
    source: &'source str,
    module_names: Vec<String>,
    definitions: BTreeMap<String, String>,
    symbol_bodies: Vec<SymbolBody>,
    symbol_ordinals: BTreeMap<(String, String), u64>,
    is_test_file: bool,
    pending_heritage: Vec<PendingHeritage>,
}

impl<'graph, 'source> GoExtractor<'graph, 'source> {
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
            module_names: go_module_path(&file.repo_relative_path),
            definitions: BTreeMap::new(),
            symbol_bodies: Vec::new(),
            symbol_ordinals: BTreeMap::new(),
            is_test_file,
            pending_heritage: Vec::new(),
        }
    }

    fn walk(&mut self, node: Node<'_>) {
        match node.kind() {
            // The package name is implicit; qualified names come from the
            // package directory, so the clause itself surfaces no symbol.
            "package_clause" => {}
            "import_declaration" => self.extract_imports(node),
            "function_declaration" => self.extract_function(node),
            "method_declaration" => self.extract_method(node),
            "type_declaration" => self.extract_type_declaration(node),
            "const_declaration" => self.extract_value_declaration(node, "const_spec", "const"),
            "var_declaration" => self.extract_value_declaration(node, "var_spec", "var"),
            _ => self.walk_children(node),
        }
    }

    fn walk_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child);
        }
    }

    /// Emits one `Import` node per import spec (so grouped `import ( … )` blocks
    /// surface a node per package), with an `Imports` edge from the file.
    fn extract_imports(&mut self, node: Node<'_>) {
        for spec in descendant_kinds(node, "import_spec") {
            let name = collapse_whitespace(self.node_text(spec));
            if name.is_empty() {
                continue;
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
                span(spec),
                name.clone(),
                "go",
                format!("Go import {name}"),
            ));
            self.add_edge(
                EdgeLabel::Imports,
                self.file_id.to_owned(),
                id,
                format!("{} imports {name}", self.owner_name()),
            );
        }
    }

    fn extract_function(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            return;
        };
        let qualified_name = self.qualify(&[&local_name]);
        let symbol_kind = if self.is_test_file {
            "test"
        } else {
            "function"
        };
        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.record_definition(&local_name, &qualified_name, &id);
        self.symbol_bodies.push(SymbolBody {
            id,
            name: qualified_name,
            text: reference_text(node, self.source, REFERENCE_EXCLUDED_KINDS),
        });
    }

    fn extract_method(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            return;
        };
        // Qualify by the receiver type so `func (w *Widget) Describe()` becomes
        // `<pkg>.Widget.Describe`. Methods on an unnamed receiver fall back to
        // package-level qualification.
        let receiver = self.receiver_type_name(node);
        let qualified_name = receiver.as_ref().map_or_else(
            || self.qualify(&[&local_name]),
            |recv| self.qualify(&[recv, &local_name]),
        );
        let symbol_kind = if self.is_test_file { "test" } else { "method" };
        let id = self.add_symbol(node, symbol_kind, &qualified_name);
        self.record_definition(&local_name, &qualified_name, &id);
        self.symbol_bodies.push(SymbolBody {
            id,
            name: qualified_name,
            text: reference_text(node, self.source, REFERENCE_EXCLUDED_KINDS),
        });
    }

    fn extract_type_declaration(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for spec in node.named_children(&mut cursor) {
            // `type_spec` is a defined type (`type T …`); `type_alias` is
            // `type T = U`. Both expose `name` and `type` fields.
            if !matches!(spec.kind(), "type_spec" | "type_alias") {
                continue;
            }
            let Some(local_name) = node_name(spec, self.source) else {
                continue;
            };
            let qualified_name = self.qualify(&[&local_name]);
            let type_node = spec.child_by_field_name("type");
            let symbol_kind = match (spec.kind(), type_node.map(|t| t.kind())) {
                ("type_alias", _) => "type",
                (_, Some("struct_type")) => "struct",
                (_, Some("interface_type")) => "interface",
                _ => "type",
            };
            let id = self.add_symbol(spec, symbol_kind, &qualified_name);
            self.record_definition(&local_name, &qualified_name, &id);

            // Embedded types/interfaces are the closest Go analog to inheritance;
            // defer their Implements edges until all in-file definitions exist.
            if let Some(type_node) = type_node {
                for base in self.embedded_type_names(type_node) {
                    self.pending_heritage.push(PendingHeritage {
                        source_id: id.clone(),
                        base_name: base.clone(),
                        summary: format!("{qualified_name} embeds {base}"),
                    });
                }
            }
        }
    }

    fn extract_value_declaration(&mut self, node: Node<'_>, spec_kind: &str, symbol_kind: &str) {
        for spec in descendant_kinds(node, spec_kind) {
            let mut cursor = spec.walk();
            for name_node in spec.children_by_field_name("name", &mut cursor) {
                let Some(local_name) = identifier_text(name_node, self.source) else {
                    continue;
                };
                let qualified_name = self.qualify(&[&local_name]);
                let id = self.add_symbol(spec, symbol_kind, &qualified_name);
                self.record_definition(&local_name, &qualified_name, &id);
            }
        }
    }

    /// Returns the receiver type name of a method, stripping pointers and
    /// generic/package qualification down to the leaf type identifier.
    fn receiver_type_name(&self, node: Node<'_>) -> Option<String> {
        let receiver = node.child_by_field_name("receiver")?;
        let mut cursor = receiver.walk();
        let param = receiver
            .named_children(&mut cursor)
            .find(|child| child.kind() == "parameter_declaration")?;
        let type_node = param.child_by_field_name("type")?;
        leaf_type_name(type_node, self.source)
    }

    /// Collects embedded type names from a `struct_type` or `interface_type`.
    ///
    /// Struct embedding is a `field_declaration` with no field name; interface
    /// embedding is a `type_elem` element.
    fn embedded_type_names(&self, type_node: Node<'_>) -> Vec<String> {
        let mut names = Vec::new();
        match type_node.kind() {
            "struct_type" => {
                for field in descendant_kinds(type_node, "field_declaration") {
                    // A named field has at least one `name` child; embedded
                    // fields have none — only an anonymous type.
                    if field.child_by_field_name("name").is_some() {
                        continue;
                    }
                    if let Some(type_child) = field.child_by_field_name("type")
                        && let Some(name) = leaf_type_name(type_child, self.source)
                    {
                        names.push(name);
                    }
                }
            }
            "interface_type" => {
                let mut cursor = type_node.walk();
                for elem in type_node.named_children(&mut cursor) {
                    if elem.kind() != "type_elem" {
                        continue;
                    }
                    // A `type_elem` is an embedded interface only when it is a
                    // single plain type name. A union (`A | B`) or a `~T`
                    // approximation (`negated_type`) is a generics type-set
                    // constraint — a type set, not inheritance — so it must not
                    // produce an Implements edge.
                    let mut inner = elem.walk();
                    let terms = elem.named_children(&mut inner).collect::<Vec<_>>();
                    let [term] = terms.as_slice() else {
                        continue;
                    };
                    if term.kind() == "negated_type" {
                        continue;
                    }
                    if let Some(name) = leaf_type_name(*term, self.source) {
                        names.push(name);
                    }
                }
            }
            _ => {}
        }
        names
    }

    fn add_symbol(&mut self, node: Node<'_>, symbol_kind: &str, qualified_name: &str) -> String {
        let disambiguator =
            next_symbol_ordinal(&mut self.symbol_ordinals, symbol_kind, qualified_name);
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
            "go",
            disambiguator,
            format!("Go {symbol_kind} {qualified_name}\nSource:\n{normalized}"),
        ));
        self.add_edge(
            EdgeLabel::Defines,
            self.file_id.to_owned(),
            id.clone(),
            format!("{} defines {qualified_name}", self.owner_name()),
        );
        id
    }

    /// Registers a symbol under both its local and qualified names so the
    /// reference-edge pass can match either spelling in sibling bodies.
    fn record_definition(&mut self, local_name: &str, qualified_name: &str, id: &str) {
        self.definitions
            .insert(local_name.to_owned(), id.to_owned());
        self.definitions
            .insert(qualified_name.to_owned(), id.to_owned());
    }

    fn add_edge(&mut self, label: EdgeLabel, source: String, target: String, summary: String) {
        add_graph_edge(self.graph, label, source, target, summary);
    }

    fn emit_reference_edges(&mut self) {
        emit_reference_edges(self.graph, &self.definitions, &self.symbol_bodies);
    }

    /// Resolves deferred `Implements` edges for embedded types now that every
    /// in-file definition is known (handles types embedded before they appear).
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

    fn owner_name(&self) -> String {
        if self.module_names.is_empty() {
            self.file.repo_relative_path.clone()
        } else {
            self.module_names.join(".")
        }
    }

    /// Builds a dotted qualified name from the package path plus the given
    /// trailing segments (e.g. `[recv, method]`).
    fn qualify(&self, tail: &[&str]) -> String {
        let mut parts = self.module_names.clone();
        parts.extend(tail.iter().map(ToString::to_string));
        parts.join(".")
    }

    fn node_text(&self, node: Node<'_>) -> &'source str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }
}

/// Resolves a type expression to its leaf type identifier.
///
/// Strips `*T` pointers, `pkg.T` qualification (keeps the leaf `T`), and
/// `T[…]` generic instantiation. Returns `None` for anonymous/composite types.
fn leaf_type_name(node: Node<'_>, source: &str) -> Option<String> {
    match node.kind() {
        "type_identifier" => identifier_text(node, source),
        // `*T` — recurse into the pointed-to type.
        "pointer_type" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find_map(|child| leaf_type_name(child, source))
        }
        // `pkg.T` — the leaf is the `name` field.
        "qualified_type" => node
            .child_by_field_name("name")
            .and_then(|n| identifier_text(n, source)),
        // `T[Args]` — the base is the `type` field.
        "generic_type" => node
            .child_by_field_name("type")
            .and_then(|n| leaf_type_name(n, source)),
        _ => None,
    }
}

/// Returns true when the repo-relative path is a Go test file (`*_test.go`).
#[must_use]
pub fn is_test_file(repo_relative_path: &str) -> bool {
    repo_relative_path.ends_with("_test.go")
}

/// Computes the dotted package path a Go file contributes to qualified names.
///
/// Go packages are directory-scoped, so the filename is dropped and the
/// directory segments form the prefix: `internal/widget/widget.go` →
/// `["internal", "widget"]`; a root-level `main.go` → `[]`.
/// Computes the package path a Go file contributes to qualified names.
///
/// Go packages are directory-scoped: the filename is dropped and the directory
/// segments form the prefix. `internal/widget/widget.go` → `["internal", "widget"]`;
/// a root-level `main.go` → `[]`.
fn go_module_path(repo_relative_path: &str) -> Vec<String> {
    let mut parts = path_segments(repo_relative_path);
    // Drop the filename; the package is the containing directory.
    parts.pop();
    parts
}

/// Normalizes Go source by stripping `//` and `/* */` comments and collapsing
/// whitespace while preserving string- and rune-literal contents.
///
/// Go raw strings use backticks; backtick-delimited content passes backslash
/// through unchanged. Interpreted strings (`"`) and rune literals (`'`) process
/// escape sequences normally.
#[must_use]
pub fn normalize_code(code: &str) -> String {
    normalize_c_like_code(code, &['`'])
}

/// Normalizes whole-file Go content for the File node summary.
#[must_use]
pub fn normalize_file_code(code: &str) -> String {
    normalize_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_is_directory_scoped() {
        assert_eq!(go_module_path("main.go"), Vec::<String>::new());
        assert_eq!(
            go_module_path("widget/widget.go"),
            vec!["widget".to_owned()]
        );
        assert_eq!(
            go_module_path("internal/widget/widget.go"),
            vec!["internal".to_owned(), "widget".to_owned()]
        );
    }

    #[test]
    fn is_test_file_matches_go_convention() {
        assert!(is_test_file("widget/widget_test.go"));
        assert!(!is_test_file("widget/widget.go"));
        assert!(!is_test_file("widget/test.go"));
    }

    #[test]
    fn normalize_strips_line_comments_and_collapses_whitespace() {
        let code = "func  Foo( a int )  int { // comment\n\treturn a }";
        let norm = normalize_code(code);
        assert!(!norm.contains("//"), "line comment not stripped: {norm}");
        assert!(!norm.contains("comment"), "comment text leaked: {norm}");
        assert!(norm.contains("func Foo("), "func decl mangled: {norm}");
    }

    #[test]
    fn normalize_strips_block_comments() {
        let code = "/* block */ const x = 1 /* end */";
        let norm = normalize_code(code);
        assert!(!norm.contains("/*"), "block comment not stripped: {norm}");
        assert!(norm.contains("const x=1"), "code mangled: {norm}");
    }

    #[test]
    fn normalize_preserves_raw_strings() {
        let code = "s := `not // a comment`";
        assert!(
            normalize_code(code).contains("not // a comment"),
            "raw string content was stripped"
        );
    }

    #[test]
    fn normalize_preserves_interpreted_strings() {
        let code = r#"s := "not /* a comment */""#;
        assert!(
            normalize_code(code).contains("not /* a comment */"),
            "interpreted string content was stripped"
        );
    }

    #[test]
    fn normalize_handles_unterminated_block_comment() {
        let code = "const x = 1 /* never closed";
        let norm = normalize_code(code);
        assert!(!norm.contains("/*"), "unterminated comment leaked: {norm}");
        assert!(
            norm.contains("const x=1"),
            "code before comment mangled: {norm}"
        );
    }
}
