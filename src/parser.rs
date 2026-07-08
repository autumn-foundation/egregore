//! Parser orchestration for source files.

use crate::{
    error::Result,
    fs::SourceFile,
    ir::{Graph, GraphRecord},
    languages::{self, Language, cross_file::FileFacts},
};

/// Extracts syntax-backed graph records for a source file, dispatching to the
/// extractor for the file's detected language.
///
/// Files whose extension is not a supported source language are skipped.
///
/// Returns the file's cross-file resolution facts (issue #152); empty for
/// languages without a cross-file resolution pass.
///
/// # Errors
///
/// Returns an error when source reading or Tree-sitter parsing fails.
pub fn extract_source_file(
    file: &SourceFile,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<FileFacts> {
    match languages::detect(&file.repo_relative_path) {
        Some(Language::Rust) => languages::rust::extract_file(file, file_id, repository_id, graph),
        Some(Language::Python) => {
            languages::python::extract_file(file, file_id, repository_id, graph)
                .map(|()| FileFacts::default())
        }
        Some(Language::TypeScript) => {
            languages::typescript::extract_file(file, file_id, repository_id, graph)
                .map(|()| FileFacts::default())
        }
        Some(Language::Go) => languages::go::extract_file(file, file_id, repository_id, graph)
            .map(|()| FileFacts::default()),
        None => Ok(FileFacts::default()),
    }
}

/// Extracts syntax-backed graph records from source text that may not exist in
/// the current working tree, dispatching by the file's detected language.
///
/// Files whose extension is not a supported source language are skipped.
///
/// Returns the file's cross-file resolution facts (issue #152); empty for
/// languages without a cross-file resolution pass.
///
/// # Errors
///
/// Returns an error when Tree-sitter cannot parse the supplied source text.
pub fn extract_source_text(
    file: &SourceFile,
    source: &str,
    file_id: &str,
    repository_id: &str,
    graph: &mut Graph,
) -> Result<FileFacts> {
    match languages::detect(&file.repo_relative_path) {
        Some(Language::Rust) => {
            languages::rust::extract_file_source(file, source, file_id, repository_id, graph)
        }
        Some(Language::Python) => {
            languages::python::extract_file_source(file, source, file_id, repository_id, graph)
                .map(|()| FileFacts::default())
        }
        Some(Language::TypeScript) => {
            languages::typescript::extract_file_source(file, source, file_id, repository_id, graph)
                .map(|()| FileFacts::default())
        }
        Some(Language::Go) => {
            languages::go::extract_file_source(file, source, file_id, repository_id, graph)
                .map(|()| FileFacts::default())
        }
        None => Ok(FileFacts::default()),
    }
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
