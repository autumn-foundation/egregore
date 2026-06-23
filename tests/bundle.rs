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

#[test]
fn test_record_scrubbing_and_hashing() {
    use aletheia_egregore::ir::{GraphRecord, NodeKind, OutputHandle};

    let repo_node = GraphRecord::node(
        "repo-1".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("my-repo".to_owned()),
        "Repository node".to_owned(),
    );

    // Let's create an observation node with sensitive text and stdout inline content
    let mut obs_node = GraphRecord::node(
        "obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node".to_owned(),
    );

    if let GraphRecord::Node {
        text,
        stdout_handle,
        source_handle,
        ..
    } = &mut obs_node
    {
        *text = Some("This is a sensitive transcript text".to_owned());
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some("sensitive stdout output".to_owned()),
            hash: "blake3-stdout-hash-val".to_owned(),
            bytes: 24,
        }));
        // Provide a valid source handle so it doesn't fail citation checks yet
        *source_handle = Some("src/observation.txt".to_owned());
    }

    let records = vec![repo_node, obs_node];

    let bundle = export_bundle(&records, "id:obs-1", "0.1.0").expect("export should succeed");

    // The exported bundle should contain obs-1, but scrubbed
    let obs_record = bundle.records.iter().find(|br| br.record.id() == "obs-1").expect("should find obs-1");

    if let GraphRecord::Node {
        text,
        stdout_handle,
        ..
    } = &obs_record.record
    {
        // Assert that sensitive text is removed
        assert!(text.is_none());
        // Assert that stdout handle inline content is removed, but hash and bytes are preserved
        let handle = stdout_handle.as_ref().expect("stdout handle should be present");
        assert!(handle.inline.is_none());
        assert_eq!(handle.hash, "blake3-stdout-hash-val");
        assert_eq!(handle.bytes, 24);
    } else {
        panic!("obs-1 should be a Node");
    }

    // Verify hash of the scrubbed record is correct
    let expected_hash = blake3::hash(serde_json::to_string(&obs_record.record).unwrap().as_bytes()).to_hex().to_string();
    assert_eq!(obs_record.hash, expected_hash);
}

#[test]
fn test_coverage_threshold_fails() {
    use aletheia_egregore::ir::{GraphRecord, NodeKind};

    let repo_node = GraphRecord::node(
        "repo-1".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("my-repo".to_owned()),
        "Repository node".to_owned(),
    );

    // 1. Code record below threshold: we have a Symbol node with NO span
    let sym_node_no_span = GraphRecord::node(
        "sym-no-span".to_owned(),
        NodeKind::Symbol,
        Some("src/main.rs".to_owned()),
        None, // missing span!
        Some("my_func".to_owned()),
        "Symbol node".to_owned(),
    );

    let records = vec![repo_node.clone(), sym_node_no_span];
    let result = export_bundle(&records, "symbol:my_func", "0.1.0");
    assert!(result.is_err(), "should fail because code record is missing span and total records is 2, giving < 95% coverage");

    // 2. Non-code record below 100% threshold: Observation node with no source_handle, evidence_links, or protected handle
    let obs_node_uncited = GraphRecord::node(
        "obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node".to_owned(),
    );

    let records = vec![repo_node, obs_node_uncited];
    let result = export_bundle(&records, "id:obs-1", "0.1.0");
    assert!(result.is_err(), "should fail because non-code record Observation lacks any citable source or evidence link");
}
