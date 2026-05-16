//! Parser orchestration for source files.

use crate::{
    error::Result,
    fs::SourceFile,
    ir::{Graph, GraphRecord},
    languages,
};

/// Extracts syntax-backed graph records for a source file.
///
/// # Errors
///
/// Returns an error when source reading or Tree-sitter parsing fails.
pub fn extract_source_file(file: &SourceFile, file_id: &str, graph: &mut Graph) -> Result<()> {
    languages::rust::extract_file(file, file_id, graph)
}

/// Extracts syntax-backed graph records from source text that may not exist in
/// the current working tree.
///
/// # Errors
///
/// Returns an error when Tree-sitter cannot parse the supplied source text.
pub fn extract_source_text(
    file: &SourceFile,
    source: &str,
    file_id: &str,
    graph: &mut Graph,
) -> Result<()> {
    languages::rust::extract_file_source(file, source, file_id, graph)
}

/// Adds a repository containment edge for a file node.
pub fn add_repository_file_edge(graph: &mut Graph, repository_id: &str, file_id: &str) {
    graph.push(GraphRecord::edge(
        crate::ir::EdgeLabel::Contains,
        repository_id.to_owned(),
        file_id.to_owned(),
        Some("1.0".to_owned()),
        "Repository contains source file".to_owned(),
    ));
}
