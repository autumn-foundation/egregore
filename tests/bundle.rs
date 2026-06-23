#![allow(missing_docs)]

use aletheia_egregore::{
    bundle::{export_bundle, EvidenceBundle},
    GraphRecord,
};

#[test]
fn test_basic_bundle_module_exists() {
    let records: Vec<GraphRecord> = vec![];
    let result = export_bundle(&records, "id:test", "0.1.0");
    assert!(result.is_err());
}

#[test]
fn test_bfs_traversal_and_selectors() {
    use aletheia_egregore::ir::{GraphRecord, NodeKind, EdgeLabel, SourceSpan};

    let repo_node = GraphRecord::node(
        "repo-1".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("my-repo".to_owned()),
        "Repository node".to_owned(),
    );

    let file_node = GraphRecord::node(
        "file-1".to_owned(),
        NodeKind::File,
        Some("src/main.rs".to_owned()),
        None,
        Some("src/main.rs".to_owned()),
        "File node".to_owned(),
    );

    let span = SourceSpan {
        start_line: 1,
        end_line: 10,
        start_byte: 0,
        end_byte: 0,
    };
    let sym_node = GraphRecord::node(
        "sym-1".to_owned(),
        NodeKind::Symbol,
        Some("src/main.rs".to_owned()),
        Some(span),
        Some("my_func".to_owned()),
        "Symbol node".to_owned(),
    );

    let edge_repo_file = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo-1".to_owned(),
        "file-1".to_owned(),
        None,
        "repo contains file".to_owned(),
    );

    let edge_file_sym = GraphRecord::edge(
        EdgeLabel::Defines,
        "file-1".to_owned(),
        "sym-1".to_owned(),
        None,
        "file defines sym".to_owned(),
    );

    let records = vec![
        repo_node.clone(),
        file_node.clone(),
        sym_node.clone(),
        edge_repo_file.clone(),
        edge_file_sym.clone(),
    ];

    let bundle = export_bundle(&records, "symbol:my_func", "0.1.0").expect("export should succeed");

    let ids: std::collections::HashSet<&str> = bundle.records.iter().map(|br| br.record.id()).collect();
    assert!(ids.contains("sym-1"));
    assert!(ids.contains("file-1"));
    assert!(ids.contains("repo-1"));
    assert!(ids.contains(edge_file_sym.id()));
    assert!(ids.contains(edge_repo_file.id()));
}
