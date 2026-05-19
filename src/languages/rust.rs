//! Rust Tree-sitter extraction.

use std::{collections::BTreeMap, path::PathBuf};

use tree_sitter::{Node, Parser};

use crate::{
    error::{CodegraphError, Result},
    fs::SourceFile,
    ir::{EdgeLabel, Graph, GraphRecord, NodeKind, SourceSpan, stable_id},
};

/// Extracts Rust syntax records from one source file.
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
) -> Result<()> {
    let source =
        std::fs::read_to_string(&file.path).map_err(|source| CodegraphError::ReadFile {
            path: file.path.clone(),
            source,
        })?;
    extract_file_source(file, &source, file_id, repository_id, graph)
}

/// Extracts Rust syntax records from supplied source text.
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
) -> Result<()> {
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
    Ok(())
}

#[derive(Debug, Clone)]
struct ImplContext {
    display: String,
    method_owner: String,
    id: String,
}

#[derive(Debug, Clone)]
struct SymbolBody {
    id: String,
    name: String,
    text: String,
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
        self.graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Module,
            self.file.repo_relative_path.clone(),
            span(node),
            qualified_name.clone(),
            "rust",
            format!("Rust module {qualified_name}"),
        ));
        self.add_edge(
            EdgeLabel::Contains,
            self.owner_id(),
            id.clone(),
            format!("{} contains module {qualified_name}", self.owner_name()),
        );

        self.module_names.push(local_name);
        self.owner_ids.push(id);
        self.walk_children(node);
        self.owner_ids.pop();
        self.module_names.pop();
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
        self.graph.push(GraphRecord::syntax_node(
            id.clone(),
            NodeKind::Import,
            self.file.repo_relative_path.clone(),
            span(node),
            name.clone(),
            "rust",
            format!("Rust import {name}"),
        ));
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
        let node_text = self.node_text(node);
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
        self.definitions.insert(local_name, id.clone());
        self.definitions.insert(qualified_name.clone(), id.clone());
        self.symbol_bodies.push(SymbolBody {
            id,
            name: qualified_name,
            text: node_text.to_owned(),
        });
        self.walk_children(node);
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
        let id = stable_id(&[
            "node",
            "diagnostic",
            self.repository_id,
            &self.file.repo_relative_path,
            &invocation,
            &span(node).start_byte.to_string(),
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

    fn add_symbol(&mut self, node: Node<'_>, symbol_kind: &str, qualified_name: &str) -> String {
        let id = stable_id(&[
            "node",
            "symbol",
            symbol_kind,
            self.repository_id,
            &self.file.repo_relative_path,
            qualified_name,
            &span(node).start_byte.to_string(),
        ]);
        self.graph.push(GraphRecord::symbol(
            id.clone(),
            symbol_kind,
            self.file.repo_relative_path.clone(),
            span(node),
            qualified_name.to_owned(),
            format!("Rust {symbol_kind} {qualified_name}"),
        ));
        self.add_edge(
            EdgeLabel::Defines,
            self.owner_id(),
            id.clone(),
            format!("{} defines {qualified_name}", self.owner_name()),
        );
        id
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
                if body.id == *target_id || name == &body.name || !body.text.contains(name) {
                    continue;
                }

                self.add_edge(
                    EdgeLabel::Mentions,
                    body.id.clone(),
                    target_id.clone(),
                    format!("{} mentions {name}", body.name),
                );

                if looks_like_call(&body.text, name) {
                    self.add_edge(
                        EdgeLabel::Calls,
                        body.id.clone(),
                        target_id.clone(),
                        format!("{} calls {name}", body.name),
                    );
                }
            }
        }
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

fn node_name(node: Node<'_>, source: &str) -> Option<String> {
    node.child_by_field_name("name")
        .and_then(|name| name.utf8_text(source.as_bytes()).ok())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(ToOwned::to_owned)
}

fn span(node: Node<'_>) -> SourceSpan {
    SourceSpan {
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    }
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

fn macro_invocation_name(text: &str) -> String {
    let trimmed = text.trim();
    trimmed
        .split_once('!')
        .map_or(trimmed, |(name, _)| name.trim())
        .trim_end_matches(';')
        .to_owned()
        + "!"
}

fn looks_like_call(text: &str, name: &str) -> bool {
    let simple_name = name.rsplit("::").next().unwrap_or(name);
    let direct = format!("{simple_name}(");
    let associated = format!("::{simple_name}(");
    let method = format!(".{simple_name}(");
    text.contains(&direct) || text.contains(&associated) || text.contains(&method)
}

fn file_module_path(repo_relative_path: &str) -> Vec<String> {
    let parts = repo_relative_path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
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
