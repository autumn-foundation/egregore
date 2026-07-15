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
fn block_local_fn_in_a_trait_method_body_resolves_its_bare_call() {
    // REGRESSION (issue #390): a `fn helper` defined block-local inside a
    // default trait method body is a FREE function, not a trait method. The
    // walker is still under `trait_context` while descending into the method
    // body, so gating trait-method attribution on the broad flag would mark
    // `helper` `is_trait_method` and drop its legal bare `helper()` call as
    // unresolved. Attribution must ride DIRECT structural trait membership, so
    // the bare call RESOLVES to the local function.
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
    // The block-local helper is a plain free function (module-qualified name,
    // NOT `alpha::T::helper`).
    let helper = symbol_id(&records, "function", "alpha::helper", "src/alpha.rs");
    let f = symbol_id(&records, "function", "alpha::f", "src/alpha.rs");

    // The bare `helper()` call binds the local free function — never dropped.
    assert_calls_edge_with_resolution(&records, &f, &helper, "resolved");
    // And it is NOT recorded unresolved against a Diagnostic.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Diagnostic"
                && record["name"] == "helper"
                && record["repo_relative_path"] == "src/alpha.rs"
        }),
        "the block-local helper() call must resolve, not emit an unresolved Diagnostic"
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
fn block_local_fn_in_an_impl_method_body_is_unchanged_by_the_trait_fix() {
    // SYMMETRIC-BREAKAGE GUARD: the trait-attribution change is gated on
    // `impl_context.is_none()`, so an impl method's own attribution is never
    // touched. A block-local `fn helper` inside an impl method keeps its
    // pre-existing impl-scoped attribution (`method`, owner `S`) — the trait
    // fix does not leak into the impl path and does not mark it a trait
    // method. (The impl path carries its own latent nested-item behavior,
    // out of scope for this trait regression.)
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
    // Unchanged impl-scoped attribution: helper stays a `method` under S.
    let helper = symbol_id(&records, "method", "alpha::S::helper", "src/alpha.rs");
    // It is a distinct symbol from the impl method `m`.
    let m = symbol_id(&records, "method", "alpha::S::m", "src/alpha.rs");
    assert_ne!(helper, m, "the nested helper is a distinct symbol from m");
    // The direct impl method `m` is present and correctly a method under S.
    assert!(
        !records.iter().any(|record| {
            record["record_type"] == "node"
                && record["kind"] == "Symbol"
                && record["symbol_kind"] == "function"
                && record["name"] == "alpha::S::helper"
        }),
        "the trait fix must not reclassify the impl-nested helper"
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
