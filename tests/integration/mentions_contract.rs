//! Contract for issue #442: the `MENTIONS` edge label is reserved, not emitted.
//!
//! The README and PRD schema document `MENTIONS` as a reserved edge label that
//! no current language extractor emits; resolved usage is recorded as
//! `REFERENCES`. These tests lock both halves of that contract:
//!
//! 1. A scan of the Rust fixture emits zero `MENTIONS` edges (the "never
//!    emitted" half), while the live `REFERENCES` edge is present.
//! 2. Consumer-facing contract documents (`README.md`, `skills/egregore/SKILL.md`)
//!    must not present `MENTIONS` as a live extractor edge (the "documented as
//!    unsupported" half), so no downstream consumer mistakes "never emitted"
//!    for "none found".

#![allow(missing_docs)]

use std::{fs, path::PathBuf};

use aletheia_egregore::scan_repository;
use serde_json::Value;

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scan_edge_labels(repo: &std::path::Path) -> Vec<String> {
    let jsonl = scan_repository(repo)
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record["record_type"] == "edge")
        .filter_map(|record| {
            record
                .get("label")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn rust_scan_emits_no_mentions_edges_but_references_are_present() {
    let labels = scan_edge_labels(&fixture_repo());
    let mentions = labels
        .iter()
        .filter(|label| label.as_str() == "MENTIONS")
        .count();
    assert_eq!(
        mentions, 0,
        "MENTIONS is reserved, not emitted: a Rust scan must emit zero MENTIONS edges"
    );
    assert!(
        labels.iter().any(|label| label.as_str() == "REFERENCES"),
        "resolved usage is recorded as REFERENCES; expected REFERENCES edges in the scan"
    );
}

#[test]
fn consumer_docs_do_not_present_mentions_as_a_live_extractor_edge() {
    let root = workspace_root();
    let readme = fs::read_to_string(root.join("README.md")).expect("README.md should be readable");
    let skill = fs::read_to_string(root.join("skills/egregore/SKILL.md"))
        .expect("skills/egregore/SKILL.md should be readable");

    assert!(
        readme.contains("MENTIONS` is a reserved edge label"),
        "README must document MENTIONS as a reserved, unemitted edge label"
    );
    let offending: Vec<&str> = skill
        .lines()
        .filter(|line| line.contains("IMPORTS, MENTIONS)"))
        .collect();
    assert!(
        offending.is_empty(),
        "SKILL.md must not present MENTIONS as a live extractor edge: {offending:?}"
    );
}
