//! Python Tree-sitter extraction.

use std::collections::BTreeMap;

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, stable_id},
    languages::common::{contains_identifier, looks_like_call, span},
};

/// Extracts Python syntax records from one source file.
///
/// # Errors
///
/// Returns an error when the file cannot be read, the Python grammar cannot be
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

/// Extracts Python syntax records from supplied source text.
///
/// # Errors
///
/// Returns an error when the Python grammar cannot be loaded, or Tree-sitter
/// cannot produce a syntax tree.
pub fn extract_file_source(
    file: &SourceFile,
    source: &str,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<()> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|error| CodegraphError::ParserLanguage(error.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| CodegraphError::Parse {
            path: file.path.clone(),
        })?;

    let mut extractor = PythonExtractor::new(file, file_id, repository_id, graph, source);
    extractor.walk(tree.root_node());
    extractor.emit_reference_edges();
    Ok(())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ScopeKind {
    Class,
    Function,
}

#[derive(Debug, Clone)]
struct Scope {
    name: String,
    kind: ScopeKind,
    id: String,
}

#[derive(Debug, Clone)]
struct SymbolBody {
    id: String,
    name: String,
    text: String,
}

struct PythonExtractor<'graph, 'source> {
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
}

impl<'graph, 'source> PythonExtractor<'graph, 'source> {
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
            module_names: python_module_path(&file.repo_relative_path),
            scope_stack: Vec::new(),
            definitions: BTreeMap::new(),
            symbol_bodies: Vec::new(),
            symbol_ordinals: BTreeMap::new(),
        }
    }

    fn walk(&mut self, node: Node<'_>) {
        match node.kind() {
            "import_statement" | "import_from_statement" | "future_import_statement" => {
                self.extract_import(node);
            }
            "class_definition" => self.extract_class(node),
            "function_definition" => self.extract_function(node),
            "expression_statement" => self.extract_expression_statement(node),
            // A decorated definition wraps a function/class; recurse so the inner
            // definition is handled in the current scope.
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
        let name = normalize_import(self.node_text(node));
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
            "python",
            format!("Python import {name}"),
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

        // A class inheriting from a base defined in the same file gets an
        // Implements edge to it — the closest analog to Rust's `impl Trait for
        // Type`.
        for base in self.superclass_names(node) {
            if let Some(target) = self.definitions.get(&base).cloned() {
                self.add_edge(
                    EdgeLabel::Implements,
                    id.clone(),
                    target,
                    format!("{qualified_name} inherits {base}"),
                );
            }
        }

        self.scope_stack.push(Scope {
            name: local_name,
            kind: ScopeKind::Class,
            id,
        });
        self.walk_children(node);
        self.scope_stack.pop();
    }

    fn extract_function(&mut self, node: Node<'_>) {
        let Some(local_name) = node_name(node, self.source) else {
            self.walk_children(node);
            return;
        };
        let node_text = self.node_text(node);
        let qualified_name = self.qualify(&local_name);
        let symbol_kind = if self.is_test_function(&local_name) {
            "test"
        } else if self.in_class_scope() {
            "method"
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

    fn extract_expression_statement(&mut self, node: Node<'_>) {
        // Only module-level and class-level bindings to a single name are
        // surfaced as `variable` symbols (parity with Rust const/static); names
        // bound inside a function body are locals and are skipped.
        if self.in_function_scope() {
            return;
        }
        let mut cursor = node.walk();
        let Some(assignment) = node
            .named_children(&mut cursor)
            .find(|child| child.kind() == "assignment")
        else {
            return;
        };
        let Some(left) = assignment.child_by_field_name("left") else {
            return;
        };
        if left.kind() != "identifier" {
            return;
        }
        let Some(local_name) = identifier_text(left, self.source) else {
            return;
        };
        let qualified_name = self.qualify(&local_name);
        let id = self.add_symbol(assignment, "variable", &qualified_name);
        self.definitions.insert(local_name, id.clone());
        self.definitions.insert(qualified_name, id);
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
        let mut record = GraphRecord::symbol(
            id.clone(),
            symbol_kind,
            self.file.repo_relative_path.clone(),
            span(node),
            qualified_name.to_owned(),
            format!("Python {symbol_kind} {qualified_name}\nSource:\n{normalized}"),
        );
        // `GraphRecord::symbol` defaults the language tag to Rust; stamp the
        // Python tag and the source-order disambiguator in place.
        if let GraphRecord::Node {
            language,
            disambiguator: node_disambiguator,
            ..
        } = &mut record
        {
            *language = Some("python".to_owned());
            *node_disambiguator = Some(disambiguator);
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

    fn next_symbol_disambiguator(&mut self, symbol_kind: &str, qualified_name: &str) -> u64 {
        let key = (symbol_kind.to_owned(), qualified_name.to_owned());
        let disambiguator = self.symbol_ordinals.entry(key).or_default();
        let current = *disambiguator;
        *disambiguator += 1;
        current
    }

    fn add_edge(&mut self, label: EdgeLabel, source: String, target: String, summary: String) {
        self.graph.push(GraphRecord::edge(
            label,
            source,
            target,
            Some("1.0".to_owned()),
            summary,
        ));
    }

    fn emit_reference_edges(&mut self) {
        let definitions = self.definitions.clone();
        let bodies = self.symbol_bodies.clone();
        for body in bodies {
            for (name, target_id) in &definitions {
                if body.id == *target_id
                    || name == &body.name
                    || !contains_identifier(&body.text, name)
                {
                    continue;
                }

                // Type the deterministic code-topology edge as `Calls` when the
                // body invokes the name, otherwise `References` (value uses,
                // base classes, type hints). Both are consumed by change-impact
                // and context queries.
                if looks_like_call(&body.text, name) {
                    self.add_edge(
                        EdgeLabel::Calls,
                        body.id.clone(),
                        target_id.clone(),
                        format!("{} calls {name}", body.name),
                    );
                } else {
                    self.add_edge(
                        EdgeLabel::References,
                        body.id.clone(),
                        target_id.clone(),
                        format!("{} references {name}", body.name),
                    );
                }
            }
        }
    }

    fn superclass_names(&self, node: Node<'_>) -> Vec<String> {
        let Some(superclasses) = node.child_by_field_name("superclasses") else {
            return Vec::new();
        };
        let mut cursor = superclasses.walk();
        superclasses
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "identifier")
            .filter_map(|child| identifier_text(child, self.source))
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

    fn in_test_class(&self) -> bool {
        self.scope_stack
            .iter()
            .any(|scope| scope.kind == ScopeKind::Class && scope.name.starts_with("Test"))
    }

    fn is_test_function(&self, local_name: &str) -> bool {
        local_name.starts_with("test_") || (self.in_test_class() && local_name.starts_with("test"))
    }

    fn node_text(&self, node: Node<'_>) -> &'source str {
        node.utf8_text(self.source.as_bytes()).unwrap_or("")
    }
}

fn node_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| identifier_text(name, source))
}

fn identifier_text(node: Node<'_>, source: &str) -> Option<String> {
    node.utf8_text(source.as_bytes())
        .ok()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

/// Collapses an import statement to a single whitespace-normalized line so its
/// node identity is stable regardless of line wrapping.
fn normalize_import(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Computes the dotted module path a Python file contributes to qualified names.
///
/// `pkg/mod.py` → `["pkg", "mod"]`, `pkg/__init__.py` → `["pkg"]` (the package),
/// and a top-level `foo.py` → `["foo"]`.
fn python_module_path(repo_relative_path: &str) -> Vec<String> {
    let mut parts = repo_relative_path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let Some(last) = parts.pop() else {
        return Vec::new();
    };
    if last == "__init__.py" {
        return parts;
    }
    if let Some(stem) = last.strip_suffix(".py") {
        parts.push(stem.to_owned());
    }
    parts
}

/// Normalizes Python source by stripping `#` comments and collapsing whitespace
/// while preserving string-literal contents (including triple-quoted strings).
#[must_use]
pub fn normalize_code(code: &str) -> String {
    let mut result = String::new();
    let mut in_line_comment = false;
    let mut in_string = false;
    let mut string_delim = '"';
    let mut string_triple = false;
    let mut escaped = false;

    let mut pending_space = false;
    let mut last_pushed: Option<char> = None;

    let chars: Vec<char> = code.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_line_comment {
            if c == '\n' {
                in_line_comment = false;
                pending_space = true;
            }
            i += 1;
            continue;
        }
        if in_string {
            result.push(c);
            last_pushed = Some(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == string_delim {
                if string_triple {
                    if i + 2 < chars.len()
                        && chars[i + 1] == string_delim
                        && chars[i + 2] == string_delim
                    {
                        result.push(chars[i + 1]);
                        result.push(chars[i + 2]);
                        in_string = false;
                        i += 3;
                        continue;
                    }
                } else {
                    in_string = false;
                }
            }
            i += 1;
            continue;
        }
        if c == '#' {
            in_line_comment = true;
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let triple = i + 2 < chars.len() && chars[i + 1] == c && chars[i + 2] == c;
            pending_space = false;
            in_string = true;
            string_delim = c;
            string_triple = triple;
            escaped = false;
            result.push(c);
            last_pushed = Some(c);
            if triple {
                result.push(chars[i + 1]);
                result.push(chars[i + 2]);
                i += 3;
            } else {
                i += 1;
            }
            continue;
        }
        if c.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space {
                pending_space = false;
                let is_current_ident = c.is_alphanumeric() || c == '_';
                let is_last_ident =
                    last_pushed.is_some_and(|last| last.is_alphanumeric() || last == '_');
                if is_current_ident && is_last_ident {
                    result.push(' ');
                }
            }
            result.push(c);
            last_pushed = Some(c);
        }
        i += 1;
    }
    result.trim().to_owned()
}

/// Normalizes whole-file Python content for the File node summary.
#[must_use]
pub fn normalize_file_code(code: &str) -> String {
    normalize_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_handles_packages_and_modules() {
        assert_eq!(python_module_path("foo.py"), vec!["foo".to_owned()]);
        assert_eq!(
            python_module_path("pkg/mod.py"),
            vec!["pkg".to_owned(), "mod".to_owned()]
        );
        assert_eq!(
            python_module_path("pkg/__init__.py"),
            vec!["pkg".to_owned()]
        );
        assert!(python_module_path("__init__.py").is_empty());
    }

    #[test]
    fn normalize_strips_hash_comments_and_collapses_whitespace() {
        let code = "def  f( a ):  # trailing comment\n    return a";
        assert_eq!(normalize_code(code), "def f(a):return a");
    }

    #[test]
    fn normalize_preserves_hash_inside_strings() {
        let code = "x = \"not # a comment\"";
        assert_eq!(normalize_code(code), "x=\"not # a comment\"");
    }

    #[test]
    fn normalize_preserves_triple_quoted_docstrings() {
        let code = "def f():\n    \"\"\"doc # not comment\n    second line\"\"\"\n    pass";
        let normalized = normalize_code(code);
        assert!(
            normalized.contains("\"\"\"doc # not comment\n    second line\"\"\""),
            "got: {normalized}"
        );
        assert!(normalized.contains("pass"), "got: {normalized}");
    }

    #[test]
    fn normalize_import_collapses_wrapped_lines() {
        assert_eq!(
            normalize_import("from a import (\n    b,\n    c,\n)"),
            "from a import ( b, c, )"
        );
    }
}
