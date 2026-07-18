#![allow(missing_docs)]
//! Cross-file call resolution (issue #152).
//!
//! `eg scan` must emit `CALLS` edges whose targets are defined in *other*
//! files of the same repository, labeled with a `resolution` status
//! (`resolved` / `ambiguous` / `unresolved`), without regressing the
//! comment/string/substring precision guarantees coordinated with #134.

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use aletheia_egregore::{scan_repository_at_with_override, scan_repository_history_with_override};
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-07T00:00:00Z";
const REPO_ID: &str = "cross-file-calls-fixture";

fn write_fixture(root: &Path, files: &[(&str, &str)]) {
    for (relative, contents) in files {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
            .expect("fixture parent dir should be created");
        fs::write(path, contents).expect("fixture file should be written");
    }
}

fn scan_fixture(root: &Path) -> Vec<Value> {
    let jsonl = scan_repository_at_with_override(root, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    parse_jsonl(&jsonl)
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

fn calls_edge<'a>(records: &'a [Value], source: &str, target: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "edge"
            && record["label"] == "CALLS"
            && record["source"] == source
            && record["target"] == target
    })
}

fn assert_calls_edge_with_resolution(
    records: &[Value],
    source: &str,
    target: &str,
    resolution: &str,
) {
    let edge = calls_edge(records, source, target).unwrap_or_else(|| {
        panic!("missing CALLS edge from {source} to {target} (expected {resolution})")
    });
    assert_eq!(
        edge["resolution"], resolution,
        "CALLS edge from {source} to {target} should be labeled {resolution}, got: {edge}"
    );
}

#[test]
fn scan_emits_cross_file_call_edges_for_functions_and_methods() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn render(&self) -> usize {\n        self.value\n    }\n}\n\npub fn shared_helper() -> usize {\n    7\n}\n",
            ),
            (
                "src/beta.rs",
                "use crate::alpha::shared_helper;\n\npub fn beta_caller() -> usize {\n    shared_helper()\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub fn gamma_caller(w: &crate::alpha::Widget) -> usize {\n    crate::alpha::shared_helper() + w.render()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let helper = symbol_id(&records, "function", "alpha::shared_helper", "src/alpha.rs");
    let render = symbol_id(&records, "method", "alpha::Widget::render", "src/alpha.rs");
    let beta_caller = symbol_id(&records, "function", "beta::beta_caller", "src/beta.rs");
    let gamma_caller = symbol_id(&records, "function", "gamma::gamma_caller", "src/gamma.rs");

    // Direct call through an import, path-qualified call, and method call all
    // resolve across the file boundary to the single in-repo definition.
    assert_calls_edge_with_resolution(&records, &beta_caller, &helper, "resolved");
    assert_calls_edge_with_resolution(&records, &gamma_caller, &helper, "resolved");
    assert_calls_edge_with_resolution(&records, &gamma_caller, &render, "resolved");
}

#[test]
fn ambiguous_simple_names_emit_labeled_edges_to_all_candidates() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/alpha.rs", "pub fn dupe() -> usize {\n    1\n}\n"),
            ("src/delta.rs", "pub fn dupe() -> usize {\n    2\n}\n"),
            (
                "src/beta.rs",
                "pub fn calls_dupe() -> usize {\n    dupe()\n}\n",
            ),
            (
                "src/epsilon.rs",
                "pub fn calls_alpha_dupe() -> usize {\n    crate::alpha::dupe()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let alpha_dupe = symbol_id(&records, "function", "alpha::dupe", "src/alpha.rs");
    let delta_dupe = symbol_id(&records, "function", "delta::dupe", "src/delta.rs");
    let calls_dupe = symbol_id(&records, "function", "beta::calls_dupe", "src/beta.rs");
    let calls_alpha_dupe = symbol_id(
        &records,
        "function",
        "epsilon::calls_alpha_dupe",
        "src/epsilon.rs",
    );

    // An unqualified call matching two in-repo definitions is labeled
    // ambiguous and carries an edge to every candidate.
    assert_calls_edge_with_resolution(&records, &calls_dupe, &alpha_dupe, "ambiguous");
    assert_calls_edge_with_resolution(&records, &calls_dupe, &delta_dupe, "ambiguous");

    // A path-qualified call narrows to exactly one candidate.
    assert_calls_edge_with_resolution(&records, &calls_alpha_dupe, &alpha_dupe, "resolved");
    assert!(
        calls_edge(&records, &calls_alpha_dupe, &delta_dupe).is_none(),
        "path-qualified call must not fan out to the non-matching candidate"
    );
}

#[test]
fn unresolved_external_calls_are_labeled_not_dropped_or_invented() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("src/alpha.rs", "pub fn in_repo() -> usize {\n    1\n}\n"),
            (
                "src/beta.rs",
                "pub fn uses_external() -> usize {\n    external_dep::render_widget()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let uses_external = symbol_id(&records, "function", "beta::uses_external", "src/beta.rs");

    // The unresolved call is recorded against a Diagnostic node, never an
    // invented in-repo Symbol.
    let diagnostic = records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "external_dep::render_widget"
                && record["repo_relative_path"] == "src/beta.rs"
        })
        .expect("unresolved call should emit a Diagnostic node");
    let diagnostic_id = diagnostic["id"]
        .as_str()
        .expect("diagnostic should have ID");
    assert_calls_edge_with_resolution(&records, &uses_external, diagnostic_id, "unresolved");

    let symbol_ids: Vec<&str> = records
        .iter()
        .filter(|record| record["record_type"] == "node" && record["kind"] == "Symbol")
        .filter_map(|record| record["id"].as_str())
        .collect();
    for target in symbol_ids {
        assert!(
            calls_edge(&records, &uses_external, target).is_none(),
            "unresolved external call must not invent an edge to in-repo symbol {target}"
        );
    }
}

#[test]
fn comment_string_and_substring_mentions_produce_no_cross_file_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub fn quiet_target() -> usize {\n    3\n}\n",
            ),
            (
                "src/beta.rs",
                "// quiet_target() is discussed in this comment only\npub fn documented() -> &'static str {\n    \"call quiet_target() later\"\n}\n\npub fn quiet_target_extended() -> usize {\n    4\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let quiet_target = symbol_id(&records, "function", "alpha::quiet_target", "src/alpha.rs");

    let offending: Vec<&Value> = records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge"
                && record["target"] == quiet_target.as_str()
                && matches!(
                    record["label"].as_str(),
                    Some("CALLS" | "REFERENCES" | "MENTIONS")
                )
        })
        .collect();
    assert!(
        offending.is_empty(),
        "comment/string/substring occurrences must not produce cross-file edges: {offending:?}"
    );
}

#[test]
fn cross_file_edges_are_byte_stable_across_repeated_scans() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub fn shared_helper() -> usize {\n    7\n}\npub fn dupe() {}\n",
            ),
            (
                "src/beta.rs",
                "pub fn beta_caller() -> usize {\n    shared_helper() + external_dep::widget()\n}\npub fn dupe() {}\npub fn calls_dupe() {\n    dupe();\n}\n",
            ),
        ],
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
    assert!(
        first.contains(r#""resolution":"resolved""#)
            && first.contains(r#""resolution":"ambiguous""#)
            && first.contains(r#""resolution":"unresolved""#),
        "stability check must cover all three resolution statuses: {first}"
    );
}

#[test]
fn incremental_scan_emits_and_retires_cross_file_call_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    write_fixture(
        &repo,
        &[
            (
                "src/alpha.rs",
                "pub fn shared_helper() -> usize {\n    7\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn beta_caller() -> usize {\n    shared_helper()\n}\n",
            ),
        ],
    );
    let cache_path = temp.path().join("codegraph-cache.json");

    let first = aletheia_egregore::incremental::scan_repository_incremental_at(
        &repo,
        &cache_path,
        FIXED_TIME,
    )
    .expect("first incremental scan should work");
    let first_records = parse_jsonl(
        &first
            .graph
            .to_jsonl()
            .expect("first incremental graph should serialize"),
    );
    let helper = symbol_id(
        &first_records,
        "function",
        "alpha::shared_helper",
        "src/alpha.rs",
    );
    let beta_caller = symbol_id(
        &first_records,
        "function",
        "beta::beta_caller",
        "src/beta.rs",
    );
    assert_calls_edge_with_resolution(&first_records, &beta_caller, &helper, "resolved");
    let edge_id = calls_edge(&first_records, &beta_caller, &helper)
        .expect("cross-file edge should exist")["id"]
        .as_str()
        .expect("edge should have ID")
        .to_owned();

    // Removing the call retires the edge with a tombstone on the next scan.
    fs::write(
        repo.join("src/beta.rs"),
        "pub fn beta_caller() -> usize {\n    9\n}\n",
    )
    .expect("fixture should update");
    let second = aletheia_egregore::incremental::scan_repository_incremental_at(
        &repo,
        &cache_path,
        "2026-06-08T00:00:00Z",
    )
    .expect("second incremental scan should work");
    let second_records = parse_jsonl(
        &second
            .graph
            .to_jsonl()
            .expect("second incremental graph should serialize"),
    );
    assert!(
        calls_edge(&second_records, &beta_caller, &helper).is_none(),
        "removed call must not re-emit the cross-file edge"
    );
    assert!(
        second_records.iter().any(|record| {
            record["record_type"] == "tombstone" && record["deleted_id"] == edge_id.as_str()
        }),
        "stale cross-file edge must be tombstoned so persisted stores can retire it"
    );
}

// --- Trait-method call resolution (issue #390) -----------------------------

#[test]
fn scan_binds_trait_qualified_path_and_receiver_calls_to_trait_methods() {
    // Both a signature-only (`read`) and a default-bodied (`name`) trait
    // method must be reachable by a trait-qualified `Device::m()` path call
    // and by a `x.m()` receiver call, across a file boundary.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait Device {\n    fn read(&self) -> u32;\n    fn name(&self) -> u32 {\n        7\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn path_caller() -> u32 {\n    Device::read() + Device::name()\n}\n\npub fn receiver_caller(d: &crate::alpha::Device) -> u32 {\n    d.read() + d.name()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    // Trait methods keep kind "function" and their trait-free qualified name.
    let read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let name = symbol_id(&records, "function", "alpha::name", "src/alpha.rs");
    let path_caller = symbol_id(&records, "function", "beta::path_caller", "src/beta.rs");
    let receiver_caller = symbol_id(&records, "function", "beta::receiver_caller", "src/beta.rs");

    // `Device::read()` / `Device::name()` path calls narrow to the exact
    // trait method (unique → resolved).
    assert_calls_edge_with_resolution(&records, &path_caller, &read, "resolved");
    assert_calls_edge_with_resolution(&records, &path_caller, &name, "resolved");
    // `d.read()` / `d.name()` receiver calls reach the trait methods too.
    assert_calls_edge_with_resolution(&records, &receiver_caller, &read, "resolved");
    assert_calls_edge_with_resolution(&records, &receiver_caller, &name, "resolved");
}

#[test]
fn bare_call_never_binds_a_trait_method() {
    // NO-WRONG-EDGE: a bare `read()` (`Direct`) can never invoke a trait
    // method. It must resolve to a Diagnostic (unresolved), never the trait
    // method Symbol — the corollary false-bind closure.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait Device {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn bare_caller() -> u32 {\n    read()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let bare_caller = symbol_id(&records, "function", "beta::bare_caller", "src/beta.rs");

    assert!(
        calls_edge(&records, &bare_caller, &read).is_none(),
        "a bare read() call must not false-bind the trait method"
    );
    // It is honestly unresolved against a Diagnostic instead.
    let diagnostic = records
        .iter()
        .find(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "read"
                && record["repo_relative_path"] == "src/beta.rs"
        })
        .expect("bare unresolved call should emit a Diagnostic node");
    let diagnostic_id = diagnostic["id"].as_str().expect("diagnostic id");
    assert_calls_edge_with_resolution(&records, &bare_caller, diagnostic_id, "unresolved");
}

#[test]
fn trait_qualified_path_binds_only_the_named_trait() {
    // NO-WRONG-EDGE: `Aa::read()` with two traits `Aa` and `Bb` each
    // declaring `read`, plus an unrelated free function `read`, binds ONLY
    // `Aa::read` (exact owner suffix) — never `Bb::read`, never the free fn.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait Aa {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait Bb {\n    fn read(&self) -> u32;\n}\n",
            ),
            ("src/delta.rs", "pub fn read() -> u32 {\n    0\n}\n"),
            (
                "src/beta.rs",
                "pub fn caller() -> u32 {\n    Aa::read()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let aa_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let bb_read = symbol_id(&records, "function", "gamma::read", "src/gamma.rs");
    let free_read = symbol_id(&records, "function", "delta::read", "src/delta.rs");
    let caller = symbol_id(&records, "function", "beta::caller", "src/beta.rs");

    assert_calls_edge_with_resolution(&records, &caller, &aa_read, "resolved");
    assert!(
        calls_edge(&records, &caller, &bb_read).is_none(),
        "Aa::read() must not bind the differently-named trait Bb::read"
    );
    assert!(
        calls_edge(&records, &caller, &free_read).is_none(),
        "Aa::read() must not bind an unrelated free function read"
    );
}

#[test]
fn receiver_call_to_two_trait_methods_is_ambiguous_to_both() {
    // NO-WRONG-EDGE: a `x.read()` receiver call where two traits declare
    // `read` fans out to BOTH as ambiguous labeled edges — never a silent
    // winner.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait Aa {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait Bb {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn caller(x: &u32) -> u32 {\n    x.read()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let aa_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let bb_read = symbol_id(&records, "function", "gamma::read", "src/gamma.rs");
    let caller = symbol_id(&records, "function", "beta::caller", "src/beta.rs");

    assert_calls_edge_with_resolution(&records, &caller, &aa_read, "ambiguous");
    assert_calls_edge_with_resolution(&records, &caller, &bb_read, "ambiguous");
}

#[test]
fn receiver_call_with_no_matching_method_invents_no_edge() {
    // NO-WRONG-EDGE: a `x.write()` receiver call with no in-repo `write`
    // definition mints no edge and no Diagnostic (external by construction).
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait Device {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn caller(x: &u32) -> u32 {\n    x.write()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let caller = symbol_id(&records, "function", "beta::caller", "src/beta.rs");

    assert!(
        calls_edge(&records, &caller, &read).is_none(),
        "x.write() must not bind the unrelated trait method read"
    );
    // A receiver call with no candidate is external — no Diagnostic either.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "write"
        }),
        "an unresolved receiver method call must not emit a Diagnostic node"
    );
}

#[test]
fn trait_method_call_edges_are_byte_stable_across_repeated_scans() {
    // Determinism guard for the new trait-method recall + ambiguity paths.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait Aa {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait Bb {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn path_caller() -> u32 {\n    Aa::read()\n}\npub fn receiver_caller(x: &u32) -> u32 {\n    x.read()\n}\n",
            ),
        ],
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
    assert!(
        first.contains(r#""resolution":"resolved""#)
            && first.contains(r#""resolution":"ambiguous""#),
        "stability check must cover the resolved and ambiguous trait-method paths: {first}"
    );
}

// --- Trait-method attribution is DIRECT-membership only (issue #390) --------

#[test]
fn block_local_fn_in_a_trait_method_body_has_corrected_identity_but_is_not_cross_file_callable() {
    // Issue #413 (round 3, Codex finding A) — flips the earlier #412 recall
    // assertion, coordinator-authorized. A `fn helper` defined block-local
    // inside a default trait method body keeps its CORRECTED identity: a plain
    // free function (kind `function`, module-qualified `alpha::helper`, never a
    // trait method). But it is lexically unreachable through the flat call
    // index, so it is NOT exported as a call candidate: its own bare `helper()`
    // call is now UNRESOLVED.
    //
    // WHY UNRESOLVED (do not "fix" this back): a block-local fn is lexically
    // unreachable through the flat cross-file/same-file call index; keeping it
    // resolvable would reopen the wrong-edge vector where a bare call elsewhere
    // binds the buried item (Codex round 3). Scoped block-local recall is
    // restored by a follow-up issue.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/alpha.rs",
            "pub trait T {\n    fn f(&self) -> u32 {\n        fn helper() -> u32 {\n            3\n        }\n        helper()\n    }\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    // Corrected identity retained: a plain free function, module-qualified name,
    // NOT `alpha::T::helper`.
    let helper = symbol_id(&records, "function", "alpha::helper", "src/alpha.rs");
    let f = symbol_id(&records, "function", "alpha::f", "src/alpha.rs");
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["name"] == "alpha::T::helper"
        }),
        "the block-local helper must not be a trait-method target (identity stays corrected)"
    );

    // The bare `helper()` call is UNRESOLVED: the block-local fn is not a call
    // candidate. Keeping it resolvable would reopen the wrong-edge vector.
    assert!(
        calls_edge(&records, &f, &helper).is_none(),
        "a block-local fn is lexically unreachable through the flat call index; keeping it resolvable would reopen the wrong-edge vector (Codex round 3); scoped block-local recall is restored by a follow-up issue"
    );
}

#[test]
fn block_local_fn_in_a_trait_method_is_not_a_trait_method_target() {
    // The block-local `helper` must NOT receive the enclosing trait as an
    // owner segment: a `T::helper()` trait-qualified path call must therefore
    // find NO candidate (helper's segments are `[alpha, helper]`, not
    // `[alpha, T, helper]`), proving it was not mis-attributed as a trait
    // method. No-wrong-edge in the other direction.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn f(&self) -> u32 {\n        fn helper() -> u32 {\n            3\n        }\n        helper()\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn caller() -> u32 {\n    crate::alpha::T::helper()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let helper = symbol_id(&records, "function", "alpha::helper", "src/alpha.rs");
    let caller = symbol_id(&records, "function", "beta::caller", "src/beta.rs");

    assert!(
        calls_edge(&records, &caller, &helper).is_none(),
        "T::helper() must not bind a block-local free function as a trait method"
    );
}

#[test]
fn block_local_fn_in_an_impl_method_body_has_corrected_identity_but_is_not_cross_file_callable() {
    // Issue #413 (round 3, Codex finding A). A `fn helper` defined block-local
    // inside an impl method body keeps its CORRECTED identity: a plain free
    // function (kind `function`, module-qualified `alpha::helper`, never a
    // `method` `alpha::S::helper`, no owner DEFINES). But it is lexically
    // unreachable through the flat call index, so it is NOT exported as a call
    // candidate: its own bare `helper()` call is now UNRESOLVED.
    //
    // WHY UNRESOLVED (do not "fix" this back): a block-local fn is lexically
    // unreachable through the flat cross-file/same-file call index; keeping it
    // resolvable would reopen the wrong-edge vector where a bare call elsewhere
    // binds the buried item (Codex round 3). Scoped block-local recall is
    // restored by a follow-up issue.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/alpha.rs",
            "pub struct S;\nimpl S {\n    pub fn m(&self) -> u32 {\n        fn helper() -> u32 {\n            5\n        }\n        helper()\n    }\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    // Corrected identity retained: a plain free function, module-qualified name,
    // NOT `alpha::S::helper`.
    let helper = symbol_id(&records, "function", "alpha::helper", "src/alpha.rs");
    let m = symbol_id(&records, "method", "alpha::S::m", "src/alpha.rs");
    assert_ne!(helper, m, "the nested helper is a distinct symbol from m");
    // The nested helper must NOT be recorded as a method under S.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["name"] == "alpha::S::helper"
        }),
        "the impl-nested helper must not be mis-attributed as method S::helper"
    );

    // The bare `helper()` call is UNRESOLVED: the block-local fn is not a call
    // candidate. Keeping it resolvable would reopen the wrong-edge vector.
    assert!(
        calls_edge(&records, &m, &helper).is_none(),
        "a block-local fn is lexically unreachable through the flat call index; keeping it resolvable would reopen the wrong-edge vector (Codex round 3); scoped block-local recall is restored by a follow-up issue"
    );
}

#[test]
fn block_local_fn_in_an_impl_method_is_not_an_impl_method_target() {
    // NO-WRONG-EDGE (issue #413): the block-local `helper` must NOT receive the
    // impl owner as an owner segment: an `S::helper()` path call must therefore
    // find NO candidate (helper's segments are `[alpha, helper]`, not
    // `[alpha, S, helper]`), proving it was not mis-attributed as an impl method.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub struct S;\nimpl S {\n    pub fn m(&self) -> u32 {\n        fn helper() -> u32 {\n            5\n        }\n        helper()\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn caller() -> u32 {\n    crate::alpha::S::helper()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let helper = symbol_id(&records, "function", "alpha::helper", "src/alpha.rs");
    let caller = symbol_id(&records, "function", "beta::caller", "src/beta.rs");

    assert!(
        calls_edge(&records, &caller, &helper).is_none(),
        "S::helper() must not bind a block-local free function as an impl method"
    );
}

#[test]
fn bare_call_does_not_bind_a_block_local_fn_buried_in_an_impl_method() {
    // NO-WRONG-EDGE (issue #413 round 3, Codex finding A): a `fn helper` buried
    // in an impl method body is lexically unreachable from a separate module's
    // `g()`. It must NOT be a cross-file call candidate — `g`'s bare `helper()`
    // (whose only in-repo same-name def is the buried block-local) stays
    // UNRESOLVED, never a confident wrong resolved edge to the buried item.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub struct S;\nimpl S {\n    pub fn m(&self) -> u32 {\n        fn helper() -> u32 {\n            5\n        }\n        helper()\n    }\n}\n",
            ),
            ("src/beta.rs", "pub fn g() -> u32 {\n    helper()\n}\n"),
        ],
    );

    let records = scan_fixture(repo);
    let helper = symbol_id(&records, "function", "alpha::helper", "src/alpha.rs");
    let g = symbol_id(&records, "function", "beta::g", "src/beta.rs");

    assert!(
        calls_edge(&records, &g, &helper).is_none(),
        "g()'s bare helper() must not bind the block-local fn buried in impl S::m"
    );
}

#[test]
fn module_fn_call_not_ambiguous_with_same_named_block_local_fn() {
    // NO-WRONG-EDGE (issue #413 round 3, Codex finding A): a module-level
    // `fn helper` and a same-named block-local `fn helper` buried in `impl S::m`
    // must not collide. A separate module's `g()` calling `helper()` resolves to
    // the MODULE `helper` ONLY (single `resolved`), never an ambiguous fan-out
    // that includes the lexically-unreachable block-local.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub struct S;\nimpl S {\n    pub fn m(&self) -> u32 {\n        fn helper() -> u32 {\n            5\n        }\n        helper()\n    }\n}\n",
            ),
            ("src/gamma.rs", "pub fn helper() -> u32 {\n    7\n}\n"),
            (
                "src/beta.rs",
                "use crate::gamma::helper;\npub fn g() -> u32 {\n    helper()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let module_helper = symbol_id(&records, "function", "gamma::helper", "src/gamma.rs");
    let g = symbol_id(&records, "function", "beta::g", "src/beta.rs");

    // The bare call binds the single module helper, resolved (not ambiguous with
    // the lexically-unreachable block-local `alpha::helper`).
    assert_calls_edge_with_resolution(&records, &g, &module_helper, "resolved");
    // No ambiguous edge from g to any helper — the block-local is not a candidate.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "edge"
                && record["label"] == "CALLS"
                && record["source"] == g
                && record["resolution"] == "ambiguous"
        }),
        "g()'s helper() must resolve to the module helper only, never ambiguous with a block-local"
    );
}

// --- In-trait `Self::` calls carry the trait owner (issue #390) -------------

#[test]
fn in_trait_self_call_resolves_to_the_trait_associated_fn() {
    // REGRESSION (issue #390): a `Self::make()` call inside a default trait
    // method names the trait's OWN associated fn. In a trait body there is no
    // `impl_context`, so before the fix `Self` was stripped with no owner
    // substituted, collapsing the call to `["make"]` — which the tightened
    // free-function pool (trait methods excluded) no longer binds. Substitute
    // the enclosing trait as the owner so `Self::make()` -> `["T","make"]`
    // resolves to the trait method via the multi-segment suffix match.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/alpha.rs",
            "pub trait T {\n    fn make() -> u32;\n    fn f(&self) -> u32 {\n        Self::make()\n    }\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let make = symbol_id(&records, "function", "alpha::make", "src/alpha.rs");
    let f = symbol_id(&records, "function", "alpha::f", "src/alpha.rs");

    assert_calls_edge_with_resolution(&records, &f, &make, "resolved");
}

#[test]
fn in_trait_self_call_mints_no_wrong_edge() {
    // NO-WRONG-EDGE: `Self::make()` in trait T binds ONLY `T::make`, never a
    // free function `make`, never another trait's `U::make`; and a
    // `Self::other()` naming an associated fn the trait does not declare stays
    // unresolved (no invented edge) — the substitution is an exact suffix
    // match, so it is provable, never a wildcard.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn make() -> u32;\n    fn f(&self) -> u32 {\n        Self::make()\n    }\n    fn g(&self) -> u32 {\n        Self::other()\n    }\n}\n",
            ),
            ("src/gamma.rs", "pub fn make() -> u32 {\n    0\n}\n"),
            ("src/delta.rs", "pub trait U {\n    fn make() -> u32;\n}\n"),
        ],
    );

    let records = scan_fixture(repo);
    let t_make = symbol_id(&records, "function", "alpha::make", "src/alpha.rs");
    let free_make = symbol_id(&records, "function", "gamma::make", "src/gamma.rs");
    let u_make = symbol_id(&records, "function", "delta::make", "src/delta.rs");
    let f = symbol_id(&records, "function", "alpha::f", "src/alpha.rs");
    let g = symbol_id(&records, "function", "alpha::g", "src/alpha.rs");

    // Self::make() binds only the trait's own associated fn.
    assert_calls_edge_with_resolution(&records, &f, &t_make, "resolved");
    assert!(
        calls_edge(&records, &f, &free_make).is_none(),
        "Self::make() must not bind an unrelated free function make"
    );
    assert!(
        calls_edge(&records, &f, &u_make).is_none(),
        "Self::make() must not bind a different trait's U::make"
    );
    // Self::other() names nothing the trait declares — no invented edge.
    assert!(
        calls_edge(&records, &g, &t_make).is_none(),
        "Self::other() must not wildcard onto T::make"
    );
    assert!(
        calls_edge(&records, &g, &free_make).is_none()
            && calls_edge(&records, &g, &u_make).is_none(),
        "Self::other() must invent no edge"
    );
}

#[test]
fn trait_nested_and_self_call_edges_are_byte_stable_across_repeated_scans() {
    // Determinism guard for the block-local and in-trait `Self::` paths.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/alpha.rs",
            "pub trait T {\n    fn make() -> u32;\n    fn f(&self) -> u32 {\n        fn helper() -> u32 {\n            3\n        }\n        helper() + Self::make()\n    }\n}\n",
        )],
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
    assert!(
        first.contains(r#""resolution":"resolved""#),
        "stability check must cover the resolved nested + Self:: trait paths: {first}"
    );
}

// --- In-trait `self.` method calls carry the trait owner (issue #390) --------

#[test]
fn in_trait_self_method_call_binds_only_the_declaring_trait() {
    // REGRESSION (issue #390): a `self.read()` call inside a default trait
    // method names the trait's OWN method `read`. In a trait body there is no
    // `impl_context`, so before the fix `receiver_owner` was None and the call
    // was a plain `Method` — which the PR's widened Method pool then fanned out
    // to EVERY same-named trait method, minting an ambiguous edge to an
    // unrelated `trait U { fn read }`. Carrying the enclosing trait as
    // `receiver_owner` makes it a `SelfMethod` narrowed to `T::read` ONLY.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32;\n    fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait U {\n    fn read(&self) -> u32;\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let u_read = symbol_id(&records, "function", "gamma::read", "src/gamma.rs");
    let f = symbol_id(&records, "function", "alpha::f", "src/alpha.rs");

    // Recall: self.read() resolves to the declaring trait's own method.
    assert_calls_edge_with_resolution(&records, &f, &t_read, "resolved");
    // NO-WRONG-EDGE: the unrelated trait U::read is never bound.
    assert!(
        calls_edge(&records, &f, &u_read).is_none(),
        "self.read() in trait T must not fan out to the unrelated trait U::read"
    );
}

#[test]
fn in_trait_self_method_call_with_no_matching_method_invents_no_edge() {
    // NO-WRONG-EDGE (issue #390): `self.other()` inside trait T, where T
    // declares no `other`, must stay unresolved — the narrowed candidate set is
    // empty and the branch must NOT fall back to the broad method pool. An
    // unrelated `trait U { fn other }` must never be bound (prefer a MISSING
    // edge over a WRONG one).
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32;\n    fn g(&self) -> u32 {\n        self.other()\n    }\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait U {\n    fn other(&self) -> u32;\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let u_other = symbol_id(&records, "function", "gamma::other", "src/gamma.rs");
    let g = symbol_id(&records, "function", "alpha::g", "src/alpha.rs");

    assert!(
        calls_edge(&records, &g, &u_other).is_none(),
        "self.other() naming nothing on trait T must not fan out to U::other"
    );
    // A receiver-typed self call with no candidate is external — no Diagnostic.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "other"
        }),
        "an unresolved self-method call must not emit a Diagnostic node"
    );
}

#[test]
fn impl_self_method_call_unaffected_by_trait_widening() {
    // SYMMETRY GUARD (issue #390): an impl `self.g()` (owner `S`) still narrows
    // to its inherent `S::g`. The SelfMethod trait-method widening must not leak
    // a same-named trait method `T::g` in another file into the impl self-call.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub struct S;\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.g()\n    }\n    pub fn g(&self) -> u32 {\n        3\n    }\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait T {\n    fn g(&self) -> u32;\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let s_g = symbol_id(&records, "method", "alpha::S::g", "src/alpha.rs");
    let t_g = symbol_id(&records, "function", "gamma::g", "src/gamma.rs");
    let f = symbol_id(&records, "method", "alpha::S::f", "src/alpha.rs");

    // The impl self-call binds its own inherent method.
    assert_calls_edge_with_resolution(&records, &f, &s_g, "resolved");
    // The same-named trait method is NOT admitted into the impl self-call.
    assert!(
        calls_edge(&records, &f, &t_g).is_none(),
        "impl self.g() must not admit the same-named trait method T::g"
    );
}

// --- IMPLEMENTS-gated self-dispatch to trait defaults (issue #414) ----------

#[test]
fn impl_self_call_binds_an_implemented_trait_default() {
    // RECALL (issue #414): `impl T for S {}` adopts the trait default `T::read`;
    // an inherent `impl S { fn f(&self){ self.read(); } }` calls it via `self`.
    // The call carries owner `S`, but `T::read`'s segments end `[T, read]`, so
    // the strict `[S, read]` owner-narrowing (issue #390) excludes it and the
    // call was left unresolved. Bind it to `T::read` because the IMPLEMENTS index
    // PROVES S implements T.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl crate::alpha::T for S {}\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    assert_calls_edge_with_resolution(&records, &f, &t_read, "resolved");
}

#[test]
fn impl_self_call_ignores_an_unimplemented_trait_default() {
    // NO-WRONG-EDGE (issue #414): a second trait `U` also declares a `read`
    // default but S does NOT implement it. `self.read()` in `impl S` must bind
    // ONLY the implemented `T::read`, never the unrelated `U::read`.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/kappa.rs",
                "pub trait U {\n    fn read(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl crate::alpha::T for S {}\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let u_read = symbol_id(&records, "function", "kappa::read", "src/kappa.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    assert_calls_edge_with_resolution(&records, &f, &t_read, "resolved");
    assert!(
        calls_edge(&records, &f, &u_read).is_none(),
        "self.read() must not bind U::read, a trait S does not implement"
    );
}

#[test]
fn impl_self_call_to_a_method_from_two_implemented_traits_stays_unresolved() {
    // CONSERVATIVE (issue #414, Codex P1 on #420): S implements BOTH T and V,
    // each declaring a `read` default. Rust dispatch for `self.read()` depends on
    // which trait is in lexical scope at the call site (its `use` imports), which
    // this cross-file pass does not resolve. Emitting candidates for both would
    // mint a false CALLS edge to whichever trait is NOT in scope. Multiple
    // candidate impls therefore stay UNRESOLVED — no edge to either method.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/kappa.rs",
                "pub trait V {\n    fn read(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl crate::alpha::T for S {}\nimpl crate::kappa::V for S {}\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let v_read = symbol_id(&records, "function", "kappa::read", "src/kappa.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    assert!(
        calls_edge(&records, &f, &t_read).is_none(),
        "a 2-implemented-trait self-dispatch must not bind T::read (call-site trait scope unresolved)"
    );
    assert!(
        calls_edge(&records, &f, &v_read).is_none(),
        "a 2-implemented-trait self-dispatch must not bind V::read (call-site trait scope unresolved)"
    );
}

#[test]
fn impl_self_call_respects_call_site_trait_scope() {
    // NO-WRONG-EDGE (issue #414, Codex P1 on #420): S implements T and U, both
    // declaring `read`, but the calling module only `use`s U. Rust dispatches
    // `self.read()` to `U::read` ONLY — `T::read` is not method-call-visible
    // (out of lexical scope). This cross-file pass does not thread the call
    // site's `use` imports, so it cannot tell which trait is in scope; emitting
    // an edge to `T::read` would be a WRONG edge for valid Rust. The critical
    // assertion is that NO edge to the out-of-scope `T::read` is minted; per the
    // conservative-narrowing charge the in-scope `U::read` is left unresolved too
    // (prefer a MISSING edge over a WRONG one).
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait U {\n    fn read(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "use crate::gamma::U;\npub struct S;\nimpl crate::alpha::T for S {}\nimpl crate::gamma::U for S {}\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let u_read = symbol_id(&records, "function", "gamma::read", "src/gamma.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    // CRITICAL: never bind the out-of-scope trait's method.
    assert!(
        calls_edge(&records, &f, &t_read).is_none(),
        "self.read() must not bind T::read: only `use crate::gamma::U` is in scope at the call site"
    );
    // Conservative narrowing: the in-scope U::read is also left unresolved (the
    // pass cannot prove call-site trait scope).
    assert!(
        calls_edge(&records, &f, &u_read).is_none(),
        "a 2-implemented-trait self-dispatch stays unresolved, never a wrong or guessed edge"
    );
}

#[test]
fn impl_self_call_prefers_an_inherent_method_over_a_trait_default() {
    // INHERENT PRECEDENCE (issue #414): S has an inherent `read` AND implements
    // `T` (whose default also declares `read`). Rust dispatch binds the inherent
    // method, so `self.read()` must bind `S::read`, NOT `T::read`.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl crate::alpha::T for S {}\nimpl S {\n    pub fn read(&self) -> u32 {\n        9\n    }\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let s_read = symbol_id(&records, "method", "beta::S::read", "src/beta.rs");
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    assert_calls_edge_with_resolution(&records, &f, &s_read, "resolved");
    assert!(
        calls_edge(&records, &f, &t_read).is_none(),
        "the inherent S::read must win; T::read must never be bound"
    );
}

#[test]
fn impl_self_call_without_an_implements_proof_stays_unresolved() {
    // NO PROOF / pre-#414 guard preserved: trait `T` with a `read` default
    // EXISTS, but there is NO `impl T for S`. `self.read()` in `impl S` must stay
    // UNRESOLVED — prefer a MISSING edge over a WRONG one.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    assert!(
        calls_edge(&records, &f, &t_read).is_none(),
        "self.read() with no `impl T for S` proof must not bind T::read"
    );
}

#[test]
fn impl_self_call_does_not_bind_across_same_simple_name_types() {
    // NO-WRONG-EDGE (issue #414, same-simple-name-type collision): two DISTINCT
    // types both named `S` live in different modules of ONE crate root. `b::S`
    // implements `x::U` (adopting its `read` default); `a::S` implements
    // nothing. Because the implemented-traits map is keyed on the BARE type name
    // `S`, an unguarded join would let `self.read()` in `impl a::S` bind
    // `x::U::read` — a former guaranteed MISS turned into a confident WRONG edge.
    // The bare `impl_type` is ambiguous among the crate root's type definitions,
    // so `a::S`'s self-dispatch must stay UNRESOLVED.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/x.rs",
                "pub trait U {\n    fn read(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            ("src/b.rs", "pub struct S;\nimpl crate::x::U for S {}\n"),
            (
                "src/a.rs",
                "pub struct S;\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let u_read = symbol_id(&records, "function", "x::read", "src/x.rs");
    let f = symbol_id(&records, "method", "a::S::f", "src/a.rs");

    assert!(
        calls_edge(&records, &f, &u_read).is_none(),
        "self.read() in impl a::S must not bind x::U::read via a same-simple-name-type collision"
    );
}

#[test]
fn impl_self_call_does_not_bind_a_trait_impld_for_an_external_leaf_collision() {
    // NO-WRONG-EDGE (issue #414, external-leaf collision, Codex P2 on #420): a
    // trait `T` is implemented for the EXTERNAL type `std::string::String`, whose
    // leaf name `String` collides with a LOCAL `struct String` that implements a
    // DIFFERENT trait `U`. Under the bare-leaf map key `String`, the external
    // contributor would pollute the local type's implemented-trait set to
    // {T, U}, so `self.read()` in `impl String` would fan out an ambiguous edge
    // to `T::read` even though the local `String` only implements `U`. The
    // implementing type must resolve to a UNIQUE LOCAL type def, so the external
    // `std::string::String` relation is dropped: `self.read()` binds ONLY the
    // local `U::read`.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[(
            "src/lib.rs",
            "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\npub trait U {\n    fn read(&self) -> u32 {\n        2\n    }\n}\npub struct String;\nimpl T for std::string::String {}\nimpl U for String {}\nimpl String {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
        )],
    );

    let records = scan_fixture(repo);
    let f = symbol_id(&records, "method", "String::f", "src/lib.rs");
    // Both trait methods qualify to the Symbol name `read` in one crate root, so
    // they cannot be told apart by node name. Distinguish the bug from the fix by
    // the shape of the CALLS edges out of `f`: the bug pollutes the local
    // `String`'s implemented set to {T, U}, minting TWO ambiguous edges (one to
    // each `read`); the fix drops the external `std::string::String` relation,
    // leaving {U} — exactly ONE resolved edge to the local `U::read`.
    let read_ids: std::collections::BTreeSet<&str> = records
        .iter()
        .filter(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == "function"
                && record["name"] == "read"
                && record["repo_relative_path"] == "src/lib.rs"
        })
        .filter_map(|record| record["id"].as_str())
        .collect();
    assert_eq!(
        read_ids.len(),
        2,
        "T::read and U::read must both be extracted"
    );

    let calls_from_f: Vec<(&str, &str)> = records
        .iter()
        .filter(|record| {
            record["record_type"] == "edge" && record["label"] == "CALLS" && record["source"] == f
        })
        .filter_map(|record| {
            Some((
                record["target"].as_str()?,
                record["resolution"].as_str().unwrap_or(""),
            ))
        })
        .filter(|(target, _)| read_ids.contains(target))
        .collect();

    // Exactly one resolved edge to a local `read` (the implemented `U::read`),
    // and no ambiguous fan-out — the external `impl T for std::string::String`
    // never contributes.
    assert_eq!(
        calls_from_f.len(),
        1,
        "self.read() must bind exactly one local read method, got: {calls_from_f:?}"
    );
    assert_eq!(
        calls_from_f[0].1, "resolved",
        "the single self.read() edge must be resolved, not an ambiguous external-leaf fan-out"
    );
}

#[test]
fn impl_self_call_binds_an_implemented_trait_default_across_three_files() {
    // CROSS-FILE (issue #414): the trait (alpha), the calling `impl S` + struct
    // (beta), and the `impl T for S` relation (gamma) live in three DIFFERENT
    // files, exercising the repo-wide ImplTargetIndex join and the global
    // implemented-traits map.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
            (
                "src/gamma.rs",
                "impl crate::alpha::T for crate::beta::S {}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let t_read = symbol_id(&records, "function", "alpha::read", "src/alpha.rs");
    let f = symbol_id(&records, "method", "beta::S::f", "src/beta.rs");

    assert_calls_edge_with_resolution(&records, &f, &t_read, "resolved");
}

#[test]
fn implements_gated_self_dispatch_edges_are_byte_stable_across_repeated_scans() {
    // Determinism guard for the #414 single-implemented-trait recall path (the
    // resolved case — multiple implemented traits stay unresolved and mint no
    // edge, so a single trait exercises a real deterministic resolved edge).
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/beta.rs",
                "pub struct S;\nimpl crate::alpha::T for S {}\nimpl S {\n    pub fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
        ],
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
    assert!(
        first.contains(r#""resolution":"resolved""#),
        "stability check must cover the resolved implements-gated self-dispatch path: {first}"
    );
}

#[test]
fn in_trait_self_method_call_edges_are_byte_stable_across_repeated_scans() {
    // Determinism guard for the in-trait `self.` method-call resolution path.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/alpha.rs",
                "pub trait T {\n    fn read(&self) -> u32;\n    fn f(&self) -> u32 {\n        self.read()\n    }\n}\n",
            ),
            (
                "src/gamma.rs",
                "pub trait U {\n    fn read(&self) -> u32;\n}\n",
            ),
        ],
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
    assert!(
        first.contains(r#""resolution":"resolved""#),
        "stability check must cover the resolved in-trait self-method path: {first}"
    );
}

fn implements_edge<'a>(records: &'a [Value], source: &str, target: &str) -> Option<&'a Value> {
    records.iter().find(|record| {
        record["record_type"] == "edge"
            && record["label"] == "IMPLEMENTS"
            && record["source"] == source
            && record["target"] == target
    })
}

// Cross-file out-of-line trait impls (issue #344): an `impl crate::T for Foo`
// in a separate `mod m;` file must edge-back to the crate-root trait, and the
// incremental cache must recompute (and retire) the edge from `FileFacts` on a
// re-scan exactly like cross-file CALLS.
#[test]
fn incremental_scan_emits_and_retires_cross_file_implements_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    write_fixture(
        &repo,
        &[
            (
                "src/lib.rs",
                "pub trait T {\n    fn go(&self);\n}\n\npub mod m;\n",
            ),
            (
                "src/m.rs",
                "pub struct Foo;\n\nimpl crate::T for Foo {\n    fn go(&self) {}\n}\n",
            ),
        ],
    );
    let cache_path = temp.path().join("codegraph-cache.json");

    let first = aletheia_egregore::incremental::scan_repository_incremental_at(
        &repo,
        &cache_path,
        FIXED_TIME,
    )
    .expect("first incremental scan should work");
    let first_records = parse_jsonl(
        &first
            .graph
            .to_jsonl()
            .expect("first incremental graph should serialize"),
    );
    let trait_t = symbol_id(&first_records, "trait", "T", "src/lib.rs");
    let impl_foo = symbol_id(
        &first_records,
        "impl",
        "m::impl crate::T for Foo",
        "src/m.rs",
    );
    let edge = implements_edge(&first_records, &impl_foo, &trait_t)
        .expect("cross-file IMPLEMENTS edge should exist");
    let edge_id = edge["id"].as_str().expect("edge should have ID").to_owned();

    // Removing the impl retires the edge with a tombstone on the next scan.
    fs::write(repo.join("src/m.rs"), "pub struct Foo;\n").expect("fixture should update");
    let second = aletheia_egregore::incremental::scan_repository_incremental_at(
        &repo,
        &cache_path,
        "2026-06-08T00:00:00Z",
    )
    .expect("second incremental scan should work");
    let second_records = parse_jsonl(
        &second
            .graph
            .to_jsonl()
            .expect("second incremental graph should serialize"),
    );
    assert!(
        implements_edge(&second_records, &impl_foo, &trait_t).is_none(),
        "removed impl must not re-emit the cross-file IMPLEMENTS edge"
    );
    assert!(
        second_records.iter().any(|record| {
            record["record_type"] == "tombstone" && record["deleted_id"] == edge_id.as_str()
        }),
        "stale cross-file IMPLEMENTS edge must be tombstoned so persisted stores can retire it"
    );
}

#[test]
fn history_replay_emits_cross_file_call_edges_per_commit() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);
    write_fixture(
        &repo,
        &[
            (
                "src/alpha.rs",
                "pub fn shared_helper() -> usize {\n    7\n}\n",
            ),
            (
                "src/beta.rs",
                "pub fn beta_caller() -> usize {\n    shared_helper()\n}\n",
            ),
        ],
    );
    commit_with_date(&repo, "seed", "2026-01-01T00:00:00Z");

    let records = parse_jsonl(
        &scan_repository_history_with_override(&repo, Some("cross-file-history-fixture"))
            .expect("history fixture should scan")
            .to_jsonl()
            .expect("history graph should serialize"),
    );
    let helper = symbol_id(&records, "function", "alpha::shared_helper", "src/alpha.rs");
    let beta_caller = symbol_id(&records, "function", "beta::beta_caller", "src/beta.rs");
    let edge = calls_edge(&records, &beta_caller, &helper)
        .expect("history replay should emit the cross-file CALLS edge");
    assert_eq!(edge["resolution"], "resolved");
    assert!(
        edge["temporal"]["git_commit"].is_string(),
        "history cross-file edge should carry commit provenance: {edge}"
    );
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        out.status.success(),
        "git command {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn commit_with_date(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        out.status.success(),
        "git commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[allow(dead_code)]
fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// ---------------------------------------------------------------------------
// Qualified-path and cross-crate call resolution over a workspace (issue #440).
//
// End-to-end proof that `eg scan` binds a `crate::mod::fn()` and a cross-crate
// `dep_crate::mod::fn()` call to the definition Symbol (a `resolved` CALLS
// edge, visible to `transitive-callers`) instead of leaving disconnected
// per-file `Diagnostic` stubs, while an external-crate qualified call stays
// honestly unresolved. Crate members live in their own directories, so the
// repo-relative paths never begin at `src/` — the exact layout that regressed.
// ---------------------------------------------------------------------------

/// A `Diagnostic` node whose display name (the callee path) contains `needle`.
fn unresolved_stub_present(records: &[Value], needle: &str) -> bool {
    records.iter().any(|record| {
        record["record_type"] == "node"
            && record["kind"] == "Diagnostic"
            && record["name"].as_str().is_some_and(|n| n.contains(needle))
    })
}

#[test]
fn workspace_intra_crate_qualified_call_resolves_to_definition() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("crate_a/src/lib.rs", "pub mod mod_a;\npub mod mod_b;\n"),
            ("crate_a/src/mod_b.rs", "pub fn target() {}\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() {\n    crate::mod_b::target();\n}\n",
            ),
        ],
    );
    let records = scan_fixture(repo);

    let caller = symbol_id(
        &records,
        "function",
        "mod_a::caller",
        "crate_a/src/mod_a.rs",
    );
    let target = symbol_id(
        &records,
        "function",
        "mod_b::target",
        "crate_a/src/mod_b.rs",
    );
    // The call binds the real definition Symbol with a `resolved` CALLS edge.
    assert_calls_edge_with_resolution(&records, &caller, &target, "resolved");
    assert!(
        !unresolved_stub_present(&records, "mod_b::target"),
        "the qualified call must not remain a Diagnostic stub"
    );
}

#[test]
fn workspace_cross_crate_qualified_call_resolves_to_definition() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            ("crate_b/src/lib.rs", "pub mod mod_c;\n"),
            ("crate_b/src/mod_c.rs", "pub fn target() {}\n"),
            ("crate_a/src/lib.rs", "pub mod mod_a;\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() {\n    crate_b::mod_c::target();\n}\n",
            ),
        ],
    );
    let records = scan_fixture(repo);

    let caller = symbol_id(
        &records,
        "function",
        "mod_a::caller",
        "crate_a/src/mod_a.rs",
    );
    let target = symbol_id(
        &records,
        "function",
        "mod_c::target",
        "crate_b/src/mod_c.rs",
    );
    assert_calls_edge_with_resolution(&records, &caller, &target, "resolved");
}

#[test]
fn workspace_external_crate_qualified_call_stays_unresolved() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            // A same-named local `target` exists, but the call names `ext_dep`,
            // which is NOT a workspace crate, so it must not bind here.
            ("crate_a/src/lib.rs", "pub mod mod_a;\npub mod mod_c;\n"),
            ("crate_a/src/mod_c.rs", "pub fn target() {}\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() {\n    ext_dep::mod_c::target();\n}\n",
            ),
        ],
    );
    let records = scan_fixture(repo);

    let caller = symbol_id(
        &records,
        "function",
        "mod_a::caller",
        "crate_a/src/mod_a.rs",
    );
    let local = symbol_id(
        &records,
        "function",
        "mod_c::target",
        "crate_a/src/mod_c.rs",
    );
    assert!(
        calls_edge(&records, &caller, &local).is_none(),
        "an external-crate qualified call must not bind a same-named local definition"
    );
    assert!(
        unresolved_stub_present(&records, "ext_dep"),
        "the external qualified call must stay an honest Diagnostic stub"
    );
}

#[test]
fn workspace_qualified_call_disambiguates_same_name_across_modules() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "crate_a/src/lib.rs",
                "pub mod mod_a;\npub mod mod_b;\npub mod mod_d;\n",
            ),
            ("crate_a/src/mod_b.rs", "pub fn target() {}\n"),
            ("crate_a/src/mod_d.rs", "pub fn target() {}\n"),
            (
                "crate_a/src/mod_a.rs",
                "pub fn caller() {\n    crate::mod_b::target();\n}\n",
            ),
        ],
    );
    let records = scan_fixture(repo);

    let caller = symbol_id(
        &records,
        "function",
        "mod_a::caller",
        "crate_a/src/mod_a.rs",
    );
    let target_b = symbol_id(
        &records,
        "function",
        "mod_b::target",
        "crate_a/src/mod_b.rs",
    );
    let target_d = symbol_id(
        &records,
        "function",
        "mod_d::target",
        "crate_a/src/mod_d.rs",
    );
    assert_calls_edge_with_resolution(&records, &caller, &target_b, "resolved");
    assert!(
        calls_edge(&records, &caller, &target_d).is_none(),
        "crate::mod_b::target() must not bind mod_d's same-named target"
    );
}

// ---------------------------------------------------------------------------
// Provable receiver-type method-call narrowing (issue #441).
//
// Today `x.method()` fans out one `ambiguous` CALLS edge per same-named method
// in the workspace (see `receiver_call_to_two_trait_methods_is_ambiguous_to_both`).
// Issue #441 narrows the receiver call to a SINGLE `resolved` edge when the
// receiver's type is PROVABLE (syntax-level, no type inference) AND resolves to
// a UNIQUE LOCAL NON-TRAIT type (via the existing ImplTargetIndex); otherwise
// behavior stays byte-identical to today (ambiguous fan-out). A provable
// receiver type whose type has NO matching method emits NOTHING NEW — never a
// guess.
//
// The four provable receiver patterns (this suite covers 1 and 4 as the new
// behavior; 2 UFCS is a #440 regression-guard; 3 self.method() is #420):
//   1. `let x: T = …; x.method()`  (explicit ascription, unshadowed)
//   4. `fn f(x: T) { x.method() }` (typed param, simple identifier pattern)
//
// Two shared two-type workspaces: `Foo::send`/`Bar::send` (and `Foo::run`/
// `Bar::run`). The narrowing must pick the provable receiver's method ONLY.
// ---------------------------------------------------------------------------

/// A two-type workspace where both `Foo` and `Bar` declare an inherent
/// method `send`, plus a caller file to be provided per test.
fn two_type_send_workspace(caller_src: &str) -> Vec<(&'static str, String)> {
    vec![
        (
            "src/foo.rs",
            "pub struct Foo;\nimpl Foo {\n    pub fn send(&self) -> u32 {\n        1\n    }\n}\n"
                .to_owned(),
        ),
        (
            "src/bar.rs",
            "pub struct Bar;\nimpl Bar {\n    pub fn send(&self) -> u32 {\n        2\n    }\n}\n"
                .to_owned(),
        ),
        ("src/caller.rs", caller_src.to_owned()),
    ]
}

fn write_owned_fixture(root: &Path, files: &[(&str, String)]) {
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (*path, contents.as_str()))
        .collect();
    write_fixture(root, &borrowed);
}

#[test]
fn typed_let_receiver_narrows_to_the_provable_local_type() {
    // RED (issue #441, pattern 1): `let f: Foo = make(); f.send()` — the receiver
    // `f` has a PROVABLE type `Foo` (explicit ascription, binding unshadowed).
    // `f.send()` must bind ONLY `Foo::send` with a single `resolved` edge, never
    // fan out an ambiguous edge to `Bar::send`. Today (pre-#441) the receiver
    // type is discarded and `f.send()` fans out ambiguous to BOTH.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_owned_fixture(
        repo,
        &two_type_send_workspace(
            "pub fn caller() -> u32 {\n    let f: Foo = make();\n    f.send()\n}\n",
        ),
    );

    let records = scan_fixture(repo);
    let foo_send = symbol_id(&records, "method", "foo::Foo::send", "src/foo.rs");
    let bar_send = symbol_id(&records, "method", "bar::Bar::send", "src/bar.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    assert_calls_edge_with_resolution(&records, &caller, &foo_send, "resolved");
    assert!(
        calls_edge(&records, &caller, &bar_send).is_none(),
        "a provable receiver type `Foo` must narrow f.send() to Foo::send only, never fan out to Bar::send"
    );
}

#[test]
fn typed_param_receiver_narrows_to_the_provable_local_type() {
    // RED (issue #441, pattern 4): `fn caller(f: Foo) { f.send() }` — the param
    // `f` has a PROVABLE nominal type `Foo` (simple identifier pattern). The
    // receiver call must narrow to a single `resolved` edge to `Foo::send`,
    // never an ambiguous fan-out to `Bar::send`. Today the param type is
    // discarded and `f.send()` fans out ambiguous to BOTH.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_owned_fixture(
        repo,
        &two_type_send_workspace("pub fn caller(f: Foo) -> u32 {\n    f.send()\n}\n"),
    );

    let records = scan_fixture(repo);
    let foo_send = symbol_id(&records, "method", "foo::Foo::send", "src/foo.rs");
    let bar_send = symbol_id(&records, "method", "bar::Bar::send", "src/bar.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    assert_calls_edge_with_resolution(&records, &caller, &foo_send, "resolved");
    assert!(
        calls_edge(&records, &caller, &bar_send).is_none(),
        "a typed param `f: Foo` must narrow f.send() to Foo::send only, never fan out to Bar::send"
    );
}

#[test]
fn two_types_with_same_named_method_bind_only_the_provable_receiver() {
    // RED (issue #441): types `Foo` and `Bar` both declare `run`. With a provable
    // `let f: Foo = …`, `f.run()` binds a single `resolved` edge to `Foo::run`
    // and NO edge to `Bar::run`. The complementary both-sides check to the
    // typed-let case.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/foo.rs",
                "pub struct Foo;\nimpl Foo {\n    pub fn run(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/bar.rs",
                "pub struct Bar;\nimpl Bar {\n    pub fn run(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/caller.rs",
                "pub fn caller() -> u32 {\n    let f: Foo = make();\n    f.run()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let foo_run = symbol_id(&records, "method", "foo::Foo::run", "src/foo.rs");
    let bar_run = symbol_id(&records, "method", "bar::Bar::run", "src/bar.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    assert_calls_edge_with_resolution(&records, &caller, &foo_run, "resolved");
    assert!(
        calls_edge(&records, &caller, &bar_run).is_none(),
        "a provable receiver `Foo` must bind f.run() to Foo::run only, never Bar::run"
    );
}

#[test]
fn shadowed_receiver_binding_falls_back_to_ambiguous_fanout() {
    // GUARD / CONSERVATIVE FALLBACK (issue #441): a second, NON-ascribed binder
    // of the receiver name (`let f = other();`) shadows the earlier typed `let f:
    // Foo`, so `f`'s type is no longer provable. The narrowing must NOT fire —
    // the call falls back to today's ambiguous fan-out to ALL same-named methods.
    // This currently passes (no narrowing exists); it pins the shadow veto so the
    // GREEN implementation does not over-narrow a shadowed binding.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_owned_fixture(
        repo,
        &two_type_send_workspace(
            "pub fn caller() -> u32 {\n    let f: Foo = make();\n    let f = other();\n    f.send()\n}\n",
        ),
    );

    let records = scan_fixture(repo);
    let foo_send = symbol_id(&records, "method", "foo::Foo::send", "src/foo.rs");
    let bar_send = symbol_id(&records, "method", "bar::Bar::send", "src/bar.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    // A shadowed receiver name is non-provable: keep the ambiguous fan-out.
    assert_calls_edge_with_resolution(&records, &caller, &foo_send, "ambiguous");
    assert_calls_edge_with_resolution(&records, &caller, &bar_send, "ambiguous");
}

#[test]
// `bar_configure`/`baz_configure` are intentionally parallel (two types with the
// same method), which trips the similar-names lint without aiding clarity here.
#[allow(clippy::similar_names)]
fn provable_receiver_with_no_matching_method_invents_no_edge() {
    // RED (issue #441): a provable `let f: Foo = …` whose type `Foo` has NO
    // matching method must emit NOTHING NEW — never guess an edge to another
    // type's same-named method. `Foo` has only `send`; `Bar` and `Baz` both
    // declare `configure`; `f.configure()` must bind NEITHER. Today (pre-#441)
    // the receiver type is discarded and `f.configure()` fans out ambiguous to
    // BOTH `Bar::configure` and `Baz::configure`.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/foo.rs",
                "pub struct Foo;\nimpl Foo {\n    pub fn send(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/bar.rs",
                "pub struct Bar;\nimpl Bar {\n    pub fn configure(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/baz.rs",
                "pub struct Baz;\nimpl Baz {\n    pub fn configure(&self) -> u32 {\n        3\n    }\n}\n",
            ),
            (
                "src/caller.rs",
                "pub fn caller() -> u32 {\n    let f: Foo = make();\n    f.configure()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let bar_configure = symbol_id(&records, "method", "bar::Bar::configure", "src/bar.rs");
    let baz_configure = symbol_id(&records, "method", "baz::Baz::configure", "src/baz.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    // Foo has no `configure`: the provable receiver invents no edge to the other
    // types' same-named methods.
    assert!(
        calls_edge(&records, &caller, &bar_configure).is_none(),
        "a provable `Foo` with no `configure` must not bind Bar::configure"
    );
    assert!(
        calls_edge(&records, &caller, &baz_configure).is_none(),
        "a provable `Foo` with no `configure` must not bind Baz::configure"
    );
    // No Diagnostic invented for a provable-but-unmatched receiver method call.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "configure"
        }),
        "a provable-but-unmatched receiver method call must not emit a Diagnostic node"
    );
}

#[test]
fn external_typed_receiver_stays_ambiguous() {
    // LAW GUARD (issue #441): the receiver type must resolve to a UNIQUE LOCAL
    // NON-TRAIT type or NO narrowing happens. `let x: u32 = 0; x.read()` — `u32`
    // is external, so the ImplTargetIndex resolves nothing and the ambiguous
    // fan-out to every in-repo `read` is UNCHANGED. Currently passes; it proves
    // the GREEN implementation does not narrow on a non-local receiver type.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/foo.rs",
                "pub struct Foo;\nimpl Foo {\n    pub fn read(&self) -> u32 {\n        1\n    }\n}\n",
            ),
            (
                "src/bar.rs",
                "pub struct Bar;\nimpl Bar {\n    pub fn read(&self) -> u32 {\n        2\n    }\n}\n",
            ),
            (
                "src/caller.rs",
                "pub fn caller() -> u32 {\n    let x: u32 = 0;\n    x.read()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let foo_read = symbol_id(&records, "method", "foo::Foo::read", "src/foo.rs");
    let bar_read = symbol_id(&records, "method", "bar::Bar::read", "src/bar.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    assert_calls_edge_with_resolution(&records, &caller, &foo_read, "ambiguous");
    assert_calls_edge_with_resolution(&records, &caller, &bar_read, "ambiguous");
}

#[test]
fn trait_typed_receiver_stays_ambiguous() {
    // LAW GUARD (issue #441): a trait-typed receiver stays on its current path
    // (out of #441 scope — that is #267 dyn/generic dispatch). `d: &dyn Device`
    // resolves to a local TRAIT, not a non-trait type, so no narrowing fires and
    // `d.read()` keeps its ambiguous fan-out to both trait methods. Currently
    // passes; it proves the GREEN implementation excludes trait-typed receivers.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_fixture(
        repo,
        &[
            (
                "src/device.rs",
                "pub trait Device {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/sensor.rs",
                "pub trait Sensor {\n    fn read(&self) -> u32;\n}\n",
            ),
            (
                "src/caller.rs",
                "pub fn caller(d: &dyn Device) -> u32 {\n    d.read()\n}\n",
            ),
        ],
    );

    let records = scan_fixture(repo);
    let device_read = symbol_id(&records, "function", "device::read", "src/device.rs");
    let sensor_read = symbol_id(&records, "function", "sensor::read", "src/sensor.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    assert_calls_edge_with_resolution(&records, &caller, &device_read, "ambiguous");
    assert_calls_edge_with_resolution(&records, &caller, &sensor_read, "ambiguous");
}

#[test]
fn ufcs_receiver_call_resolves_to_the_named_type() {
    // GUARD (issue #441 pattern 2, expected already green from #440): a UFCS
    // `Foo::send(&f)` is a `Path` call that binds the exact named type's method
    // via the #440 qualified-path arm — a single `resolved` edge to `Foo::send`,
    // never `Bar::send`. `Self::send(...)` normalization is separately covered by
    // the in-trait/impl `Self::` tests in this file.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_owned_fixture(
        repo,
        &two_type_send_workspace(
            "pub fn caller() -> u32 {\n    let f = make();\n    Foo::send(&f)\n}\n",
        ),
    );

    let records = scan_fixture(repo);
    let foo_send = symbol_id(&records, "method", "foo::Foo::send", "src/foo.rs");
    let bar_send = symbol_id(&records, "method", "bar::Bar::send", "src/bar.rs");
    let caller = symbol_id(&records, "function", "caller::caller", "src/caller.rs");

    assert_calls_edge_with_resolution(&records, &caller, &foo_send, "resolved");
    assert!(
        calls_edge(&records, &caller, &bar_send).is_none(),
        "Foo::send(&f) UFCS must bind Foo::send only, never Bar::send"
    );
}

#[test]
fn narrowed_receiver_edges_are_byte_stable_across_repeated_scans() {
    // GUARD (issue #441): determinism guard for the receiver-type narrowing
    // path. The typed-let fixture scanned repeatedly must yield byte-identical
    // output. Passes today (the current ambiguous fan-out is already stable) and
    // must keep passing once GREEN narrows it.
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path();
    write_owned_fixture(
        repo,
        &two_type_send_workspace(
            "pub fn caller() -> u32 {\n    let f: Foo = make();\n    f.send()\n}\n",
        ),
    );

    let first = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
        .expect("fixture repo should scan")
        .to_jsonl()
        .expect("graph should serialize");
    for run in 2..=5 {
        let next = scan_repository_at_with_override(repo, FIXED_TIME, Some(REPO_ID))
            .expect("fixture repo should rescan")
            .to_jsonl()
            .expect("graph should reserialize");
        assert_eq!(first, next, "scan {run} must be byte-identical to scan 1");
    }
}
