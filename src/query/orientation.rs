use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// A crate entry point or binary target.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct EntryPoint {
    /// Stable record ID of the entry point file.
    pub record_id: String,
    /// Repository-relative path to the entry point file.
    pub repo_relative_path: String,
}

/// The kind of a node in the module tree.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Eq, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ModuleNodeKind {
    /// A directory directory segment.
    Directory,
    /// A source file target.
    File,
}

/// A node in the repository module and directory/file tree structure.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct ModuleTreeNode {
    /// Directory segment or filename name.
    pub name: String,
    /// Repo-relative path of this directory or file.
    pub path: String,
    /// Kind of the node: directory or file.
    pub kind: ModuleNodeKind,
    /// Transitive count of symbols contained in files under this path.
    pub symbol_count: usize,
    /// Stable record ID of the file node (if file kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Documented absent span reason for directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_handle_reason: Option<crate::citation_audit::AbsentHandleRule>,
    /// Nested child directories and files.
    pub children: Vec<Self>,
}

/// A symbol ranked by inbound reference degree.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct ReferencedSymbol {
    /// Stable record ID of the symbol.
    pub record_id: String,
    /// Fully qualified name of the symbol.
    pub name: String,
    /// Repository-relative path containing the symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span of the symbol's definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented absent span reason (if span is None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_handle_reason: Option<crate::citation_audit::AbsentHandleRule>,
    /// Total count of inbound reference/calls edges.
    pub inbound_degree: usize,
}

/// Structured orientation map for cold-starting in a repository.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct OrientationMap {
    /// Entry points (roots/binaries).
    pub entry_points: Vec<EntryPoint>,
    /// Directory/file tree with transitive symbol counts.
    pub module_tree: Vec<ModuleTreeNode>,
    /// Top most-referenced symbols ranked by degree.
    pub top_referenced_symbols: Vec<ReferencedSymbol>,
}

/// Error kinds returned by the orientation map builder.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum OrientationError {
    /// The graph contains zero code-graph nodes.
    #[error("graph has zero code-graph nodes")]
    EmptyGraph,
    /// No entry-point files were found.
    #[error("no entry-point files found in the graph")]
    NoEntryPoints,
}

/// Helper to determine if a path is considered a crate root or binary target.
#[must_use]
pub fn is_entry_point(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let path_ref = std::path::Path::new(&normalized);
    let extension = path_ref.extension();

    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("rs")) {
        return normalized == "src/lib.rs"
            || normalized == "src/main.rs"
            || normalized.ends_with("/src/lib.rs")
            || normalized.ends_with("/src/main.rs")
            || normalized.starts_with("src/bin/")
            || normalized.contains("/src/bin/");
    }

    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("go")) {
        return normalized == "main.go"
            || normalized.ends_with("/main.go")
            || normalized.contains("/cmd/");
    }

    if extension
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ts") || ext.eq_ignore_ascii_case("tsx"))
    {
        return normalized == "index.ts"
            || normalized == "main.ts"
            || normalized == "index.tsx"
            || normalized.ends_with("/index.ts")
            || normalized.ends_with("/main.ts")
            || normalized.ends_with("/index.tsx");
    }

    if extension
        .is_some_and(|ext| ext.eq_ignore_ascii_case("js") || ext.eq_ignore_ascii_case("jsx"))
    {
        return normalized == "index.js"
            || normalized == "main.js"
            || normalized.ends_with("/index.js")
            || normalized.ends_with("/main.js");
    }

    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("py")) {
        return normalized == "main.py"
            || normalized == "app.py"
            || normalized.ends_with("/main.py")
            || normalized.ends_with("/app.py");
    }

    false
}

struct TrieNode {
    name: String,
    path: String,
    is_file: bool,
    symbol_count: usize,
    record_id: Option<String>,
    children: BTreeMap<String, Self>,
}

impl TrieNode {
    fn compute_transitive_counts(&mut self) -> usize {
        let children_sum: usize = self
            .children
            .values_mut()
            .map(Self::compute_transitive_counts)
            .sum();
        if !self.is_file {
            self.symbol_count = children_sum;
        }
        self.symbol_count
    }
}

fn convert_trie_node(node: TrieNode) -> ModuleTreeNode {
    let children: Vec<ModuleTreeNode> =
        node.children.into_values().map(convert_trie_node).collect();
    ModuleTreeNode {
        name: node.name,
        path: node.path,
        kind: if node.is_file {
            ModuleNodeKind::File
        } else {
            ModuleNodeKind::Directory
        },
        symbol_count: node.symbol_count,
        record_id: node.record_id,
        absent_handle_reason: if node.is_file {
            None
        } else {
            Some(crate::citation_audit::AbsentHandleRule::NoSpanModuleLevel)
        },
        children,
    }
}

/// Returns a repository orientation map (entry points, module tree, top symbols).
///
/// # Errors
///
/// Returns `OrientationError::EmptyGraph` if there are no code nodes, or
/// `OrientationError::NoEntryPoints` if no entry point files exist.
pub fn orientation_map(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
) -> Result<OrientationMap, OrientationError> {
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    let index = RepositoryIndex::build(records);

    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    let is_code_edge = |label: EdgeLabel| -> bool {
        matches!(
            label,
            EdgeLabel::Contains
                | EdgeLabel::Defines
                | EdgeLabel::Imports
                | EdgeLabel::References
                | EdgeLabel::Calls
                | EdgeLabel::Implements
                | EdgeLabel::Mentions
                // A struct-literal `CONSTRUCTS` edge (issue #443) is code
                // topology and counts toward a type's inbound reference degree.
                | EdgeLabel::Constructs
                // A `REGISTERS_ROUTE` edge (issue #445) is code topology and
                // counts toward a handler's inbound reference degree, so an
                // attribute-routed handler is not misclassified as dead.
                | EdgeLabel::RegistersRoute
        )
    };

    // 1. Zero code-graph nodes check
    let code_nodes_count = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node { id, kind, .. } = r {
                if tombstoned_ids.contains(id.as_str()) {
                    return false;
                }
                if !is_owned(id.as_str()) {
                    return false;
                }
                matches!(
                    kind,
                    NodeKind::Repository
                        | NodeKind::File
                        | NodeKind::Module
                        | NodeKind::Symbol
                        | NodeKind::Import
                        | NodeKind::Diagnostic
                )
            } else {
                false
            }
        })
        .count();

    if code_nodes_count == 0 {
        return Err(OrientationError::EmptyGraph);
    }

    // 2. Entry points extraction
    let mut entry_points = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            if is_entry_point(path) {
                entry_points.push(EntryPoint {
                    record_id: id.clone(),
                    repo_relative_path: path.clone(),
                });
            }
        }
    }
    if entry_points.is_empty() {
        return Err(OrientationError::NoEntryPoints);
    }
    entry_points.sort_by(|a, b| a.repo_relative_path.cmp(&b.repo_relative_path));

    // 3. Module/file tree
    let mut files = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            files.push((id.clone(), path.clone()));
        }
    }

    let mut file_symbol_counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            let normalized_path = path.replace('\\', "/");
            *file_symbol_counts.entry(normalized_path).or_default() += 1;
        }
    }

    let mut trie_roots: BTreeMap<String, TrieNode> = BTreeMap::new();
    for (file_id, file_path) in &files {
        let normalized = file_path.replace('\\', "/");
        let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            continue;
        }
        let first_seg = segments[0].to_string();
        let count = file_symbol_counts.get(&normalized).copied().unwrap_or(0);

        let mut curr_node = trie_roots
            .entry(first_seg.clone())
            .or_insert_with(|| TrieNode {
                name: first_seg.clone(),
                path: first_seg.clone(),
                is_file: segments.len() == 1,
                symbol_count: if segments.len() == 1 { count } else { 0 },
                record_id: if segments.len() == 1 {
                    Some(file_id.clone())
                } else {
                    None
                },
                children: BTreeMap::new(),
            });

        for (i, seg) in segments.iter().enumerate().skip(1) {
            let subpath = segments[0..=i].join("/");
            let is_last = i == segments.len() - 1;
            curr_node = curr_node
                .children
                .entry(seg.to_string())
                .or_insert_with(|| TrieNode {
                    name: seg.to_string(),
                    path: subpath,
                    is_file: is_last,
                    symbol_count: if is_last { count } else { 0 },
                    record_id: if is_last { Some(file_id.clone()) } else { None },
                    children: BTreeMap::new(),
                });
        }
    }

    let mut roots: Vec<TrieNode> = trie_roots.into_values().collect();
    for root in &mut roots {
        root.compute_transitive_counts();
    }
    // Sort roots alphabetically
    roots.sort_by(|a, b| a.name.cmp(&b.name));
    let module_tree: Vec<ModuleTreeNode> = roots.into_iter().map(convert_trie_node).collect();

    // 4. Top-referenced symbols
    let mut inbound_degrees: BTreeMap<String, usize> = BTreeMap::new();
    let mut active_symbols = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name,
            repo_relative_path,
            span,
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            active_symbols.insert(
                id.clone(),
                (
                    name.clone().unwrap_or_default(),
                    repo_relative_path.clone(),
                    *span,
                ),
            );
        }
    }

    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            target,
            source,
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_code_edge(*label) {
                continue;
            }
            if !is_owned(source.as_str()) {
                continue;
            }
            if active_symbols.contains_key(target) {
                *inbound_degrees.entry(target.clone()).or_default() += 1;
            }
        }
    }

    let mut ranked_symbols: Vec<ReferencedSymbol> = active_symbols
        .into_iter()
        .map(|(id, (name, repo_relative_path, span))| {
            let inbound_degree = inbound_degrees.get(&id).copied().unwrap_or(0);
            ReferencedSymbol {
                record_id: id,
                name,
                repo_relative_path,
                span,
                absent_handle_reason: if span.is_none() {
                    Some(crate::citation_audit::AbsentHandleRule::NoSpanModuleLevel)
                } else {
                    None
                },
                inbound_degree,
            }
        })
        .collect();

    ranked_symbols.sort_by(|a, b| {
        b.inbound_degree
            .cmp(&a.inbound_degree)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    ranked_symbols.truncate(limit);

    Ok(OrientationMap {
        entry_points,
        module_tree,
        top_referenced_symbols: ranked_symbols,
    })
}
