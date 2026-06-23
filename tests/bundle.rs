#![allow(missing_docs)]

use aletheia_egregore::{
    GraphRecord,
    bundle::{export_bundle, verify_bundle},
};

#[test]
fn test_basic_bundle_module_exists() {
    let records: Vec<GraphRecord> = vec![];
    let result = export_bundle(&records, "id:test", "0.1.0");
    assert!(result.is_err());
}

#[test]
fn test_bfs_traversal_and_selectors() {
    use aletheia_egregore::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

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
        repo_node,
        file_node,
        sym_node,
        edge_repo_file.clone(),
        edge_file_sym.clone(),
    ];

    let bundle = export_bundle(&records, "symbol:my_func", "0.1.0").expect("export should succeed");

    let ids: std::collections::HashSet<&str> =
        bundle.records.iter().map(|br| br.record.id()).collect();
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
    let obs_record = bundle
        .records
        .iter()
        .find(|br| br.record.id() == "obs-1")
        .expect("should find obs-1");

    if let GraphRecord::Node {
        text,
        stdout_handle,
        ..
    } = &obs_record.record
    {
        // Assert that sensitive text is removed
        assert!(text.is_none());
        // Assert that stdout handle inline content is removed, but hash and bytes are preserved
        let handle = stdout_handle
            .as_ref()
            .expect("stdout handle should be present");
        assert!(handle.inline.is_none());
        assert_eq!(handle.hash, "blake3-stdout-hash-val");
        assert_eq!(handle.bytes, 24);
    } else {
        panic!("obs-1 should be a Node");
    }

    // Verify hash of the scrubbed record is correct
    let expected_hash = blake3::hash(
        serde_json::to_string(&obs_record.record)
            .unwrap()
            .as_bytes(),
    )
    .to_hex()
    .to_string();
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
    assert!(
        result.is_err(),
        "should fail because code record is missing span and total records is 2, giving < 95% coverage"
    );

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
    assert!(
        result.is_err(),
        "should fail because non-code record Observation lacks any citable source or evidence link"
    );
}

#[test]
fn test_bundle_verification() {
    use aletheia_egregore::ir::{EdgeLabel, GraphRecord, NodeKind, OutputHandle};

    let repo_node = GraphRecord::node(
        "repo-1".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("my-repo".to_owned()),
        "Repository node".to_owned(),
    );

    let mut obs_node = GraphRecord::node(
        "obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node".to_owned(),
    );

    if let GraphRecord::Node { source_handle, .. } = &mut obs_node {
        *source_handle = Some("src/obs.txt".to_owned());
    }

    let edge = GraphRecord::edge(
        EdgeLabel::Contains,
        "repo-1".to_owned(),
        "obs-1".to_owned(),
        None,
        "repo contains obs".to_owned(),
    );

    let records = vec![repo_node, obs_node, edge];
    let mut bundle = export_bundle(&records, "id:obs-1", "0.1.0").expect("export should succeed");

    // 1. Positive case: valid bundle should verify successfully
    let report = verify_bundle(&bundle);
    assert!(report.ok);
    assert!(report.integrity.passed);
    assert!(report.coverage.passed);
    assert!(report.safety.passed);

    // 2. Integrity failure: tamper with a record hash
    let original_hash = bundle.records[0].hash.clone();
    bundle.records[0].hash = "wrong_hash".to_owned();
    let report = verify_bundle(&bundle);
    assert!(!report.ok);
    assert!(!report.integrity.passed);
    bundle.records[0].hash = original_hash; // restore

    // 3. Integrity failure: wrong ordering
    bundle.records.swap(0, 1);
    let report = verify_bundle(&bundle);
    assert!(!report.ok);
    assert!(!report.integrity.passed);
    bundle.records.swap(0, 1); // restore

    // 4. Safety failure: unredacted secret API token in text field
    let obs_rec = bundle
        .records
        .iter_mut()
        .find(|r| r.record.id() == "obs-1")
        .unwrap();
    if let GraphRecord::Node { text, .. } = &mut obs_rec.record {
        *text = Some("unredacted API token: sk_live_12345abcdef".to_owned());
    }
    let report = verify_bundle(&bundle);
    assert!(!report.ok);
    assert!(!report.safety.passed);

    // restore
    let obs_rec = bundle
        .records
        .iter_mut()
        .find(|r| r.record.id() == "obs-1")
        .unwrap();
    if let GraphRecord::Node { text, .. } = &mut obs_rec.record {
        *text = None;
    }

    // 5. Safety failure: inline payload present in stdout_handle
    let obs_rec = bundle
        .records
        .iter_mut()
        .find(|r| r.record.id() == "obs-1")
        .unwrap();
    if let GraphRecord::Node { stdout_handle, .. } = &mut obs_rec.record {
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some("unredacted payload".to_owned()),
            hash: "abc".to_owned(),
            bytes: 18,
        }));
    }
    let report = verify_bundle(&bundle);
    assert!(!report.ok);
    assert!(!report.safety.passed);
}

#[test]
#[allow(clippy::too_many_lines)]
fn test_bundle_cli_workflow() {
    use aletheia_egregore::ir::{GraphRecord, NodeKind};
    use assert_cmd::Command;
    use predicates::prelude::*;
    use std::fs;

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("graph.jsonl");
    let bundle_path = temp.path().join("bundle.json");

    // Seed a valid graph
    let repo_node = GraphRecord::node(
        "repo-1".to_owned(),
        NodeKind::Repository,
        None,
        None,
        Some("my-repo".to_owned()),
        "Repository node".to_owned(),
    );

    let mut obs_node = GraphRecord::node(
        "obs-1".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Observation node".to_owned(),
    );

    if let GraphRecord::Node {
        source_handle,
        text,
        schema_version,
        ..
    } = &mut obs_node
    {
        *source_handle = Some("src/obs.txt".to_owned());
        *text = Some("This contains sensitive text".to_owned());
        *schema_version = 1;
    }

    let records = vec![repo_node, obs_node];
    let mut graph_content = String::new();
    for r in records {
        graph_content.push_str(&serde_json::to_string(&r).unwrap());
        graph_content.push('\n');
    }
    fs::write(&graph_path, graph_content).unwrap();

    // 1. Export the bundle
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("bundle")
        .arg("export")
        .arg("--root-selector")
        .arg("id:obs-1")
        .arg("--graph")
        .arg(&graph_path)
        .arg("--out")
        .arg(&bundle_path)
        .assert()
        .success();

    // Verify bundle file content is redacted
    let bundle_content = fs::read_to_string(&bundle_path).expect("bundle file should exist");
    assert!(bundle_content.contains(r#""root_selector": "id:obs-1""#));
    assert!(
        !bundle_content.contains("This contains sensitive text"),
        "prose should be redacted"
    );

    // 2. Inspect the bundle
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("bundle")
        .arg("inspect")
        .arg(&bundle_path)
        .assert()
        .success()
        .stdout(predicate::str::contains("Root Selector: id:obs-1"))
        .stdout(predicate::str::contains("Repository Identity: repo-1"))
        .stdout(predicate::str::contains("agent_authored: 1"));

    // 3. Verify the bundle (text format)
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("bundle")
        .arg("verify")
        .arg(&bundle_path)
        .arg("--format")
        .arg("text")
        .assert()
        .success()
        .stdout(predicate::str::contains("Verification Verdict: PASS"))
        .stdout(predicate::str::contains("- Integrity: PASS"))
        .stdout(predicate::str::contains("- Coverage: PASS"))
        .stdout(predicate::str::contains("- Safety: PASS"));

    // 4. Verify the bundle (json format)
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("bundle")
        .arg("verify")
        .arg(&bundle_path)
        .arg("--format")
        .arg("json")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""ok": true"#));

    // 5. Tamper with the bundle and run verify
    let mut bundle: serde_json::Value = serde_json::from_str(&bundle_content).unwrap();
    if let Some(first_rec) = bundle
        .get_mut("records")
        .and_then(|r| r.as_array_mut())
        .and_then(|a| a.get_mut(0))
    {
        first_rec["hash"] = serde_json::Value::String("tampered_hash_value".to_owned());
    }
    let tampered_content = serde_json::to_string_pretty(&bundle).unwrap();
    fs::write(&bundle_path, tampered_content).unwrap();

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("bundle")
        .arg("verify")
        .arg(&bundle_path)
        .assert()
        .failure()
        .code(predicate::eq(1))
        .stdout(predicate::str::contains(r#""ok": false"#));

    // Test short alias `eg` binary for verify
    Command::cargo_bin("eg")
        .expect("binary should run")
        .arg("bundle")
        .arg("verify")
        .arg(&bundle_path)
        .assert()
        .failure()
        .code(predicate::eq(1));

    // 6. Test export failure due to coverage
    let mut uncited_obs = GraphRecord::node(
        "obs-uncited".to_owned(),
        NodeKind::Observation,
        None,
        None,
        None,
        "Uncited Observation".to_owned(),
    );
    if let GraphRecord::Node {
        text,
        schema_version,
        ..
    } = &mut uncited_obs
    {
        *text = Some("Uncited text".to_owned());
        *schema_version = 1;
    }

    let records2 = vec![uncited_obs];
    let mut graph_content2 = String::new();
    for r in records2 {
        graph_content2.push_str(&serde_json::to_string(&r).unwrap());
        graph_content2.push('\n');
    }
    let graph_path2 = temp.path().join("graph2.jsonl");
    fs::write(&graph_path2, graph_content2).unwrap();

    let bundle_path2 = temp.path().join("bundle2.json");
    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("bundle")
        .arg("export")
        .arg("--root-selector")
        .arg("id:obs-uncited")
        .arg("--graph")
        .arg(&graph_path2)
        .arg("--out")
        .arg(&bundle_path2)
        .assert()
        .failure()
        .code(predicate::eq(1))
        .stderr(predicate::str::contains(r#""code":"export_failed""#));
}
