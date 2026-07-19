#![allow(missing_docs)]
//! Cross-file struct-literal `CONSTRUCTS` edges (issue #443).
//!
//! `eg scan` must emit a `CONSTRUCTS` edge from a constructing Symbol to the
//! constructed type's definition Symbol for each struct literal `Type { … }`,
//! resolving the type path with the same crate-root confinement the CALLS pass
//! uses, binding ONLY on a unique resolution, marking E0063 exhaustiveness, and
//! staying byte-deterministic.

use std::{fs, path::Path};

use aletheia_egregore::scan_repository_at_with_override;
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";
const REPO_ID: &str = "cross-file-constructs-fixture";

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

fn scan_jsonl(root: &Path) -> String {
    scan_repository_at_with_override(root, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize")
}

fn scan_fixture(root: &Path) -> Vec<Value> {
    parse_jsonl(&scan_jsonl(root))
}

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn symbol_id(records: &[Value], symbol_kind: &str, name: &str, path: &str) -> String {
    records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == symbol_kind
                && record["name"] == name
                && record["repo_relative_path"] == path
        })
        .unwrap_or_else(|| panic!("missing {symbol_kind} symbol {name} in {path}"))["id"]
        .as_str()
        .expect("symbol should have an ID")
        .to_owned()
}

fn constructs_edge<'a>(records: &'a [Value], source: &str, target: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "edge"
            && record["label"] == "CONSTRUCTS"
            && record["source"] == source
            && record["target"] == target
    })
}

fn constructs_edges_targeting<'a>(records: &'a [Value], target: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["label"] == "CONSTRUCTS"
                && record["target"] == target
        })
        .collect()
}

// ── (a) cross-file construction in one crate ──────────────────────────────────

#[test]
fn scan_emits_cross_file_constructs_edge_same_crate() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/deal.rs", "pub struct Deal {\n    pub id: u64,\n}\n"),
            (
                "src/make.rs",
                "use crate::deal::Deal;\n\npub fn make() -> Deal {\n    Deal { id: 1 }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let deal = symbol_id(&records, "struct", "deal::Deal", "src/deal.rs");
    let make = symbol_id(&records, "function", "make::make", "src/make.rs");

    let edge = constructs_edge(&records, &make, &deal)
        .unwrap_or_else(|| panic!("missing CONSTRUCTS edge from {make} to {deal}"));
    assert_eq!(
        edge["is_exhaustive"], true,
        "an exhaustive (no ..base) literal must be E0063-breakable: {edge}"
    );
    assert_eq!(edge["confidence"], "1.0");
}

// ── (b) cross-crate provable construction ─────────────────────────────────────

#[test]
fn scan_emits_cross_crate_constructs_edge() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "crates/crate_a/src/lib.rs",
                "pub struct Deal {\n    pub id: u64,\n}\n",
            ),
            (
                "crates/crate_b/src/lib.rs",
                "pub fn make() -> crate_a::Deal {\n    crate_a::Deal { id: 1 }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let deal = symbol_id(&records, "struct", "Deal", "crates/crate_a/src/lib.rs");
    let make = symbol_id(&records, "function", "make", "crates/crate_b/src/lib.rs");

    let edge = constructs_edge(&records, &make, &deal)
        .unwrap_or_else(|| panic!("missing cross-crate CONSTRUCTS edge from {make} to {deal}"));
    assert_eq!(edge["is_exhaustive"], true);
}

// ── (c) ambiguous same-named types stay unbound ───────────────────────────────

#[test]
fn ambiguous_type_construction_binds_nothing() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "crates/crate_a/src/lib.rs",
                "pub struct Deal {\n    pub id: u64,\n}\n",
            ),
            (
                "crates/crate_b/src/lib.rs",
                "pub struct Deal {\n    pub id: u64,\n}\n",
            ),
            (
                "crates/crate_c/src/lib.rs",
                "pub fn make() -> u64 {\n    let d = Deal { id: 1 };\n    d.id\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let deal_a = symbol_id(&records, "struct", "Deal", "crates/crate_a/src/lib.rs");
    let deal_b = symbol_id(&records, "struct", "Deal", "crates/crate_b/src/lib.rs");

    // An unqualified `Deal { … }` cannot be uniquely resolved across two crates,
    // so NO edge is minted to either candidate — ambiguity never picks one.
    assert!(
        constructs_edges_targeting(&records, &deal_a).is_empty(),
        "ambiguous construction must not bind crate_a::Deal"
    );
    assert!(
        constructs_edges_targeting(&records, &deal_b).is_empty(),
        "ambiguous construction must not bind crate_b::Deal"
    );
}

// ── (d) FRU vs exhaustive marker ──────────────────────────────────────────────

#[test]
fn fru_and_exhaustive_markers_collapse_with_or() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/deal.rs", "pub struct Deal {\n    pub id: u64,\n}\n"),
            (
                "src/make.rs",
                concat!(
                    "use crate::deal::Deal;\n\n",
                    "pub fn make_exhaustive() -> Deal {\n    Deal { id: 1 }\n}\n\n",
                    "pub fn make_fru(base: Deal) -> Deal {\n    Deal { id: 1, ..base }\n}\n\n",
                    "pub fn make_both(base: Deal) -> Deal {\n",
                    "    let _a = Deal { id: 1 };\n",
                    "    Deal { id: 2, ..base }\n}\n",
                ),
            ),
        ],
    );

    let records = scan_fixture(repo);
    let deal = symbol_id(&records, "struct", "deal::Deal", "src/deal.rs");
    let exhaustive = symbol_id(&records, "function", "make::make_exhaustive", "src/make.rs");
    let fru = symbol_id(&records, "function", "make::make_fru", "src/make.rs");
    let both = symbol_id(&records, "function", "make::make_both", "src/make.rs");

    assert_eq!(
        constructs_edge(&records, &exhaustive, &deal).expect("exhaustive edge")["is_exhaustive"],
        true,
        "a literal with no ..base is exhaustive (E0063 risk)"
    );
    assert_eq!(
        constructs_edge(&records, &fru, &deal).expect("fru edge")["is_exhaustive"],
        false,
        "a literal with ..base absorbs new fields (no E0063 risk)"
    );
    assert_eq!(
        constructs_edge(&records, &both, &deal).expect("both edge")["is_exhaustive"],
        true,
        "two collapsed sites OR their markers: any exhaustive site => true"
    );
}

// ── (e) Self literal inside an impl ───────────────────────────────────────────

#[test]
fn self_literal_constructs_the_impl_owner() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            concat!(
                "pub struct Deal {\n    pub id: u64,\n}\n\n",
                "impl Deal {\n    pub fn new() -> Self {\n        Self { id: 1 }\n    }\n}\n",
            ),
        )],
    );

    let records = scan_fixture(repo);
    let deal = symbol_id(&records, "struct", "Deal", "src/lib.rs");
    let new = symbol_id(&records, "method", "Deal::new", "src/lib.rs");

    let edge = constructs_edge(&records, &new, &deal)
        .unwrap_or_else(|| panic!("missing CONSTRUCTS edge from Self literal {new} -> {deal}"));
    assert_eq!(edge["is_exhaustive"], true);
}

// ── (f) enum-struct variant construction ──────────────────────────────────────

#[test]
fn enum_struct_variant_constructs_the_enum() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            concat!(
                "pub enum Shape {\n    Circle { r: u32 },\n    Square { s: u32 },\n}\n\n",
                "pub fn make() -> Shape {\n    Shape::Circle { r: 1 }\n}\n",
            ),
        )],
    );

    let records = scan_fixture(repo);
    let shape = symbol_id(&records, "enum", "Shape", "src/lib.rs");
    let make = symbol_id(&records, "function", "make", "src/lib.rs");

    let edge = constructs_edge(&records, &make, &shape).unwrap_or_else(|| {
        panic!("missing CONSTRUCTS edge from enum-variant literal {make} -> {shape}")
    });
    assert_eq!(edge["is_exhaustive"], true);
}

// ── (g) byte-determinism ──────────────────────────────────────────────────────

#[test]
fn constructs_scan_is_byte_deterministic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/deal.rs", "pub struct Deal {\n    pub id: u64,\n}\n"),
            (
                "src/make.rs",
                "use crate::deal::Deal;\n\npub fn make() -> Deal {\n    Deal { id: 1 }\n}\n",
            ),
        ],
    );

    let first = scan_jsonl(repo);
    let second = scan_jsonl(repo);
    assert_eq!(first, second, "repeated scans must be byte-identical");
    assert!(
        first.contains("\"CONSTRUCTS\""),
        "the fixture must actually emit a CONSTRUCTS edge"
    );
}
