//! Integration tests for the Antigravity transcript JSONL importer.

use aletheia_egregore::antigravity::{ImportOptions, import_antigravity};
use aletheia_egregore::ir::GraphRecord;
use std::path::PathBuf;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("corpus/fixtures/antigravity_sample.jsonl")
}

fn node_kinds(records: &[GraphRecord]) -> Vec<&str> {
    records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node { kind, .. } = r {
                Some(kind.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn nodes_of_kind<'a>(records: &'a [GraphRecord], target: &str) -> Vec<&'a GraphRecord> {
    records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node { kind, .. } = r {
                kind.as_str() == target
            } else {
                false
            }
        })
        .collect()
}

#[test]
fn basic_antigravity_import_kinds() {
    let path = fixture();
    let opts = ImportOptions::passthrough();
    let graph = import_antigravity(&path, &opts).expect("import should succeed");
    let records = graph.records();

    let kinds = node_kinds(records);

    assert!(
        kinds.contains(&"AgentSession"),
        "expected AgentSession; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"AgentRun"),
        "expected AgentRun; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"AgentTurn"),
        "expected AgentTurn; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"ToolCall"),
        "expected ToolCall; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"CommandRun"),
        "expected CommandRun; got: {kinds:?}"
    );
    assert!(
        kinds.contains(&"Verification"),
        "expected Verification; got: {kinds:?}"
    );
}

#[test]
fn antigravity_redaction_applies() {
    let path = fixture();
    let opts = ImportOptions::default(); // default uses v1 redaction
    let graph = import_antigravity(&path, &opts).expect("import should succeed");
    let records = graph.records();

    // Check that AgentTurn text is redacted (or processed)
    let turns = nodes_of_kind(records, "AgentTurn");
    assert!(!turns.is_empty());

    for turn in turns {
        if let GraphRecord::Node { text: Some(t), .. } = turn {
            assert!(!t.is_empty());
            // Verify redaction policy was stamped
            if let GraphRecord::Node {
                redaction_policy_version,
                ..
            } = turn
            {
                assert!(redaction_policy_version.is_some());
            }
        }
    }
}
