#![allow(missing_docs)]

//! Query-lane capability manifest (issue #251).
//!
//! `eg query lanes` emits a deterministic JSON catalog of every public query
//! lane. The catalog is derived from the actual clap command surface (names,
//! one-line purposes, required inputs) joined with a per-lane static
//! classification (store mode, trust classes, citation flag, freshness), and
//! these tests pin the anti-drift contract: no orphan lanes (implemented but
//! unlisted), no phantom entries (listed but unimplemented), populated input
//! contracts on every entry, and byte-stable output.

use std::collections::BTreeSet;

use assert_cmd::Command;

/// Closed trust-class vocabulary (`crate::query::TrustClass::ALL`, doc order).
const TRUST_VOCAB: &[&str] = &[
    "source_derived",
    "verification_evidence",
    "agent_verified",
    "agent_unverified",
    "agent_contradicted",
    "project_state",
    "artifact",
    "runtime_observation",
    "other",
];

/// Closed freshness vocabulary (`src/cli/lanes.rs` documents each value).
const FRESHNESS_VOCAB: &[&str] = &[
    "ingest_snapshot",
    "valid_time",
    "working_tree",
    "store_live",
];

/// Run `eg query lanes` in a fresh empty directory: no store, no config, no
/// network. Returns raw stdout bytes.
fn run_lanes(extra_args: &[&str]) -> Vec<u8> {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let output = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("lanes")
        .args(extra_args)
        .current_dir(temp.path())
        .output()
        .expect("lanes command should run");
    assert!(
        output.status.success(),
        "eg query lanes failed; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "eg query lanes wrote to stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn lanes_value() -> serde_json::Value {
    let stdout = run_lanes(&[]);
    serde_json::from_slice(&stdout).expect("lanes output should be valid JSON")
}

fn lane_entries(value: &serde_json::Value) -> &[serde_json::Value] {
    value
        .get("lanes")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .expect("manifest should have a top-level \"lanes\" array")
}

/// Public `eg query` subcommand names scraped from `eg query --help`.
///
/// Only lines that start a command entry (exactly two leading spaces, then
/// the kebab-case name) count, so wrapped description continuations (which
/// are indented much deeper) can never be mistaken for a lane.
fn query_subcommand_names() -> BTreeSet<String> {
    let output = Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("query")
        .arg("--help")
        .output()
        .expect("eg query --help should run");
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    let mut names = BTreeSet::new();
    let mut in_commands = false;
    for line in help.lines() {
        if line.trim_end() == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if line.trim().is_empty() || !line.starts_with(' ') {
            break;
        }
        // Command entries start with exactly two spaces; wrapped description
        // lines are indented to the description column.
        if let Some(rest) = line.strip_prefix("  ")
            && !rest.starts_with(' ')
        {
            let name: String = rest.split_whitespace().next().unwrap_or_default().into();
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                names.insert(name);
            }
        }
    }
    names
}

#[test]
fn lanes_command_works_on_empty_dir_with_no_store() {
    // Acceptance: no network, no ingested store, fresh clone with an empty
    // data dir. Success + empty stderr already asserted by run_lanes.
    let value = lanes_value();
    assert!(
        !lane_entries(&value).is_empty(),
        "manifest should list at least one lane"
    );
}

#[test]
fn lanes_json_is_byte_stable_across_runs() {
    let first = run_lanes(&[]);
    let second = run_lanes(&[]);
    assert_eq!(
        first, second,
        "default JSON output must be byte-for-byte stable across repeated runs"
    );
}

#[test]
fn lanes_are_canonically_ordered_by_id() {
    let value = lanes_value();
    let ids: Vec<&str> = lane_entries(&value)
        .iter()
        .map(|e| {
            e.get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "lanes must be canonically ordered by id");
    assert!(
        ids.iter().collect::<BTreeSet<_>>().len() == ids.len(),
        "lane ids must be unique"
    );
}

#[test]
fn lanes_cover_every_query_subcommand_exactly() {
    // The anti-drift contract: the manifest is derived from the actual
    // command surface. `lanes` itself is the meta-command, not a query lane,
    // so it is excluded from both sides of the comparison.
    let mut surface = query_subcommand_names();
    assert!(
        surface.remove("lanes"),
        "eg query --help should list the lanes subcommand"
    );
    // `help` is clap's auto-generated meta-subcommand, not a query lane.
    surface.remove("help");
    let value = lanes_value();
    let manifest: BTreeSet<String> = lane_entries(&value)
        .iter()
        .map(|e| {
            e.get("id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        })
        .collect();
    let orphans: Vec<_> = surface.difference(&manifest).collect();
    let phantoms: Vec<_> = manifest.difference(&surface).collect();
    assert!(
        orphans.is_empty(),
        "implemented but unlisted lanes (orphans): {orphans:?}"
    );
    assert!(
        phantoms.is_empty(),
        "listed but unimplemented lanes (phantoms): {phantoms:?}"
    );
}

#[test]
fn lanes_entries_have_populated_contracts() {
    let value = lanes_value();
    for entry in lane_entries(&value) {
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let purpose = entry
            .get("purpose")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            !purpose.is_empty(),
            "lane {id} must have a one-line purpose"
        );
        assert!(
            !purpose.contains('\n'),
            "lane {id} purpose must be a single line"
        );
        let inputs = entry
            .get("required_inputs")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len);
        assert!(
            inputs > 0,
            "lane {id} must declare at least one required input"
        );
        let store_mode = entry
            .get("store_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(
            store_mode == "structural" || store_mode == "semantic",
            "lane {id} has invalid store_mode {store_mode:?}"
        );
        let trust: Vec<&str> = entry
            .get("trust_classes")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert!(!trust.is_empty(), "lane {id} must declare trust classes");
        for class in &trust {
            assert!(
                TRUST_VOCAB.contains(class),
                "lane {id} has unknown trust class {class:?}"
            );
        }
        assert!(
            entry
                .get("citable_handles")
                .and_then(serde_json::Value::as_bool)
                .is_some(),
            "lane {id} must declare citable_handles as a boolean"
        );
        let freshness: Vec<&str> = entry
            .get("freshness")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        assert!(
            !freshness.is_empty(),
            "lane {id} must declare freshness semantics"
        );
        for f in &freshness {
            assert!(
                FRESHNESS_VOCAB.contains(f),
                "lane {id} has unknown freshness value {f:?}"
            );
        }
    }
}

#[test]
fn lanes_semantic_lanes_require_embed_store() {
    let value = lanes_value();
    let mut semantic = BTreeSet::new();
    for entry in lane_entries(&value) {
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let store_mode = entry
            .get("store_mode")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let inputs: Vec<&str> = entry
            .get("required_inputs")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default();
        if store_mode == "semantic" {
            semantic.insert(id.to_owned());
            assert!(
                inputs.iter().any(|i| i.contains("--data-dir")),
                "semantic lane {id} must require a --data-dir (embedded --embed store)"
            );
        }
    }
    assert_eq!(
        semantic,
        expected_semantic_lanes(),
        "exactly the embedding lanes should be semantic"
    );
}

/// The embedding lanes exist only with the `embeddings` feature; without it
/// no lane may claim the semantic store mode.
#[cfg(feature = "embeddings")]
fn expected_semantic_lanes() -> BTreeSet<String> {
    BTreeSet::from([
        "semantic".to_owned(),
        "semantic-context".to_owned(),
        "semantic-memory".to_owned(),
        // `similar` ranks by stored embeddings (issue #154); it needs the
        // vector index but no query-side model.
        "similar".to_owned(),
    ])
}

/// The embedding lanes exist only with the `embeddings` feature; without it
/// no lane may claim the semantic store mode.
#[cfg(not(feature = "embeddings"))]
fn expected_semantic_lanes() -> BTreeSet<String> {
    BTreeSet::new()
}

#[test]
fn lanes_text_format_is_a_human_skimmable_table() {
    let stdout = run_lanes(&["--format", "text"]);
    let text = String::from_utf8(stdout).expect("text output should be UTF-8");
    let header = text.lines().next().unwrap_or_default();
    for column in ["LANE", "PURPOSE", "STORE", "TRUST", "CITABLE", "FRESHNESS"] {
        assert!(
            header.contains(column),
            "text table header should contain {column}; got: {header}"
        );
    }
    let value = lanes_value();
    for entry in lane_entries(&value) {
        let id = entry
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        assert!(text.contains(id), "text table should list lane {id}");
    }
}
