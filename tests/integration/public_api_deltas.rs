//! Integration tests for `eg query public-api-deltas` (issue #157):
//! classification of changes to the externally-reachable public API surface
//! across a commit range, composed from recorded visibility/signature fields
//! (issue #124) and the range-delta mechanics (issue #118).
#![allow(missing_docs)]

use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    EdgeLabel, GraphRecord, NodeKind, SourceSpan, TemporalMetadata,
    query::{PublicApiDeltaRow, PublicApiDeltasOptions, RangeDeltasError, public_api_deltas},
    scan_repository_history, stable_id,
};
use assert_cmd::Command as CargoCommand;

// ---------------------------------------------------------------------------
// Synthetic record helpers (mirrors tests/integration/range_deltas.rs)
// ---------------------------------------------------------------------------

fn temporal(commit: &str, parents: &[&str], valid_time: &str) -> TemporalMetadata {
    TemporalMetadata {
        git_commit: commit.to_owned(),
        git_parent_commits: parents.iter().map(|s| (*s).to_owned()).collect(),
        valid_time: valid_time.to_owned(),
        author_time: Some(valid_time.to_owned()),
        observed_at: valid_time.to_owned(),
        valid_time_source: Some("git_commit_committer_date".to_owned()),
    }
}

fn commit(sha: &str, parents: &[&str], valid_time: &str) -> GraphRecord {
    let id = stable_id(&["node", "commit", "repo_test", sha]);
    GraphRecord::node(
        id,
        NodeKind::Commit,
        None,
        None,
        Some(sha.to_owned()),
        format!("Commit {sha}"),
    )
    .with_temporal(temporal(sha, parents, valid_time))
}

const fn span() -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 10,
        start_line: 1,
        end_line: 1,
    }
}

/// A Rust `Symbol` snapshot carrying the issue #124 declaration surface.
#[allow(clippy::too_many_arguments)]
fn rust_symbol(
    name: &str,
    kind: &str,
    path: &str,
    visibility: &str,
    signature: &str,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    let id = stable_id(&["node", "symbol", kind, "repo_test", path, name, "0"]);
    GraphRecord::syntax_symbol(
        id,
        kind,
        path.to_owned(),
        span(),
        name.to_owned(),
        "rust",
        0,
        format!("Rust {kind} {name}\nSource:\n{body}"),
    )
    .with_declaration_surface(
        Some(visibility.to_owned()),
        Some(signature.to_owned()),
        None,
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

/// A Rust `Symbol` snapshot with **no** recorded visibility (pre-#124 scan).
fn rust_symbol_no_visibility(
    name: &str,
    path: &str,
    body: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    let id = stable_id(&["node", "symbol", "function", "repo_test", path, name, "0"]);
    GraphRecord::syntax_symbol(
        id,
        "function",
        path.to_owned(),
        span(),
        name.to_owned(),
        "rust",
        0,
        format!("Rust function {name}\nSource:\n{body}"),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

/// A Rust `Module` snapshot carrying its recorded visibility (issue #213).
fn rust_module(
    name: &str,
    path: &str,
    visibility: &str,
    commit: &str,
    valid_time: &str,
) -> GraphRecord {
    let id = stable_id(&["node", "module", "repo_test", path, name]);
    GraphRecord::syntax_node(
        id,
        NodeKind::Module,
        path.to_owned(),
        span(),
        name.to_owned(),
        "rust",
        format!("Rust module {name}"),
    )
    .with_declaration_surface(Some(visibility.to_owned()), None, None)
    .with_temporal(temporal(commit, &[], valid_time))
}

fn calls_edge(source_id: &str, target_id: &str, commit: &str, valid_time: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Calls,
        source_id.to_owned(),
        target_id.to_owned(),
        Some("1.0".to_owned()),
        "caller calls callee".to_owned(),
    )
    .with_temporal(temporal(commit, &[], valid_time))
}

const T1: &str = "2026-01-01T00:00:00Z";
const T2: &str = "2026-01-02T00:00:00Z";

/// Two linear commits exercising every public-surface change class between
/// `c1` and `c2`:
///
/// - `keep`: `pub` at both endpoints, unchanged — never reported.
/// - `doomed`: `pub` at base, absent at head — `removed`.
/// - `narrowed_fn`: `pub` at base, `pub(crate)` at head — `visibility_narrowed`.
/// - `widened_fn`: `pub(crate)` at base, `pub` at head — `visibility_widened`.
/// - `sig_fn`: `pub` at both, signature changed — `signature_changed`.
/// - `brand_new`: absent at base, `pub` at head — `added`.
/// - `private_fn`: private at both, body modified — internal only, never a
///   public-surface row.
/// - `body_only`: `pub` at both, body changed, signature unchanged — tallied,
///   never a row.
#[allow(clippy::too_many_lines)]
fn synthetic_surface_records() -> Vec<GraphRecord> {
    vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        // c1 snapshots
        rust_symbol(
            "keep",
            "function",
            "src/lib.rs",
            "public",
            "fn keep() -> u32",
            "A",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "doomed",
            "function",
            "src/lib.rs",
            "public",
            "fn doomed() -> u32",
            "B",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "narrowed_fn",
            "function",
            "src/lib.rs",
            "public",
            "fn narrowed_fn() -> u32",
            "C",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "widened_fn",
            "function",
            "src/lib.rs",
            "crate",
            "fn widened_fn() -> u32",
            "D",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "sig_fn",
            "function",
            "src/lib.rs",
            "public",
            "fn sig_fn() -> u32",
            "E1",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "private_fn",
            "function",
            "src/lib.rs",
            "private",
            "fn private_fn() -> u32",
            "F1",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "body_only",
            "function",
            "src/lib.rs",
            "public",
            "fn body_only() -> u32",
            "G1",
            "c1sha0000",
            T1,
        ),
        // c2 snapshots
        rust_symbol(
            "keep",
            "function",
            "src/lib.rs",
            "public",
            "fn keep() -> u32",
            "A",
            "c2sha0000",
            T2,
        ),
        rust_symbol(
            "narrowed_fn",
            "function",
            "src/lib.rs",
            "crate",
            "fn narrowed_fn() -> u32",
            "C",
            "c2sha0000",
            T2,
        ),
        rust_symbol(
            "widened_fn",
            "function",
            "src/lib.rs",
            "public",
            "fn widened_fn() -> u32",
            "D",
            "c2sha0000",
            T2,
        ),
        rust_symbol(
            "sig_fn",
            "function",
            "src/lib.rs",
            "public",
            "fn sig_fn(x: u8) -> u64",
            "E2",
            "c2sha0000",
            T2,
        ),
        rust_symbol(
            "brand_new",
            "function",
            "src/lib.rs",
            "public",
            "fn brand_new() -> u32",
            "H",
            "c2sha0000",
            T2,
        ),
        rust_symbol(
            "private_fn",
            "function",
            "src/lib.rs",
            "private",
            "fn private_fn() -> u32",
            "F2",
            "c2sha0000",
            T2,
        ),
        rust_symbol(
            "body_only",
            "function",
            "src/lib.rs",
            "public",
            "fn body_only() -> u32",
            "G2",
            "c2sha0000",
            T2,
        ),
    ]
}

fn names<'a>(rows: &'a [PublicApiDeltaRow<'a>]) -> Vec<&'a str> {
    rows.iter().map(|row| row.name).collect()
}

// ---------------------------------------------------------------------------
// Closed change-class taxonomy over synthetic history
// ---------------------------------------------------------------------------

#[test]
fn public_api_deltas_classifies_every_change_class() {
    let records = synthetic_surface_records();
    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions::default(),
    )
    .expect("range should resolve");

    assert_eq!(report.base, "c1sha0000");
    assert_eq!(report.head, "c2sha0000");
    assert_eq!(report.range_commit_count, 1);

    assert_eq!(names(&report.added), vec!["brand_new"]);
    assert_eq!(names(&report.removed), vec!["doomed"]);
    assert_eq!(names(&report.signature_changed), vec!["sig_fn"]);
    assert_eq!(names(&report.visibility_narrowed), vec!["narrowed_fn"]);
    assert_eq!(names(&report.visibility_widened), vec!["widened_fn"]);

    // Potentially-breaking flags: removal, narrowing, and signature change
    // are flagged; addition and widening are not.
    for row in report.removed.iter().chain(&report.signature_changed) {
        assert!(row.potentially_breaking, "{} must be flagged", row.name);
    }
    for row in &report.visibility_narrowed {
        assert!(row.potentially_breaking, "{} must be flagged", row.name);
    }
    for row in report.added.iter().chain(&report.visibility_widened) {
        assert!(
            !row.potentially_breaking,
            "{} must not be flagged",
            row.name
        );
    }

    // Before/after surface text per class.
    let removed = &report.removed[0];
    assert_eq!(removed.before_visibility, Some("public"));
    assert_eq!(removed.before_signature, Some("fn doomed() -> u32"));
    assert_eq!(removed.after_visibility, None);
    assert_eq!(removed.after_signature, None);
    assert_eq!(removed.handle_side, "base_tombstone");

    let added = &report.added[0];
    assert_eq!(added.before_visibility, None);
    assert_eq!(added.after_visibility, Some("public"));
    assert_eq!(added.after_signature, Some("fn brand_new() -> u32"));
    assert_eq!(added.handle_side, "head");

    let sig = &report.signature_changed[0];
    assert_eq!(sig.before_signature, Some("fn sig_fn() -> u32"));
    assert_eq!(sig.after_signature, Some("fn sig_fn(x: u8) -> u64"));

    let narrowed = &report.visibility_narrowed[0];
    assert_eq!(narrowed.before_visibility, Some("public"));
    assert_eq!(narrowed.after_visibility, Some("crate"));
    assert!(!narrowed.via_module_chain);

    let widened = &report.visibility_widened[0];
    assert_eq!(widened.before_visibility, Some("crate"));
    assert_eq!(widened.after_visibility, Some("public"));

    // Every row carries citable identity fields and the introducing commit.
    for row in report
        .added
        .iter()
        .chain(&report.removed)
        .chain(&report.signature_changed)
        .chain(&report.visibility_narrowed)
        .chain(&report.visibility_widened)
    {
        assert!(!row.record_id.is_empty());
        assert!(row.schema_version >= 1);
        assert!(!row.repo_relative_path.is_empty());
        assert!(row.span.is_some() || row.absent_span_reason.is_some());
        assert_eq!(row.commit, "c2sha0000");
        assert_eq!(row.valid_time, Some(T2));
    }

    // Zero private-symbol leakage into the public-surface groups, and the
    // unchanged / body-only symbols never produce a row.
    for rows in [
        &report.added,
        &report.removed,
        &report.signature_changed,
        &report.visibility_narrowed,
        &report.visibility_widened,
    ] {
        for row in rows {
            assert_ne!(row.name, "private_fn", "private symbol leaked");
            assert_ne!(row.name, "keep");
            assert_ne!(row.name, "body_only");
        }
    }
    assert_eq!(report.counts.exported_body_only_modified, 1);
    assert_eq!(report.counts.internal_changes, 1);

    // Honest labeling: observed surface change, never a semver/breakage claim.
    assert!(report.disclaimer.contains("not proof"));
}

#[test]
fn public_api_deltas_internal_group_is_opt_in_and_labeled() {
    let records = synthetic_surface_records();

    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions::default(),
    )
    .expect("range should resolve");
    assert!(
        report.internal.is_none(),
        "internal group must be absent unless requested"
    );

    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions {
            include_internal: true,
            ..PublicApiDeltasOptions::default()
        },
    )
    .expect("range should resolve");
    let internal = report.internal.expect("internal group must be present");
    assert_eq!(internal.label, "internal_not_public_surface");
    assert_eq!(names(&internal.rows), vec!["private_fn"]);
    assert_eq!(internal.rows[0].change_class, "internal_modified");
    assert!(!internal.rows[0].potentially_breaking);
}

#[test]
fn public_api_deltas_narrows_via_module_chain() {
    // `inner::trapped` stays `pub`, but its containing module goes
    // `pub` -> private between the endpoints: the symbol leaves the surface.
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        rust_module("inner", "src/lib.rs", "public", "c1sha0000", T1),
        rust_module("inner", "src/lib.rs", "private", "c2sha0000", T2),
        rust_symbol(
            "inner::trapped",
            "function",
            "src/lib.rs",
            "public",
            "fn trapped() -> u32",
            "A",
            "c1sha0000",
            T1,
        ),
        rust_symbol(
            "inner::trapped",
            "function",
            "src/lib.rs",
            "public",
            "fn trapped() -> u32",
            "A",
            "c2sha0000",
            T2,
        ),
    ];
    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions::default(),
    )
    .expect("range should resolve");
    assert_eq!(names(&report.visibility_narrowed), vec!["inner::trapped"]);
    let row = &report.visibility_narrowed[0];
    assert!(row.potentially_breaking);
    assert!(row.via_module_chain);
    assert_eq!(row.before_visibility, Some("public"));
    assert_eq!(row.after_visibility, Some("public"));
}

#[test]
fn public_api_deltas_missing_visibility_yields_diagnostic_not_row() {
    let records = vec![
        commit("c1sha0000", &[], T1),
        commit("c2sha0000", &["c1sha0000"], T2),
        rust_symbol_no_visibility("mystery", "src/lib.rs", "A", "c1sha0000", T1),
    ];
    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions {
            include_internal: true,
            ..PublicApiDeltasOptions::default()
        },
    )
    .expect("range should resolve");
    for rows in [
        &report.added,
        &report.removed,
        &report.signature_changed,
        &report.visibility_narrowed,
        &report.visibility_widened,
    ] {
        assert!(rows.is_empty());
    }
    assert!(
        report
            .internal
            .as_ref()
            .is_some_and(|section| section.rows.is_empty()),
        "unclassifiable symbols must not be guessed into the internal group"
    );
    assert!(
        report
            .diagnostics
            .iter()
            .any(|d| d.code == "symbol_visibility_missing"),
        "missing visibility must surface as a stable diagnostic"
    );
}

#[test]
fn public_api_deltas_reuses_range_error_taxonomy() {
    let records = synthetic_surface_records();
    let err = public_api_deltas(
        &records,
        "c2",
        "c2sha0000",
        None,
        PublicApiDeltasOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, RangeDeltasError::IdenticalEndpoints { .. }));

    let err = public_api_deltas(
        &records,
        "ffff",
        "c2",
        None,
        PublicApiDeltasOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, RangeDeltasError::MissingCommit { .. }));

    let err = public_api_deltas(
        &records,
        "c2",
        "c1",
        None,
        PublicApiDeltasOptions::default(),
    )
    .unwrap_err();
    assert!(matches!(err, RangeDeltasError::ReversedRange { .. }));
}

#[test]
fn public_api_deltas_callers_join_is_opt_in() {
    let mut records = synthetic_surface_records();
    let doomed_id = stable_id(&[
        "node",
        "symbol",
        "function",
        "repo_test",
        "src/lib.rs",
        "doomed",
        "0",
    ]);
    let private_id = stable_id(&[
        "node",
        "symbol",
        "function",
        "repo_test",
        "src/lib.rs",
        "private_fn",
        "0",
    ]);
    records.push(calls_edge(&private_id, &doomed_id, "c1sha0000", T1));

    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions::default(),
    )
    .expect("range should resolve");
    assert!(
        report.removed[0].internal_callers.is_none(),
        "caller join must be absent unless requested"
    );

    let report = public_api_deltas(
        &records,
        "c1",
        "c2",
        None,
        PublicApiDeltasOptions {
            with_callers: true,
            ..PublicApiDeltasOptions::default()
        },
    )
    .expect("range should resolve");
    let callers = report.removed[0]
        .internal_callers
        .as_ref()
        .expect("removed row must carry the caller join when requested");
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].record_id, private_id);
    assert_eq!(callers[0].name, Some("private_fn"));
    assert_eq!(callers[0].repo_relative_path, Some("src/lib.rs"));

    // signature_changed rows carry the join too; added rows never do.
    assert!(report.signature_changed[0].internal_callers.is_some());
    assert!(report.added[0].internal_callers.is_none());
}

// ---------------------------------------------------------------------------
// Seeded fixture repo: end-to-end through scan-history + CLI
// ---------------------------------------------------------------------------

/// Documented fixture range (issue #157 AC1): between `first..second` one
/// `pub` item is removed (`doomed`), one narrows `pub` -> `pub(crate)`
/// (`narrowed`), one `pub fn` signature changes (`sig_changes`), one new
/// `pub` item is added (`brand_new`), and one private item is modified
/// (`private_helper`).
fn seed_surface_fixture_repo(repo: &Path) -> [String; 2] {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);

    write(
        repo,
        "src/lib.rs",
        "pub fn keep() -> u32 { 1 }\npub fn doomed() -> u32 { 2 }\npub fn narrowed() -> u32 { 3 }\npub fn sig_changes(x: u32) -> u32 { x }\nfn private_helper() -> u32 { 8 }\n",
    );
    let first = commit_fixture(repo, "seed public surface", "2026-01-01T00:00:00Z");

    write(
        repo,
        "src/lib.rs",
        "pub fn keep() -> u32 { 1 }\npub(crate) fn narrowed() -> u32 { 3 }\npub fn sig_changes(x: u64) -> u64 { x }\nfn private_helper() -> u32 { 9 }\npub fn brand_new() -> u32 { 5 }\n",
    );
    let second = commit_fixture(
        repo,
        "remove, narrow, change signature, add",
        "2026-01-02T00:00:00Z",
    );

    [first, second]
}

#[test]
fn public_api_deltas_fixture_repo_classifies_expected_surface_changes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_surface_fixture_repo(&repo);

    let jsonl = scan_repository_history(&repo)
        .expect("history should scan")
        .to_jsonl()
        .expect("history graph should serialize");
    let records: Vec<GraphRecord> = jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should parse"))
        .collect();

    let status_before = git_output(&repo, ["status", "--porcelain"]);
    assert!(status_before.is_empty(), "fixture tree must start clean");

    let report = public_api_deltas(
        &records,
        &first,
        &second,
        None,
        PublicApiDeltasOptions::default(),
    )
    .expect("range should resolve");

    assert_eq!(names(&report.added), vec!["brand_new"]);
    assert_eq!(names(&report.removed), vec!["doomed"]);
    assert_eq!(names(&report.signature_changed), vec!["sig_changes"]);
    assert_eq!(names(&report.visibility_narrowed), vec!["narrowed"]);
    assert!(report.visibility_widened.is_empty());

    // The private symbol's modification never leaks into the surface groups.
    for rows in [
        &report.added,
        &report.removed,
        &report.signature_changed,
        &report.visibility_narrowed,
        &report.visibility_widened,
    ] {
        assert!(rows.iter().all(|row| row.name != "private_helper"));
    }
    assert_eq!(report.counts.internal_changes, 1);

    // Real before/after surface text from the recorded #124 fields.
    // Signatures are the issue #124 normalized headers (whitespace-collapsed).
    let sig = &report.signature_changed[0];
    assert_eq!(sig.before_signature, Some("fn sig_changes(x:u32)->u32"));
    assert_eq!(sig.after_signature, Some("fn sig_changes(x:u64)->u64"));
    let narrowed = &report.visibility_narrowed[0];
    assert_eq!(narrowed.before_visibility, Some("public"));
    assert_eq!(narrowed.after_visibility, Some("crate"));

    // Every row carries a citable record ID plus repo-relative file/span.
    for row in report
        .added
        .iter()
        .chain(&report.removed)
        .chain(&report.signature_changed)
        .chain(&report.visibility_narrowed)
    {
        assert!(!row.record_id.is_empty());
        assert_eq!(row.repo_relative_path, "src/lib.rs");
        assert!(row.span.is_some() || row.absent_span_reason.is_some());
        assert_eq!(row.commit, second);
        assert!(row.valid_time.is_some());
    }

    // History-backed resolution reads Git objects only (tree stays clean).
    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "query must not mutate the working tree: {status_after}"
    );

    // Byte-identical across 5 consecutive runs.
    let baseline = serde_json::to_string(&report).expect("report should serialize");
    for _ in 0..4 {
        let again = public_api_deltas(
            &records,
            &first,
            &second,
            None,
            PublicApiDeltasOptions::default(),
        )
        .expect("range should resolve");
        assert_eq!(
            baseline,
            serde_json::to_string(&again).expect("report should serialize"),
            "repeated runs must be byte-equivalent"
        );
    }
}

#[test]
fn public_api_deltas_cli_is_deterministic_and_redaction_safe() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_surface_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Unique prefixes must resolve exactly like full SHAs (AC6).
    let base_prefix = &first[..12];
    let mut outputs = Vec::new();
    for _ in 0..5 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "public-api-deltas", base_prefix, &second])
            .arg("--graph")
            .arg(&graph_path)
            .assert()
            .success();
        outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    for output in &outputs[1..] {
        assert_eq!(&outputs[0], output, "CLI output must be byte-identical");
    }

    let body: serde_json::Value = serde_json::from_str(&outputs[0]).expect("stdout should be JSON");
    assert_eq!(body["ok"], true);
    assert_eq!(body["base"], first);
    assert_eq!(body["head"], second);
    for group in [
        "added",
        "removed",
        "signature_changed",
        "visibility_narrowed",
        "visibility_widened",
        "diagnostics",
    ] {
        assert!(
            body[group].is_array(),
            "group {group} must always be present"
        );
    }
    assert!(body["counts"].is_object());
    assert!(
        body.get("internal").is_none(),
        "internal group must be omitted unless requested"
    );
    assert!(
        body["disclaimer"].as_str().unwrap().contains("not proof"),
        "disclaimer must label rows as observed surface changes"
    );
    assert_eq!(body["removed"][0]["change_class"], "removed");
    assert_eq!(body["removed"][0]["potentially_breaking"], true);

    // Bounded surface text only: signatures and visibility classes are
    // allowed, raw snapshot bodies are not.
    assert!(
        !outputs[0].contains("Source:"),
        "raw snapshot bodies must never leak into the response"
    );
    assert!(
        !outputs[0].contains("{ 1 }"),
        "function body text must never leak into the response"
    );

    let status_after = git_output(&repo, ["status", "--porcelain"]);
    assert!(
        status_after.is_empty(),
        "CLI query must not mutate the tree"
    );
}

#[test]
fn public_api_deltas_cli_flags_and_text_format() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_surface_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // --include-internal surfaces the clearly-labeled internal group.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "public-api-deltas", &first, &second])
        .arg("--graph")
        .arg(&graph_path)
        .arg("--include-internal")
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["internal"]["label"], "internal_not_public_surface");
    let internal_rows = body["internal"]["rows"].as_array().unwrap();
    assert!(
        internal_rows
            .iter()
            .any(|row| row["name"] == "private_helper"),
        "private symbol delta must appear in the internal group"
    );

    // Human-readable text format is available and deterministic.
    let mut text_outputs = Vec::new();
    for _ in 0..2 {
        let assert = CargoCommand::cargo_bin("egregore")
            .expect("binary should run")
            .args(["query", "public-api-deltas", &first, &second])
            .arg("--graph")
            .arg(&graph_path)
            .args(["--format", "text"])
            .assert()
            .success();
        text_outputs.push(String::from_utf8(assert.get_output().stdout.clone()).unwrap());
    }
    assert_eq!(text_outputs[0], text_outputs[1]);
    let text = &text_outputs[0];
    assert!(text.contains("removed"));
    assert!(text.contains("doomed"));
    assert!(text.contains("signature_changed"));
    assert!(text.contains("not proof"));
}

#[test]
fn public_api_deltas_cli_exit_codes_for_diagnostics() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir should be created");
    let [first, second] = seed_surface_fixture_repo(&repo);
    let graph_path = temp.path().join("history.graph.jsonl");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Identical endpoints: exit 1, machine-readable diagnostic.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "public-api-deltas", &first, &first])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["ok"], false);
    assert_eq!(body["error"]["error_type"], "identical_endpoints");

    // Unknown commit: exit 2.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "public-api-deltas", "ffffffffffff", &second])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(2);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "missing_commit");

    // Reversed range: exit 1.
    let assert = CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .args(["query", "public-api-deltas", &second, &first])
        .arg("--graph")
        .arg(&graph_path)
        .assert()
        .code(1);
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let body: serde_json::Value = serde_json::from_str(&out).expect("stdout should be JSON");
    assert_eq!(body["error"]["error_type"], "reversed_range");
}

// ---------------------------------------------------------------------------
// Git fixture helpers (mirrors tests/integration/range_deltas.rs)
// ---------------------------------------------------------------------------

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("relative path should have parent"))
        .expect("fixture directory should be created");
    fs::write(path, contents).expect("fixture file should be written");
}

fn commit_fixture(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        status.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );
    git_output(repo, ["rev-parse", "HEAD"])
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output should be utf-8")
        .trim()
        .to_owned()
}
