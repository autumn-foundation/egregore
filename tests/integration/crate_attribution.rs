#![allow(missing_docs)]
//! Owning-Cargo-package attribution for code facts (issue #117).
//!
//! Every code-fact node must carry a deterministic attribution to the Cargo
//! package that owns it — the package NAME plus the repo-relative path of the
//! owning `Cargo.toml`, resolved from the NEAREST ENCLOSING manifest — so an
//! agent can read one crate at a time and tell same-named symbols in different
//! member crates apart without reading manifests itself.
//!
//! These tests are written against the emitted JSONL and the `eg` CLI, so they
//! compile before the field or the `--package` flag exist.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::{Command, Stdio},
};

use aletheia_egregore::{
    ir::{NodeKind, SCHEMA_VERSION},
    scan_repository_at_with_override,
};
use serde_json::Value;

const FIXED_TIME: &str = "2026-06-14T00:00:00Z";
const REPO_ID: &str = "crate-attribution-fixture";

/// A manifest body carrying a sentinel comment, so a test can prove no manifest
/// body text ever reaches the graph.
const SENTINEL: &str = "SENTINEL_MANIFEST_BODY";

// ── fixture ──────────────────────────────────────────────────────────────────

/// The workspace fixture every scan-side test shares.
///
/// Shape (deliberately, one distinct hazard per entry):
/// - a VIRTUAL workspace root (no `[package]`), so files directly under it are
///   `virtual_manifest_only`, not owned by a fabricated root package;
/// - `crates/alpha` — a lib crate that DECLARES dependencies (mints a manifest
///   `File` node);
/// - `crates/alphabet` — the sibling-prefix trap: a `starts_with` matcher would
///   let `alpha` claim its files;
/// - `crates/beta` — a bin crate with NO `[dependencies]`, so manifest
///   extraction mints no record for it; it still owns its tree;
/// - `crates/alpha/vendor/inner` — a nested crate inside a crate; the nearest
///   manifest must win over the parent;
/// - `crates/alpha/scripts/tool.py` — a non-Rust source inside a crate dir;
/// - `scripts/gen.rs` — a stray `.rs` outside every crate;
/// - `handle` defined identically in `alpha` and `beta` — the AC2 collision.
fn write_workspace_fixture(root: &Path) {
    write_fixture(
        root,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/alphabet\", \"crates/beta\"]\n",
            ),
            (
                "crates/alpha/Cargo.toml",
                &format!(
                    "# {SENTINEL}\n[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n"
                ),
            ),
            (
                // A `mod` block so the fixture exercises all three node kinds
                // AC1 names by hand: File, Module, and Symbol.
                "crates/alpha/src/lib.rs",
                "pub mod nested { pub fn inner() -> u32 { 0 } }\npub fn handle() -> u32 { 1 }\npub fn only_in_alpha() -> u32 { 2 }\n",
            ),
            (
                "crates/alpha/scripts/tool.py",
                "def helper():\n    return 1\n",
            ),
            (
                "crates/alpha/vendor/inner/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/alpha/vendor/inner/src/lib.rs",
                "pub fn vendored() -> u32 { 3 }\n",
            ),
            (
                "crates/alphabet/Cargo.toml",
                "[package]\nname = \"alphabet\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/alphabet/src/lib.rs",
                "pub fn letters() -> u32 { 4 }\n",
            ),
            (
                "crates/beta/Cargo.toml",
                "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/beta/src/main.rs",
                "pub fn handle() -> u32 { 5 }\nfn main() {}\n",
            ),
            ("scripts/gen.rs", "pub fn generate() -> u32 { 6 }\n"),
        ],
    );
}

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

fn parse_jsonl(jsonl: &str) -> Vec<Value> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).expect("record should be valid JSON"))
        .collect()
}

fn scan_fixture(root: &Path) -> Vec<Value> {
    parse_jsonl(&scan_jsonl(root))
}

/// The `(package_name, manifest_path)` pair a record was attributed to, or the
/// unattributed reason, rendered as a compact comparable string.
fn attribution_of(record: &Value) -> Option<String> {
    let attribution = record.get("crate_attribution")?;
    let status = attribution["status"].as_str()?;
    if status == "attributed" {
        Some(format!(
            "{}@{}",
            attribution["package_name"].as_str().unwrap_or("?"),
            attribution["manifest_repo_relative_path"]
                .as_str()
                .unwrap_or("?")
        ))
    } else {
        Some(format!(
            "unattributed:{}",
            attribution["unattributed_reason"].as_str().unwrap_or("?")
        ))
    }
}

fn package_of(record: &Value) -> Option<&str> {
    record
        .get("crate_attribution")?
        .get("package_name")?
        .as_str()
}

/// Every node record of a path-bearing code-graph kind.
/// Whether a node kind carries attribution, DERIVED from the classifier rather
/// than restated.
///
/// A hand-maintained list here would be a fourth copy of the contract — and it
/// had already drifted, omitting `Change`, which silently excluded every
/// deletion and history `Change` record from these assertions.
fn kind_carries_attribution(kind: &str) -> bool {
    NodeKind::ALL.iter().any(|k| {
        k.as_str() == kind && aletheia_egregore::crate_attribution::carries_crate_attribution(*k)
    })
}

fn code_fact_nodes(records: &[Value]) -> Vec<&Value> {
    records
        .iter()
        .filter(|r| {
            r["record_type"] == "node"
                && r["repo_relative_path"].is_string()
                && r["kind"].as_str().is_some_and(kind_carries_attribution)
        })
        .collect()
}

/// Path → attribution, over every path-bearing code-graph node.
fn attribution_by_path(records: &[Value]) -> BTreeMap<String, BTreeSet<String>> {
    let mut map: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in code_fact_nodes(records) {
        let path = record["repo_relative_path"]
            .as_str()
            .expect("filtered on string path")
            .to_owned();
        let attribution =
            attribution_of(record).unwrap_or_else(|| "(attribution absent)".to_owned());
        map.entry(path).or_default().insert(attribution);
    }
    map
}

/// Content snapshot of a directory tree: path → bytes, `.git` included.
///
/// Content-level, not mtime-level, so a rewrite-with-identical-bytes cannot
/// masquerade as read-only.
fn snapshot_tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut snapshot = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = fs::read(&path) {
                let key = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                snapshot.insert(key, bytes);
            }
        }
    }
    snapshot
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

fn init_git(repo: &Path) {
    git(repo, ["init"]);
    git(repo, ["config", "user.email", "codegraph@example.invalid"]);
    git(repo, ["config", "user.name", "Codegraph Test"]);
    git(repo, ["config", "core.autocrlf", "false"]);
    git(repo, ["config", "commit.gpgsign", "false"]);
}

fn commit(repo: &Path, message: &str, date: &str) -> String {
    git(repo, ["add", "-A"]);
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(
        output.status.success(),
        "git commit failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let rev = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .output()
        .expect("git rev-parse should execute");
    String::from_utf8_lossy(&rev.stdout).trim().to_owned()
}

// ── fixture precondition ─────────────────────────────────────────────────────

/// Guards the fixture itself: if a later edit collapses it, the attribution
/// tests below would keep passing while proving nothing.
#[test]
fn fixture_shape_precondition() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());
    let paths: BTreeSet<&str> = code_fact_nodes(&records)
        .iter()
        .filter_map(|r| r["repo_relative_path"].as_str())
        .collect();

    for required in [
        "crates/alpha/src/lib.rs",
        "crates/alphabet/src/lib.rs",
        "crates/beta/src/main.rs",
        "crates/alpha/vendor/inner/src/lib.rs",
        "crates/alpha/scripts/tool.py",
        "scripts/gen.rs",
    ] {
        assert!(
            paths.contains(required),
            "fixture must index {required}; indexed: {paths:?}"
        );
    }
    // >= 2 member crates, >= 1 stray file outside every crate, >= 1 same-named
    // symbol pair across crates (AC1 / AC2 preconditions).
    let handles = records
        .iter()
        .filter(|r| {
            r["kind"] == "Symbol"
                && r["name"]
                    .as_str()
                    .is_some_and(|n| n.rsplit("::").next() == Some("handle"))
        })
        .count();
    assert_eq!(
        handles, 2,
        "fixture must define `handle` in exactly two crates"
    );
}

// ── AC1: every code fact carries attribution ─────────────────────────────────

#[test]
fn every_code_fact_node_carries_attribution() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    let unattributed_field: Vec<&str> = code_fact_nodes(&records)
        .iter()
        .filter(|r| r.get("crate_attribution").is_none())
        .filter_map(|r| r["repo_relative_path"].as_str())
        .collect();
    assert!(
        unattributed_field.is_empty(),
        "every path-bearing code-graph node must carry the field; missing on: {unattributed_field:?}"
    );
    assert!(
        !code_fact_nodes(&records).is_empty(),
        "the fixture must produce code-fact nodes"
    );
}

/// Field presence must be a TOTAL function over node kind, never partial.
///
/// That totality is what makes an ABSENT field mean "produced before issue
/// #117" rather than "this kind happens not to be covered".
#[test]
fn attribution_presence_is_total_over_node_kinds() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    for record in records.iter().filter(|r| r["record_type"] == "node") {
        let kind = record["kind"].as_str().unwrap_or_default();
        let carrying = matches!(
            kind,
            "File"
                | "Module"
                | "Symbol"
                | "Import"
                | "Diagnostic"
                | "PanicRiskSite"
                | "DebtMarker"
                | "UnsafeSite"
                | "DependencyDeclaration"
        );
        let has_path = record["repo_relative_path"].is_string();
        let has_field = record.get("crate_attribution").is_some();
        assert_eq!(
            has_field,
            carrying && has_path,
            "attribution presence must be total: kind={kind} path={} field={has_field}",
            record["repo_relative_path"]
        );
    }
    // Repository-scoped records must never claim a package owns them.
    for kind in ["Repository", "ScanCoverage"] {
        for record in records.iter().filter(|r| r["kind"] == kind) {
            assert!(
                record.get("crate_attribution").is_none(),
                "{kind} must never carry crate attribution"
            );
        }
    }
    // Edges and tombstones never carry it.
    for record in records.iter().filter(|r| r["record_type"] != "node") {
        assert!(
            record.get("crate_attribution").is_none(),
            "only node records carry crate attribution"
        );
    }
}

/// The attribution of every fixture path, pinned as literal expected tuples.
///
/// Hand-pinned rather than snapshot-compared, so a wrong regeneration is
/// visible in review rather than silently re-blessed.
#[test]
fn attribution_expectations_are_hand_pinned_tuples() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());
    let observed = attribution_by_path(&records);

    let expected: &[(&str, &str)] = &[
        ("crates/alpha/Cargo.toml", "alpha@crates/alpha/Cargo.toml"),
        ("crates/alpha/src/lib.rs", "alpha@crates/alpha/Cargo.toml"),
        (
            "crates/alpha/scripts/tool.py",
            "alpha@crates/alpha/Cargo.toml",
        ),
        (
            "crates/alpha/vendor/inner/src/lib.rs",
            "inner@crates/alpha/vendor/inner/Cargo.toml",
        ),
        (
            "crates/alphabet/src/lib.rs",
            "alphabet@crates/alphabet/Cargo.toml",
        ),
        ("crates/beta/src/main.rs", "beta@crates/beta/Cargo.toml"),
        ("scripts/gen.rs", "unattributed:virtual_manifest_only"),
    ];
    for (path, attribution) in expected {
        let actual = observed
            .get(*path)
            .unwrap_or_else(|| panic!("no records for {path}; observed: {observed:#?}"));
        assert_eq!(
            actual.iter().cloned().collect::<Vec<_>>(),
            vec![(*attribution).to_owned()],
            "wrong attribution for {path}"
        );
    }
}

/// The nearest manifest wins, and the parent crate never claims the nested one.
#[test]
fn nested_vendor_crate_wins_over_parent() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    let claimed_by_alpha: Vec<&str> = code_fact_nodes(&records)
        .iter()
        .filter(|r| package_of(r) == Some("alpha"))
        .filter_map(|r| r["repo_relative_path"].as_str())
        .filter(|p| p.starts_with("crates/alpha/vendor/inner/"))
        .collect();
    assert!(
        claimed_by_alpha.is_empty(),
        "the parent crate must not claim the nested crate's files: {claimed_by_alpha:?}"
    );
    assert!(
        code_fact_nodes(&records)
            .iter()
            .any(|r| package_of(r) == Some("inner")),
        "the nested crate must own its own files"
    );
}

/// `crates/alpha` must never claim `crates/alphabet/**` — the sibling-prefix
/// trap a `str::starts_with` matcher falls into.
#[test]
fn sibling_prefix_non_bleed_end_to_end() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    for (package, foreign_prefix) in [("alpha", "crates/alphabet/"), ("alphabet", "crates/alpha/")]
    {
        // Anti-vacuity: an all-absent field would make the bleed check pass
        // while proving nothing.
        assert!(
            code_fact_nodes(&records)
                .iter()
                .any(|r| package_of(r) == Some(package)),
            "package `{package}` must own at least one record"
        );
        let bleeds: Vec<&str> = code_fact_nodes(&records)
            .iter()
            .filter(|r| package_of(r) == Some(package))
            .filter_map(|r| r["repo_relative_path"].as_str())
            .filter(|p| p.starts_with(foreign_prefix))
            .collect();
        assert!(
            bleeds.is_empty(),
            "package `{package}` bled into `{foreign_prefix}`: {bleeds:?}"
        );
    }
}

/// A member crate declaring NO dependencies mints no `DependencyDeclaration`
/// and no manifest `File` node — yet it still owns its whole directory tree.
///
/// This is the case a query-time derivation from manifest records could never
/// answer, and the reason attribution is stamped at scan time.
#[test]
fn dependency_free_member_crate_is_attributed() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    assert!(
        !records
            .iter()
            .any(|r| r["repo_relative_path"] == "crates/beta/Cargo.toml"),
        "precondition: the dependency-free manifest mints no graph record"
    );
    let beta_paths: BTreeSet<&str> = code_fact_nodes(&records)
        .iter()
        .filter(|r| package_of(r) == Some("beta"))
        .filter_map(|r| r["repo_relative_path"].as_str())
        .collect();
    assert!(
        beta_paths.contains("crates/beta/src/main.rs"),
        "the dependency-free crate must still own its sources; got {beta_paths:?}"
    );
}

/// The nearest enclosing manifest of a manifest is itself.
#[test]
fn manifest_file_node_self_attributes() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    let manifest_node = records
        .iter()
        .find(|r| r["kind"] == "File" && r["repo_relative_path"] == "crates/alpha/Cargo.toml")
        .expect("the dependency-declaring manifest mints a File node");
    assert_eq!(
        attribution_of(manifest_node).as_deref(),
        Some("alpha@crates/alpha/Cargo.toml")
    );
}

// ── AC4: unattributed markers ────────────────────────────────────────────────

/// A stray `.rs` under a virtual workspace root is `virtual_manifest_only`, and
/// carries no package name at all.
#[test]
fn stray_file_is_unattributed_with_virtual_manifest_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    let strays: Vec<&Value> = code_fact_nodes(&records)
        .into_iter()
        .filter(|r| r["repo_relative_path"] == "scripts/gen.rs")
        .collect();
    assert!(!strays.is_empty(), "the stray file must be indexed");
    for record in strays {
        let attribution = record
            .get("crate_attribution")
            .expect("stray records still carry the computed field");
        assert_eq!(attribution["status"], "unattributed");
        assert_eq!(attribution["unattributed_reason"], "virtual_manifest_only");
        assert!(
            attribution.get("package_name").is_none(),
            "an unattributed record must never carry a package name"
        );
        assert!(
            attribution.get("manifest_repo_relative_path").is_none(),
            "an unattributed record must never cite a manifest"
        );
    }
}

/// With no manifest anywhere, the reason is `no_enclosing_manifest` — a
/// different fact from "under a virtual root".
#[test]
fn stray_with_no_manifest_anywhere_is_no_enclosing_manifest() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[("src/loose.rs", "pub fn loose() -> u32 { 1 }\n")],
    );
    let records = scan_fixture(temp.path());

    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "src/loose.rs")
        .expect("the loose file must be indexed");
    assert_eq!(
        attribution_of(record).as_deref(),
        Some("unattributed:no_enclosing_manifest")
    );
}

/// An unparseable manifest STOPS the walk: inheriting the root package's name
/// across a broken boundary would fabricate an attribution.
#[test]
fn unparseable_manifest_stops_walk_and_never_inherits_ancestor() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn at_root() -> u32 { 1 }\n"),
            (
                "crates/broken/Cargo.toml",
                "this is not = = valid toml [[[\n",
            ),
            ("crates/broken/src/lib.rs", "pub fn broken() -> u32 { 2 }\n"),
        ],
    );
    let records = scan_fixture(temp.path());

    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "crates/broken/src/lib.rs")
        .expect("the file under the broken manifest must still be indexed");
    assert_eq!(
        attribution_of(record).as_deref(),
        Some("unattributed:unparseable_manifest"),
        "a broken manifest must fail closed, never inherit `root`"
    );
    // The healthy sibling is unaffected.
    let root_record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "src/lib.rs")
        .expect("root file indexed");
    assert_eq!(
        attribution_of(root_record).as_deref(),
        Some("root@Cargo.toml")
    );
}

/// A `[package]` whose name Cargo would reject is `unnamed_package` — a
/// distinct, actionable fact from "virtual root", and never a sanitized name.
#[test]
fn unnamed_package_manifest_is_its_own_reason() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/nameless/Cargo.toml",
                "[package]\nname = \"bad name\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/nameless/src/lib.rs",
                "pub fn nameless() -> u32 { 1 }\n",
            ),
        ],
    );
    let records = scan_fixture(temp.path());

    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "crates/nameless/src/lib.rs")
        .expect("indexed");
    assert_eq!(
        attribution_of(record).as_deref(),
        Some("unattributed:unnamed_package")
    );
    let jsonl = scan_jsonl(temp.path());
    assert!(
        !jsonl.contains("bad name") && !jsonl.contains("badname") && !jsonl.contains("bad_name"),
        "an invalid package name must never be emitted, sanitized, or normalized"
    );
}

/// A package name is read from the manifest, never derived from the directory.
#[test]
fn package_name_is_never_derived_from_the_directory_name() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "crates/widget-dir/Cargo.toml",
                "[package]\nname = \"totally-different\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/widget-dir/src/lib.rs", "pub fn w() -> u32 { 1 }\n"),
        ],
    );
    let records = scan_fixture(temp.path());

    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "crates/widget-dir/src/lib.rs")
        .expect("indexed");
    assert_eq!(
        attribution_of(record).as_deref(),
        Some("totally-different@crates/widget-dir/Cargo.toml")
    );
    let packages: BTreeSet<&str> = code_fact_nodes(&records)
        .iter()
        .filter_map(|r| package_of(r))
        .collect();
    assert!(
        !packages.contains("widget-dir"),
        "a directory name must never surface as a package"
    );
}

// ── AC6: redaction-safety ────────────────────────────────────────────────────

/// The manifest body never enters the graph — only the package name and the
/// manifest's repo-relative path.
#[test]
fn no_manifest_body_text_in_scan_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let jsonl = scan_jsonl(temp.path());
    assert!(
        jsonl.contains("crate_attribution"),
        "anti-vacuity: attribution must be present for this check to mean anything"
    );
    assert!(
        !jsonl.contains(SENTINEL),
        "manifest body text must never reach the graph"
    );
}

/// Emitted manifest paths are repo-relative: never absolute, never escaping,
/// never leaking the scanning machine's temp directory.
#[test]
fn manifest_paths_are_repo_relative_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());
    let temp_component = temp
        .path()
        .file_name()
        .expect("temp dir has a name")
        .to_string_lossy()
        .into_owned();

    let mut checked = 0_usize;
    for record in code_fact_nodes(&records) {
        let Some(manifest) = record
            .get("crate_attribution")
            .and_then(|a| a.get("manifest_repo_relative_path"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        checked += 1;
        assert!(!manifest.starts_with('/'), "absolute path: {manifest}");
        assert!(!manifest.contains(".."), "escaping path: {manifest}");
        assert!(!manifest.contains('\\'), "backslash path: {manifest}");
        assert!(
            !manifest.contains(temp_component.as_str()),
            "temp-dir component leaked: {manifest}"
        );
        assert!(
            manifest.ends_with("Cargo.toml"),
            "manifest handle must name a Cargo.toml: {manifest}"
        );
    }
    assert!(checked > 0, "no manifest handles were checked");
}

/// The ancestor walk is hard-bounded at the repository root: a `Cargo.toml`
/// sitting in the scan root's PARENT directory must never be consulted.
#[test]
fn ancestor_walk_never_reads_above_repo_root() {
    let temp = tempfile::tempdir().expect("temp dir");
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"outside\"\nversion = \"0.1.0\"\n",
    )
    .expect("outside manifest written");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_fixture(&repo, &[("src/lib.rs", "pub fn inner() -> u32 { 1 }\n")]);

    let jsonl = scan_jsonl(&repo);
    assert!(
        !jsonl.contains("outside"),
        "a manifest above the repository root must never be consulted"
    );
    let records = parse_jsonl(&jsonl);
    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "src/lib.rs")
        .expect("indexed");
    assert_eq!(
        attribution_of(record).as_deref(),
        Some("unattributed:no_enclosing_manifest")
    );
}

/// A `Cargo.toml` under `target/` is build output, never a manifest.
#[test]
fn manifests_under_target_are_ignored() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn at_root() -> u32 { 1 }\n"),
            (
                "target/ghostpkg/Cargo.toml",
                "[package]\nname = \"ghost\"\nversion = \"0.1.0\"\n",
            ),
        ],
    );
    let jsonl = scan_jsonl(temp.path());
    assert!(
        !jsonl.contains("ghost"),
        "a manifest under target/ must never be harvested"
    );
    // Anti-vacuity: the harvest DID run — the root manifest was found.
    let records = parse_jsonl(&jsonl);
    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "src/lib.rs")
        .expect("indexed");
    assert_eq!(attribution_of(record).as_deref(), Some("root@Cargo.toml"));
}

/// `cargo.toml` (lowercase) is not a Cargo manifest.
#[test]
fn lowercase_cargo_toml_is_not_a_manifest() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            ("crates/x/cargo.toml", "[package]\nname = \"lowercase\"\n"),
            ("crates/x/src/lib.rs", "pub fn x() -> u32 { 1 }\n"),
        ],
    );
    let jsonl = scan_jsonl(temp.path());
    assert!(
        !jsonl.contains("lowercase"),
        "manifest matching is case-sensitive"
    );
    let records = parse_jsonl(&jsonl);
    let record = code_fact_nodes(&records)
        .into_iter()
        .find(|r| r["repo_relative_path"] == "crates/x/src/lib.rs")
        .expect("indexed");
    assert_eq!(
        attribution_of(record).as_deref(),
        Some("unattributed:no_enclosing_manifest"),
        "a lowercase cargo.toml owns nothing"
    );
}

// ── AC8: determinism and read-only ───────────────────────────────────────────

/// Byte-identical attribution and ordering across five consecutive scans.
#[test]
fn scan_attribution_byte_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let baseline = scan_jsonl(temp.path());
    assert!(
        baseline.contains("crate_attribution"),
        "the determinism check must actually cover attribution"
    );
    for run in 1..5 {
        assert_eq!(
            scan_jsonl(temp.path()),
            baseline,
            "scan output diverged on run {run}"
        );
    }
}

/// The same fixture in two different temp roots yields identical attribution —
/// catching absolute-path and wall-clock leaks in one assertion.
#[test]
fn same_fixture_in_two_tempdirs_yields_identical_attribution() {
    let first = tempfile::tempdir().expect("temp dir");
    let second = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(first.path());
    write_workspace_fixture(second.path());
    let left = attribution_by_path(&scan_fixture(first.path()));
    assert!(!left.is_empty(), "anti-vacuity: the map must be populated");
    assert!(
        left.values().flatten().any(|a| a.contains('@')),
        "anti-vacuity: at least one path must be attributed"
    );
    assert_eq!(
        left,
        attribution_by_path(&scan_fixture(second.path())),
        "attribution must not depend on the repository's absolute location"
    );
}

/// Scanning mutates nothing: not the sources, not the manifests, not `.git`.
#[test]
fn scan_does_not_mutate_the_working_tree() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    init_git(temp.path());
    commit(temp.path(), "seed", "2026-06-14T00:00:00Z");

    let before = snapshot_tree(temp.path());
    let jsonl = scan_jsonl(temp.path());
    assert!(
        jsonl.contains("crate_attribution"),
        "anti-vacuity: attribution must have been computed during this scan"
    );
    let after = snapshot_tree(temp.path());
    assert_eq!(before, after, "the scan must not mutate the working tree");
}

// ── AC7: schema version ──────────────────────────────────────────────────────

/// The pinned corpus record IDs track the current schema version, so a future
/// bump cannot silently leave them stale.
#[test]
fn token_cost_corpus_ids_match_current_schema_version() {
    let corpus = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/token_cost_corpus.json"),
    )
    .expect("corpus should be readable");
    let corpus: Value = serde_json::from_str(&corpus).expect("corpus is JSON");
    let prefix = format!("codegraph:v{SCHEMA_VERSION}:");
    let questions = corpus["questions"].as_array().expect("questions array");
    assert!(!questions.is_empty());
    for question in questions {
        let id = question["expected_record_id"]
            .as_str()
            .expect("expected_record_id is a string");
        assert!(
            id.starts_with(&prefix),
            "pinned corpus id {id} is stale; expected prefix {prefix}"
        );
    }
}

// ── eg refresh: attribution is recomputed, never cached ──────────────────────

fn refresh_attribution(root: &Path, cache: &Path, at: &str) -> BTreeMap<String, BTreeSet<String>> {
    let scan = aletheia_egregore::incremental::scan_repository_incremental_at(root, cache, at)
        .expect("incremental scan should succeed");
    let jsonl = scan.graph.to_jsonl().expect("graph should serialize");
    attribution_by_path(&parse_jsonl(&jsonl))
}

/// A package RENAME with byte-identical sources must still re-attribute.
///
/// The per-file cache reuses records for unchanged sources, so attribution
/// cannot be cached alongside them: `crates/alpha/src/lib.rs` does not change
/// when `crates/alpha/Cargo.toml` renames its package, yet its owner does.
#[test]
fn refresh_reattributes_after_manifest_rename_with_unchanged_sources() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);
    let cache = temp.path().join("cache.json");

    let before = refresh_attribution(&repo, &cache, FIXED_TIME);
    assert_eq!(
        before.get("crates/alpha/src/lib.rs"),
        Some(&BTreeSet::from(
            ["alpha@crates/alpha/Cargo.toml".to_owned()]
        ))
    );

    // Rename the package; leave every source file byte-identical.
    let sources_before = fs::read_to_string(repo.join("crates/alpha/src/lib.rs")).expect("read");
    fs::write(
        repo.join("crates/alpha/Cargo.toml"),
        format!("# {SENTINEL}\n[package]\nname = \"alpha-core\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n"),
    )
    .expect("manifest rewritten");
    assert_eq!(
        fs::read_to_string(repo.join("crates/alpha/src/lib.rs")).expect("read"),
        sources_before,
        "precondition: sources must be byte-identical across the rename"
    );

    let after = refresh_attribution(&repo, &cache, "2026-06-14T00:00:01Z");
    assert_eq!(
        after.get("crates/alpha/src/lib.rs"),
        Some(&BTreeSet::from([
            "alpha-core@crates/alpha/Cargo.toml".to_owned()
        ])),
        "attribution must be recomputed on refresh, never replayed from cache"
    );
}

/// Adding a manifest carves a subtree out of its parent package.
#[test]
fn refresh_reattributes_after_manifest_addition_carves_subtree() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn at_root() -> u32 { 1 }\n"),
            ("sub/src/lib.rs", "pub fn in_sub() -> u32 { 2 }\n"),
        ],
    );
    let cache = temp.path().join("cache.json");

    let before = refresh_attribution(&repo, &cache, FIXED_TIME);
    assert_eq!(
        before.get("sub/src/lib.rs"),
        Some(&BTreeSet::from(["root@Cargo.toml".to_owned()]))
    );

    fs::write(
        repo.join("sub/Cargo.toml"),
        "[package]\nname = \"sub\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest written");

    let after = refresh_attribution(&repo, &cache, "2026-06-14T00:00:01Z");
    assert_eq!(
        after.get("sub/src/lib.rs"),
        Some(&BTreeSet::from(["sub@sub/Cargo.toml".to_owned()])),
        "a newly added manifest must carve its subtree out of the parent package"
    );
    assert_eq!(
        after.get("src/lib.rs"),
        Some(&BTreeSet::from(["root@Cargo.toml".to_owned()])),
        "the parent package keeps everything outside the new manifest's subtree"
    );
}

/// A refresh and a full scan of the same tree must agree exactly.
#[test]
fn refresh_and_full_scan_agree_on_attribution() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);
    let cache = temp.path().join("cache.json");

    // Seed the cache, then refresh again so the second run reuses it.
    let _ = refresh_attribution(&repo, &cache, FIXED_TIME);
    let refreshed = refresh_attribution(&repo, &cache, FIXED_TIME);
    let scanned = attribution_by_path(&parse_jsonl(
        &aletheia_egregore::scan_repository_at(&repo, FIXED_TIME)
            .expect("full scan")
            .to_jsonl()
            .expect("serialize"),
    ));
    assert!(!refreshed.is_empty(), "anti-vacuity");
    assert_eq!(
        refreshed, scanned,
        "the incremental and full-scan paths must attribute identically"
    );
}

// ── eg scan-history: per-commit attribution from Git objects ─────────────────

fn history_attribution(repo: &Path) -> BTreeMap<String, BTreeMap<String, BTreeSet<String>>> {
    let jsonl = aletheia_egregore::scan_repository_history(repo)
        .expect("history replay should succeed")
        .to_jsonl()
        .expect("graph should serialize");
    let records = parse_jsonl(&jsonl);
    // commit sha -> path -> attribution
    let mut map: BTreeMap<String, BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for record in code_fact_nodes(&records) {
        let Some(sha) = record["temporal"]["git_commit"].as_str() else {
            continue;
        };
        let path = record["repo_relative_path"]
            .as_str()
            .expect("filtered on string path")
            .to_owned();
        let attribution =
            attribution_of(record).unwrap_or_else(|| "(attribution absent)".to_owned());
        map.entry(sha.to_owned())
            .or_default()
            .entry(path)
            .or_default()
            .insert(attribution);
    }
    map
}

/// The load-bearing history test: attribution must be resolved against EACH
/// COMMIT'S OWN manifest tree, not the last one replayed.
///
/// An ADR-0004 symbol ID carries no commit component, so the same stable record
/// ID recurs across commits. Applying one commit's manifest index across the
/// whole graph would stamp every historical version with the final tree's
/// packages — a fabricated fact at a pinned historical point.
#[test]
fn history_attribution_is_per_commit_not_whole_graph() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write_fixture(
        &repo,
        &[
            (
                "crates/alpha/Cargo.toml",
                "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/alpha/src/lib.rs", "pub fn stable() -> u32 { 1 }\n"),
        ],
    );
    let first = commit(&repo, "seed alpha", "2026-06-01T00:00:00Z");

    // Rename the package only; the source file stays byte-identical.
    fs::write(
        repo.join("crates/alpha/Cargo.toml"),
        "[package]\nname = \"alpha-core\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest rewritten");
    let second = commit(&repo, "rename package", "2026-06-02T00:00:00Z");

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&first]["crates/alpha/src/lib.rs"],
        BTreeSet::from(["alpha@crates/alpha/Cargo.toml".to_owned()]),
        "the first commit must carry the package name AS OF that commit"
    );
    assert_eq!(
        by_commit[&second]["crates/alpha/src/lib.rs"],
        BTreeSet::from(["alpha-core@crates/alpha/Cargo.toml".to_owned()]),
        "the second commit must carry the renamed package"
    );
}

/// A commit that ADDS a manifest changes ownership from that commit onward.
#[test]
fn history_attribution_when_manifest_is_added() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write_fixture(&repo, &[("src/lib.rs", "pub fn thing() -> u32 { 1 }\n")]);
    let before = commit(&repo, "no manifest", "2026-06-01T00:00:00Z");

    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"later\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest written");
    let after = commit(&repo, "add manifest", "2026-06-02T00:00:00Z");

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&before]["src/lib.rs"],
        BTreeSet::from(["unattributed:no_enclosing_manifest".to_owned()]),
        "before the manifest existed, the file was owned by nothing"
    );
    assert_eq!(
        by_commit[&after]["src/lib.rs"],
        BTreeSet::from(["later@Cargo.toml".to_owned()])
    );
}

/// A commit that REMOVES a manifest un-owns its tree from that commit onward.
#[test]
fn history_attribution_when_manifest_is_removed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);

    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"doomed\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn thing() -> u32 { 1 }\n"),
        ],
    );
    let before = commit(&repo, "with manifest", "2026-06-01T00:00:00Z");

    fs::remove_file(repo.join("Cargo.toml")).expect("manifest removed");
    let after = commit(&repo, "drop manifest", "2026-06-02T00:00:00Z");

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&before]["src/lib.rs"],
        BTreeSet::from(["doomed@Cargo.toml".to_owned()])
    );
    assert_eq!(
        by_commit[&after]["src/lib.rs"],
        BTreeSet::from(["unattributed:no_enclosing_manifest".to_owned()]),
        "removing the manifest must un-own the tree, not keep a stale package"
    );
}

/// A committed `target/` manifest is build output, never an owner.
#[test]
fn history_manifest_listing_prunes_target() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"real\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn thing() -> u32 { 1 }\n"),
            (
                "target/ghostpkg/Cargo.toml",
                "[package]\nname = \"ghost\"\nversion = \"0.1.0\"\n",
            ),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let by_commit = history_attribution(&repo);
    let attributions: BTreeSet<&String> = by_commit
        .values()
        .flat_map(BTreeMap::values)
        .flatten()
        .collect();
    assert!(
        !attributions.iter().any(|a| a.contains("ghost")),
        "a committed target/ manifest must never own anything; got {attributions:?}"
    );
    // Anti-vacuity: the real manifest IS harvested.
    assert!(
        attributions.contains(&"real@Cargo.toml".to_owned()),
        "the real manifest must still own the tree; got {attributions:?}"
    );
}

/// `eg scan` at HEAD and `eg scan-history` at the HEAD commit must agree.
#[test]
fn history_and_scan_agree_at_head() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_workspace_fixture(&repo);
    let head = commit(&repo, "seed workspace", "2026-06-01T00:00:00Z");

    let by_commit = history_attribution(&repo);
    let history_at_head = by_commit
        .get(&head)
        .expect("HEAD commit must be replayed")
        .clone();
    let scanned = attribution_by_path(&scan_fixture(&repo));

    // The history path indexes only SOURCE files (it mints no manifest File or
    // DependencyDeclaration records), so compare over the shared source paths.
    let shared: BTreeSet<&String> = history_at_head
        .keys()
        .filter(|path| scanned.contains_key(*path))
        .collect();
    assert!(
        shared.len() >= 5,
        "anti-vacuity: too few shared paths to compare: {shared:?}"
    );
    for path in shared {
        assert_eq!(
            history_at_head[path], scanned[path],
            "history and scan disagree on {path}"
        );
    }
}

/// A manifest whose blob is unparseable must not abort the replay.
#[test]
fn unparseable_manifest_blob_does_not_abort_replay() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            ("crates/broken/Cargo.toml", "not = = toml [[[\n"),
            ("crates/broken/src/lib.rs", "pub fn thing() -> u32 { 1 }\n"),
        ],
    );
    let sha = commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&sha]["crates/broken/src/lib.rs"],
        BTreeSet::from(["unattributed:unparseable_manifest".to_owned()])
    );
}

/// History replay reads Git objects only; the checkout is byte-identical after.
#[test]
fn history_attribution_does_not_mutate_working_tree() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_workspace_fixture(&repo);
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let before = snapshot_tree(&repo);
    let jsonl = aletheia_egregore::scan_repository_history(&repo)
        .expect("history replay")
        .to_jsonl()
        .expect("serialize");
    assert!(
        jsonl.contains("crate_attribution"),
        "anti-vacuity: attribution must have been computed"
    );
    let after = snapshot_tree(&repo);
    assert_eq!(
        before, after,
        "history replay must not mutate the checkout, including .git"
    );
}

/// A manifest under a non-ASCII path must be harvested (depends on the
/// `core.quotePath=false` pre-fix).
#[test]
fn history_attributes_non_ascii_manifest_paths() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "crates/café/Cargo.toml",
                "[package]\nname = \"cafe-pkg\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/café/src/lib.rs", "pub fn brew() -> u32 { 1 }\n"),
        ],
    );
    let sha = commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&sha]["crates/café/src/lib.rs"],
        BTreeSet::from(["cafe-pkg@crates/café/Cargo.toml".to_owned()]),
        "a non-ASCII manifest path must resolve, not report unparseable"
    );
}

// ── store round-trip: write and read MUST stay symmetric ─────────────────────

#[cfg(feature = "embedded-aletheiadb")]
fn egregore_bin() -> assert_cmd::Command {
    assert_cmd::Command::cargo_bin("egregore").expect("binary should run")
}

#[cfg(feature = "embedded-aletheiadb")]
fn ingest_embedded(graph_path: &Path, data_dir: &Path) {
    egregore_bin()
        .arg("ingest")
        .arg(graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success();
}

#[cfg(feature = "embedded-aletheiadb")]
fn export_store(data_dir: &Path, out: &Path) -> String {
    egregore_bin()
        .arg("export")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--out")
        .arg(out)
        .assert()
        .success();
    fs::read_to_string(out).expect("export should be readable")
}

#[cfg(feature = "embedded-aletheiadb")]
fn inspect_store_stdout(data_dir: &Path) -> String {
    let output = egregore_bin()
        .arg("inspect")
        .arg("--data-dir")
        .arg(data_dir)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(output).expect("inspect output should be UTF-8")
}

/// Attribution must survive scan → ingest → export unchanged.
///
/// The store adapter maps node fields to properties BY HAND, so nothing is
/// automatic here: a field written but never read back would round-trip to
/// `None` and vanish from every store-backed answer.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn crate_attribution_survives_the_store_round_trip() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);

    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(&repo)).expect("graph written");
    let data_dir = temp.path().join("store");
    ingest_embedded(&graph_path, &data_dir);

    let exported = export_store(&data_dir, &temp.path().join("export.jsonl"));
    let exported_attribution = attribution_by_path(&parse_jsonl(&exported));
    let scanned_attribution = attribution_by_path(&parse_jsonl(
        &fs::read_to_string(&graph_path).expect("graph readable"),
    ));

    assert!(!scanned_attribution.is_empty(), "anti-vacuity");
    assert_eq!(
        exported_attribution, scanned_attribution,
        "attribution must survive the store round-trip byte-for-byte"
    );
    // Every closed reason variant round-trips, not just the attributed shape.
    assert!(
        exported.contains("virtual_manifest_only"),
        "an unattributed reason must survive the round-trip"
    );
}

/// Re-ingesting the SAME graph must be idempotent.
///
/// `compare_node_record` is full structural equality of the record reconstructed
/// from the store, so a property written but not read back makes every record
/// compare unequal and mints a NEW physical version on every re-ingest —
/// forever, silently, with no validator catching it.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn reingest_of_attributed_graph_is_idempotent() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_jsonl(&repo);
    assert!(
        jsonl.contains("crate_attribution"),
        "anti-vacuity: the graph must carry attribution"
    );
    fs::write(&graph_path, &jsonl).expect("graph written");
    let data_dir = temp.path().join("store");

    ingest_embedded(&graph_path, &data_dir);
    let after_first = inspect_store_stdout(&data_dir);
    let export_first = export_store(&data_dir, &temp.path().join("export1.jsonl"));

    ingest_embedded(&graph_path, &data_dir);
    let after_second = inspect_store_stdout(&data_dir);
    let export_second = export_store(&data_dir, &temp.path().join("export2.jsonl"));

    assert_eq!(
        after_first, after_second,
        "a re-ingest of an unchanged graph must write no new physical versions"
    );
    assert_eq!(
        export_first, export_second,
        "exports must be byte-identical across a re-ingest"
    );
}

/// Every carrying node kind that the fixture produces round-trips through the
/// store — not just `Symbol`.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn every_carrying_node_kind_roundtrips_through_the_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);
    // Add shapes that mint the other carrying kinds.
    write_fixture(
        &repo,
        &[(
            "crates/alpha/src/extra.rs",
            "// TODO: debt marker here\nuse std::fmt::Debug;\npub fn risky(v: Option<u32>) -> u32 { v.unwrap() }\npub unsafe fn danger() {}\n",
        )],
    );

    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = scan_jsonl(&repo);
    fs::write(&graph_path, &jsonl).expect("graph written");
    let data_dir = temp.path().join("store");
    ingest_embedded(&graph_path, &data_dir);
    let exported = export_store(&data_dir, &temp.path().join("export.jsonl"));

    let kinds_in_scan = attributed_kinds(&parse_jsonl(&jsonl));
    let kinds_in_export = attributed_kinds(&parse_jsonl(&exported));
    assert!(
        kinds_in_scan.len() >= 4,
        "anti-vacuity: the fixture must exercise several carrying kinds, got {kinds_in_scan:?}"
    );
    assert_eq!(
        kinds_in_scan, kinds_in_export,
        "every carrying node kind must keep its attribution through the store"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn attributed_kinds(records: &[Value]) -> BTreeSet<String> {
    records
        .iter()
        .filter(|r| r["record_type"] == "node" && r.get("crate_attribution").is_some())
        .filter_map(|r| r["kind"].as_str().map(str::to_owned))
        .collect()
}

// ── eg query symbol/symbols --package ────────────────────────────────────────

fn write_graph(root: &Path) -> std::path::PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);
    let graph = root.join("graph.jsonl");
    fs::write(&graph, scan_jsonl(&repo)).expect("graph written");
    graph
}

struct CliRun {
    code: i32,
    stdout: String,
    stderr: String,
}

fn run_query(args: &[&str]) -> CliRun {
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .args(args)
        .output()
        .expect("egregore should execute");
    CliRun {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn rows(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("row should be JSON"))
        .collect()
}

/// AC2: two symbols with the same qualified name in different member crates are
/// distinguishable by their crate attribution in query output.
#[test]
fn same_qualified_name_in_two_crates_is_distinguishable_by_package() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let unscoped = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph.to_str().unwrap(),
    ]);
    assert_eq!(unscoped.code, 0, "stderr: {}", unscoped.stderr);
    let unscoped_rows = rows(&unscoped.stdout);
    assert_eq!(
        unscoped_rows.len(),
        2,
        "both crates define `handle`: {}",
        unscoped.stdout
    );
    let packages: BTreeSet<&str> = unscoped_rows
        .iter()
        .filter_map(|r| r["crate_attribution"]["package_name"].as_str())
        .collect();
    assert_eq!(
        packages,
        BTreeSet::from(["alpha", "beta"]),
        "each row must name its own owning package"
    );
    let ids: BTreeSet<&str> = unscoped_rows
        .iter()
        .filter_map(|r| r["record_id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2, "the two symbols are distinct records");

    // Scoping to one package returns exactly one of them.
    let scoped = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    assert_eq!(scoped.code, 0, "stderr: {}", scoped.stderr);
    let scoped_rows = rows(&scoped.stdout);
    assert_eq!(scoped_rows.len(), 1);
    assert_eq!(scoped_rows[0]["crate_attribution"]["package_name"], "alpha");
}

/// AC3: `--package` precision AND recall are both 100% against the crate's own
/// file set — asserted as SET EQUALITY, plus empty intersection with every
/// sibling crate.
#[test]
fn package_scope_precision_and_recall_are_both_100() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let scoped = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    assert_eq!(scoped.code, 0, "stderr: {}", scoped.stderr);
    let observed: BTreeSet<String> = rows(&scoped.stdout)
        .iter()
        .filter_map(|r| r["repo_relative_path"].as_str().map(str::to_owned))
        .collect();

    // The member crate's file set, derived from the documented manifest walk:
    // every symbol-bearing file under `crates/alpha/` EXCEPT those under the
    // nested `vendor/inner` package, which is its own crate.
    //
    // `scripts/tool.py` is included deliberately, pinning documented limit L6:
    // attribution is directory containment, so a non-Rust source inside a crate
    // directory IS attributed to that Cargo package even though Cargo never
    // compiles it. A carve-out would make the rule non-uniform and more
    // surprising than the uniform rule plus a stated limit.
    let expected = BTreeSet::from([
        "crates/alpha/src/lib.rs".to_owned(),
        "crates/alpha/scripts/tool.py".to_owned(),
    ]);
    assert_eq!(
        observed, expected,
        "package scope must return exactly the crate's own symbol-bearing files"
    );

    // Zero facts from any sibling crate.
    for sibling in ["crates/alphabet/", "crates/beta/", "crates/alpha/vendor/"] {
        assert!(
            !observed.iter().any(|p| p.starts_with(sibling)),
            "sibling leakage from {sibling}: {observed:?}"
        );
    }
    // Recall check the other way: every `alpha` symbol in the graph is returned.
    let all = scan_fixture(&temp.path().join("repo"));
    let expected_ids: BTreeSet<String> = all
        .iter()
        .filter(|r| r["kind"] == "Symbol" && package_of(r) == Some("alpha"))
        .filter_map(|r| r["id"].as_str().map(str::to_owned))
        .collect();
    let observed_ids: BTreeSet<String> = rows(&scoped.stdout)
        .iter()
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .collect();
    assert!(!expected_ids.is_empty(), "anti-vacuity");
    assert_eq!(observed_ids, expected_ids, "recall must be 100%");
}

/// The nested crate is its own package, not part of its parent.
#[test]
fn package_scope_excludes_nested_child_package() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let inner = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "inner",
    ]);
    assert_eq!(inner.code, 0, "stderr: {}", inner.stderr);
    let paths: BTreeSet<String> = rows(&inner.stdout)
        .iter()
        .filter_map(|r| r["repo_relative_path"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from(["crates/alpha/vendor/inner/src/lib.rs".to_owned()])
    );
}

/// An unknown selector is a TYPO, reported as such with the known packages —
/// never a silent empty answer.
#[test]
fn unknown_package_selector_exits_1_with_sorted_known_packages() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let run = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "no-such-pkg",
    ]);
    assert_eq!(run.code, 1, "stdout: {} stderr: {}", run.stdout, run.stderr);
    let diagnostic: Value =
        serde_json::from_str(run.stderr.trim()).expect("diagnostic should be one JSON line");
    assert_eq!(diagnostic["code"], "unknown_package_selector");
    assert_eq!(diagnostic["selector"], "no-such-pkg");
    let known: Vec<&str> = diagnostic["known_packages"]
        .as_array()
        .expect("known_packages array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        known,
        vec!["alpha", "alphabet", "beta", "inner"],
        "known packages must be sorted and deduplicated"
    );
}

/// A KNOWN package with zero matching symbols is exit 2 — distinguishable from
/// the typo above.
#[test]
fn known_package_with_zero_matching_symbols_exits_2() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let run = run_query(&[
        "query",
        "symbol",
        "only_in_alpha",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "beta",
    ]);
    assert_eq!(
        run.code, 2,
        "a known package with no matching symbol is a no-match, not a bad selector; stderr: {}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("unknown_package_selector"),
        "must not be reported as a typo: {}",
        run.stderr
    );
}

/// Selector matching is exact: no case folding, no `-`/`_` normalization.
#[test]
fn package_selector_matches_exact_name_only() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"foo-bar\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn thing() -> u32 { 1 }\n"),
        ],
    );
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, scan_jsonl(&repo)).expect("graph written");

    for wrong in ["foo_bar", "FOO-BAR", "foo"] {
        let run = run_query(&[
            "query",
            "symbols",
            "*",
            "--graph",
            graph.to_str().unwrap(),
            "--package",
            wrong,
        ]);
        assert_eq!(
            run.code, 1,
            "`{wrong}` must not fuzzy-match `foo-bar`; stderr: {}",
            run.stderr
        );
        assert!(run.stderr.contains("unknown_package_selector"));
    }
    let right = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "foo-bar",
    ]);
    assert_eq!(right.code, 0, "stderr: {}", right.stderr);
}

/// There is no magic selector value: a package literally named `unattributed`
/// resolves to itself, and never to the unattributed records.
#[test]
fn package_named_unattributed_resolves_to_itself() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_fixture(
        &repo,
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/u\"]\n"),
            (
                "crates/u/Cargo.toml",
                "[package]\nname = \"unattributed\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/u/src/lib.rs", "pub fn owned() -> u32 { 1 }\n"),
            ("stray.rs", "pub fn orphan() -> u32 { 2 }\n"),
        ],
    );
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, scan_jsonl(&repo)).expect("graph written");

    let run = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "unattributed",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let paths: BTreeSet<String> = rows(&run.stdout)
        .iter()
        .filter_map(|r| r["repo_relative_path"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        paths,
        BTreeSet::from(["crates/u/src/lib.rs".to_owned()]),
        "`unattributed` is a package name, not a magic value"
    );
}

/// One package name owned by two repositories in a shared store is AMBIGUOUS —
/// silently merging them would make the precision claim false.
#[test]
fn package_selector_ambiguous_across_repositories_exits_1() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();
    for repo_id in ["repo-a", "repo-b"] {
        let repo = temp.path().join(repo_id);
        fs::create_dir_all(&repo).expect("repo dir");
        write_fixture(
            &repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "src/lib.rs",
                    &format!(
                        "pub fn from_{}() -> u32 {{ 1 }}\n",
                        repo_id.replace('-', "_")
                    ),
                ),
            ],
        );
        let jsonl = scan_repository_at_with_override(&repo, FIXED_TIME, Some(repo_id))
            .expect("scan")
            .to_jsonl()
            .expect("serialize");
        combined.push_str(&jsonl);
        combined.push('\n');
    }
    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    let run = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "shared",
    ]);
    assert_eq!(
        run.code, 1,
        "an ambiguous package must not silently merge repositories; stdout: {}",
        run.stdout
    );
    let diagnostic: Value =
        serde_json::from_str(run.stderr.trim()).expect("diagnostic should be one JSON line");
    assert_eq!(diagnostic["code"], "ambiguous_package_selector");
    assert_eq!(
        diagnostic["candidates"]
            .as_array()
            .expect("candidates")
            .len(),
        2
    );

    // `--repo` disambiguates it.
    let scoped = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "shared",
        "--repo",
        "repo-a",
    ]);
    assert_eq!(scoped.code, 0, "stderr: {}", scoped.stderr);
    assert_eq!(rows(&scoped.stdout).len(), 1);
}

/// `--package` and `--repo` compose as an intersection.
#[test]
fn package_and_repo_scope_compose_as_intersection() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);
    let graph = temp.path().join("graph.jsonl");
    fs::write(&graph, scan_jsonl(&repo)).expect("graph written");

    let both = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
        "--repo",
        REPO_ID,
    ]);
    assert_eq!(both.code, 0, "stderr: {}", both.stderr);
    let with_repo: BTreeSet<String> = rows(&both.stdout)
        .iter()
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .collect();

    let package_only = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    let without_repo: BTreeSet<String> = rows(&package_only.stdout)
        .iter()
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .collect();
    assert!(!with_repo.is_empty(), "anti-vacuity");
    assert_eq!(with_repo, without_repo, "single-repo graph: same set");
}

/// AC6: the attribution appears in `--format text` as well as `--format json`,
/// and the two agree.
#[test]
fn attribution_present_in_json_and_text_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let json = run_query(&[
        "query",
        "symbol",
        "only_in_alpha",
        "--graph",
        graph.to_str().unwrap(),
    ]);
    assert_eq!(json.code, 0, "stderr: {}", json.stderr);
    let json_rows = rows(&json.stdout);
    assert_eq!(json_rows[0]["crate_attribution"]["package_name"], "alpha");
    assert_eq!(
        json_rows[0]["crate_attribution"]["manifest_repo_relative_path"],
        "crates/alpha/Cargo.toml"
    );

    let text = run_query(&[
        "query",
        "symbol",
        "only_in_alpha",
        "--graph",
        graph.to_str().unwrap(),
        "--format",
        "text",
    ]);
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        text.stdout
            .contains("package: alpha (crates/alpha/Cargo.toml)"),
        "text output must carry the same package and manifest: {}",
        text.stdout
    );
}

/// An unattributed row renders its reason, in both formats.
#[test]
fn unattributed_row_renders_its_reason_in_both_formats() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let json = run_query(&[
        "query",
        "symbol",
        "generate",
        "--graph",
        graph.to_str().unwrap(),
    ]);
    assert_eq!(json.code, 0, "stderr: {}", json.stderr);
    let row = &rows(&json.stdout)[0];
    assert_eq!(row["crate_attribution"]["status"], "unattributed");
    assert_eq!(
        row["crate_attribution"]["unattributed_reason"],
        "virtual_manifest_only"
    );
    assert!(
        row["crate_attribution"].get("package_name").is_none(),
        "an unattributed row must not carry a package name"
    );

    let text = run_query(&[
        "query",
        "symbol",
        "generate",
        "--graph",
        graph.to_str().unwrap(),
        "--format",
        "text",
    ]);
    assert!(
        text.stdout
            .contains("package: (unattributed: virtual_manifest_only)"),
        "text output must state the reason: {}",
        text.stdout
    );
}

/// The epistemic caveat rides on every `--package`-scoped row.
#[test]
fn package_scoped_row_carries_containment_disclaimer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let scoped = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    for row in rows(&scoped.stdout) {
        assert_eq!(
            row["crate_attribution_disclaimer"],
            "attribution is nearest-enclosing-manifest directory containment, never proof the file is compiled into that package"
        );
    }

    // Unscoped rows carry no disclaimer: the manifest handle is self-evidencing.
    let unscoped = run_query(&["query", "symbols", "*", "--graph", graph.to_str().unwrap()]);
    for row in rows(&unscoped.stdout) {
        assert!(
            row.get("crate_attribution_disclaimer").is_none(),
            "the disclaimer rides only on a scoped assertion"
        );
    }
}

/// Scoping never reorders: the scoped result is an order-preserving subsequence
/// of the unscoped result.
#[test]
fn package_scope_preserves_unscoped_row_ordering() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let unscoped = run_query(&["query", "symbols", "*", "--graph", graph.to_str().unwrap()]);
    let scoped = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    let unscoped_ids: Vec<String> = rows(&unscoped.stdout)
        .iter()
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .collect();
    let scoped_ids: Vec<String> = rows(&scoped.stdout)
        .iter()
        .filter_map(|r| r["record_id"].as_str().map(str::to_owned))
        .collect();
    assert!(!scoped_ids.is_empty(), "anti-vacuity");

    let mut remaining = unscoped_ids.iter();
    for id in &scoped_ids {
        assert!(
            remaining.any(|candidate| candidate == id),
            "scoped rows must appear in unscoped order: {scoped_ids:?} vs {unscoped_ids:?}"
        );
    }
}

/// Five consecutive scoped queries produce byte-identical stdout, in both
/// formats.
#[test]
fn query_symbol_package_scope_stdout_identical_across_five_runs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    for format in ["json", "text"] {
        let args = [
            "query",
            "symbols",
            "*",
            "--graph",
            graph.to_str().unwrap(),
            "--package",
            "alpha",
            "--format",
            format,
        ];
        let baseline = run_query(&args);
        assert_eq!(baseline.code, 0, "stderr: {}", baseline.stderr);
        assert!(!baseline.stdout.trim().is_empty(), "anti-vacuity");
        for run in 1..5 {
            let repeat = run_query(&args);
            assert_eq!(
                repeat.stdout, baseline.stdout,
                "--format {format} diverged on run {run}"
            );
        }
    }
}

/// The sidecar index (#447) is a pure access-path optimization: a
/// `--package`-scoped answer must be byte-identical with and without one, on
/// both the success and the unknown-selector paths.
#[test]
fn package_scope_answers_identically_with_and_without_sidecar_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());
    let graph_str = graph.to_str().unwrap();

    let hit_cold = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph_str,
        "--package",
        "alpha",
    ]);
    let miss_cold = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph_str,
        "--package",
        "nope",
    ]);

    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["index", graph_str])
        .assert()
        .success();
    assert!(
        graph.with_extension("jsonl.idx").exists()
            || Path::new(&format!("{graph_str}.idx")).exists(),
        "the sidecar index must have been built"
    );

    let hit_indexed = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph_str,
        "--package",
        "alpha",
    ]);
    let miss_indexed = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph_str,
        "--package",
        "nope",
    ]);

    assert_eq!(
        hit_cold.stdout, hit_indexed.stdout,
        "indexed answer diverged"
    );
    assert_eq!(hit_cold.code, hit_indexed.code);
    assert_eq!(
        miss_cold.stderr, miss_indexed.stderr,
        "the unknown-selector diagnostic must list the same known packages with and without an index"
    );
    assert_eq!(miss_cold.code, miss_indexed.code);
}

/// The daemon lane does not carry the field, so the flag is REFUSED there
/// rather than silently ignored while returning unscoped rows.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn package_scope_with_daemon_exits_1_unsupported_combination() {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    fs::create_dir_all(&data_dir).expect("store dir");

    let run = run_query(&[
        "query",
        "symbol",
        "handle",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--daemon",
        "--package",
        "alpha",
    ]);
    assert_eq!(run.code, 1, "stdout: {} stderr: {}", run.stdout, run.stderr);
    assert!(
        run.stderr.contains("unsupported_combination"),
        "the flag must be refused, never silently ignored: {}",
        run.stderr
    );
}

/// A legacy record (produced before issue #117) omits the key entirely — never
/// `null`, and never a rendered "unattributed" line that would fabricate a
/// negative fact.
#[test]
fn legacy_record_omits_the_key_and_prints_no_package_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());
    // Strip the field from every record, simulating a pre-#117 producer.
    let stripped: String = fs::read_to_string(&graph)
        .expect("graph readable")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("JSON");
            if let Some(object) = record.as_object_mut() {
                object.remove("crate_attribution");
            }
            serde_json::to_string(&record).expect("serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let legacy = temp.path().join("legacy.jsonl");
    fs::write(&legacy, stripped).expect("legacy graph written");

    let json = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        legacy.to_str().unwrap(),
    ]);
    assert_eq!(json.code, 0, "stderr: {}", json.stderr);
    for row in rows(&json.stdout) {
        assert!(
            row.get("crate_attribution").is_none(),
            "a legacy row must OMIT the key, never emit null: {row}"
        );
    }

    let text = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        legacy.to_str().unwrap(),
        "--format",
        "text",
    ]);
    assert!(
        !text.stdout.contains("package:"),
        "absent attribution must print nothing — printing `unattributed` would fabricate a negative fact: {}",
        text.stdout
    );
}

/// No manifest body text reaches any query surface.
#[test]
fn no_manifest_body_text_in_query_output() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());
    for args in [
        vec!["query", "symbols", "*", "--graph", graph.to_str().unwrap()],
        vec![
            "query",
            "symbols",
            "*",
            "--graph",
            graph.to_str().unwrap(),
            "--package",
            "alpha",
        ],
        vec![
            "query",
            "symbols",
            "*",
            "--graph",
            graph.to_str().unwrap(),
            "--format",
            "text",
        ],
    ] {
        let run = run_query(&args);
        assert!(
            !run.stdout.contains(SENTINEL) && !run.stderr.contains(SENTINEL),
            "manifest body text leaked into `{args:?}`"
        );
    }
}

/// A path containing a `"` must be indexed and attributed by history replay,
/// exactly as `eg scan` indexes it.
///
/// `core.quotePath=false` suppresses octal-escaping of NON-ASCII bytes only —
/// `git ls-tree` still C-quotes any path containing `"`, `\`, or a control
/// character. The working-tree walk uses `git ls-files -z`, whose NUL-delimited
/// output is never quoted, so `eg scan` sees the real path. Unless the replay
/// also reads NUL-delimited output, the two paths disagree: the file (and its
/// crate's manifest) silently vanish from history.
#[test]
fn history_indexes_and_attributes_quote_bearing_paths() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "we\"ird/Cargo.toml",
                "[package]\nname = \"quoted-pkg\"\nversion = \"0.1.0\"\n",
            ),
            ("we\"ird/src/lib.rs", "pub fn quoted() -> u32 { 1 }\n"),
            (
                "plain/Cargo.toml",
                "[package]\nname = \"plain-pkg\"\nversion = \"0.1.0\"\n",
            ),
            ("plain/src/lib.rs", "pub fn plain() -> u32 { 2 }\n"),
        ],
    );
    let sha = commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let by_commit = history_attribution(&repo);
    let at_head = &by_commit[&sha];
    assert_eq!(
        at_head.get("we\"ird/src/lib.rs"),
        Some(&BTreeSet::from(
            ["quoted-pkg@we\"ird/Cargo.toml".to_owned()]
        )),
        "a quote-bearing path must be indexed and attributed; got {at_head:?}"
    );
    // Anti-vacuity: the plain sibling works either way.
    assert_eq!(
        at_head.get("plain/src/lib.rs"),
        Some(&BTreeSet::from(["plain-pkg@plain/Cargo.toml".to_owned()]))
    );

    // And `eg scan` and `eg scan-history` agree over the shared source paths.
    let scanned = attribution_by_path(&scan_fixture(&repo));
    for (path, attribution) in at_head {
        if let Some(scanned_attribution) = scanned.get(path) {
            assert_eq!(
                attribution, scanned_attribution,
                "scan and history disagree on {path}"
            );
        }
    }
    assert!(
        scanned.contains_key("we\"ird/src/lib.rs"),
        "precondition: eg scan indexes the quote-bearing path"
    );
}

// ── acceptance-criterion gap closers ─────────────────────────────────────────

/// AC1 names three node kinds by hand. Prove all three are present in the
/// fixture graph AND attributed — a suite that only exercised `Symbol` would
/// leave two thirds of the criterion unproven.
#[test]
fn file_module_and_symbol_are_all_attributed() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    for kind in ["File", "Module", "Symbol"] {
        let of_kind: Vec<&Value> = records
            .iter()
            .filter(|r| r["record_type"] == "node" && r["kind"] == kind)
            .collect();
        assert!(
            !of_kind.is_empty(),
            "the fixture must produce at least one {kind} node"
        );
        for record in of_kind {
            let attribution = record.get("crate_attribution").unwrap_or_else(|| {
                panic!(
                    "{kind} node {} carries no attribution",
                    record["repo_relative_path"]
                )
            });
            // Both AC1 halves: the package NAME and the owning manifest's
            // repo-relative path, or an explicit unattributed reason.
            if attribution["status"] == "attributed" {
                assert!(attribution["package_name"].is_string(), "{kind}: {record}");
                assert!(
                    attribution["manifest_repo_relative_path"]
                        .as_str()
                        .is_some_and(|p| p.ends_with("Cargo.toml")),
                    "{kind}: {record}"
                );
            } else {
                assert!(
                    attribution["unattributed_reason"].is_string(),
                    "{kind}: {record}"
                );
            }
        }
    }
}

/// AC5: attribution is derived locally with no `cargo` invocation.
///
/// Runs a real `eg scan` subprocess whose `PATH` contains ONLY `git` (which the
/// scanner genuinely needs for repository identity and the tracked-file walk).
/// If any code path shelled out to `cargo build` / `cargo check` /
/// `cargo metadata`, the scan would fail or silently degrade; instead it must
/// succeed and produce full attribution.
#[test]
fn attribution_needs_no_cargo_on_path() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);
    init_git(&repo);
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    // A PATH holding a symlink to `git` and nothing else.
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let git_path = which_git();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&git_path, bin.join("git")).expect("git symlink");
    #[cfg(not(unix))]
    fs::copy(&git_path, bin.join("git.exe")).expect("git copy");

    let out = temp.path().join("graph.jsonl");
    let output = assert_cmd::Command::cargo_bin("egregore")
        .expect("binary should run")
        .env("PATH", &bin)
        .args(["scan"])
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .output()
        .expect("scan should execute");
    assert!(
        output.status.success(),
        "scan must succeed with no cargo on PATH: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let jsonl = fs::read_to_string(&out).expect("graph written");
    let records = parse_jsonl(&jsonl);
    let attributed = code_fact_nodes(&records)
        .iter()
        .filter(|r| package_of(r).is_some())
        .count();
    assert!(
        attributed > 0,
        "attribution must be fully derived without cargo; got none"
    );
    assert_eq!(
        attribution_by_path(&records).get("crates/alpha/src/lib.rs"),
        Some(&BTreeSet::from(
            ["alpha@crates/alpha/Cargo.toml".to_owned()]
        ))
    );
}

fn which_git() -> std::path::PathBuf {
    let output = Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg("git")
        .output()
        .expect("locating git should work");
    std::path::PathBuf::from(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .expect("git must be on PATH for this test")
            .trim(),
    )
}

/// AC7: the slice reuses the existing code-graph domain and vocabulary — no new
/// graph domain, edge label, or trust class rides along with the new field.
#[test]
fn no_new_domain_edge_label_or_trust_class() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_workspace_fixture(temp.path());
    let records = scan_fixture(temp.path());

    // Every attributed record stays in the code-graph domain: `domain` is either
    // absent (the legacy-inferred codegraph default) or literally "codegraph".
    for record in code_fact_nodes(&records) {
        match record.get("domain").and_then(Value::as_str) {
            None | Some("codegraph") => {}
            Some(other) => panic!("attribution leaked into domain `{other}`: {record}"),
        }
    }

    // The edge vocabulary this fixture produces is exactly the pre-existing
    // code-graph containment/definition set — attribution mints no edge.
    let labels: BTreeSet<&str> = records
        .iter()
        .filter(|r| r["record_type"] == "edge")
        .filter_map(|r| r["label"].as_str())
        .collect();
    // Both labels predate this slice; attribution mints no edge of its own.
    assert_eq!(
        labels,
        BTreeSet::from(["CONTAINS", "DEFINES"]),
        "attribution must mint no new edge label"
    );

    // No trust class is introduced: attribution is a field on a source-derived
    // record, not a new trust vocabulary entry.
    assert!(
        !records
            .iter()
            .any(|r| r.get("trust").is_some() || r.get("trust_class").is_some()),
        "scan output must carry no trust field"
    );
}

/// AC7: the field is additive. A pre-#117 record with no `crate_attribution`
/// key must still deserialize, and a record whose attribution is `None` must
/// serialize WITHOUT the key — never as `null`, which a reader could mistake
/// for a computed "no owner".
#[test]
fn legacy_node_line_roundtrips_with_absent_attribution() {
    use aletheia_egregore::{GraphRecord, NodeKind};

    let record = GraphRecord::node(
        "codegraph:v9:deadbeef".to_owned(),
        NodeKind::Symbol,
        Some("src/lib.rs".to_owned()),
        None,
        Some("thing".to_owned()),
        "Symbol thing".to_owned(),
    );
    let line = serde_json::to_string(&record).expect("serialize");
    assert!(
        !line.contains("crate_attribution"),
        "an unattributed-by-absence record must omit the key entirely: {line}"
    );

    let parsed: GraphRecord = serde_json::from_str(&line).expect("legacy line must deserialize");
    assert!(
        parsed.crate_attribution().is_none(),
        "an absent key must read back as None (attribution UNKNOWN)"
    );

    // And a v8-era line that never had the key still parses.
    let legacy = line.replace("\"schema_version\":9", "\"schema_version\":8");
    let parsed: GraphRecord = serde_json::from_str(&legacy).expect("v8 line must deserialize");
    assert!(parsed.crate_attribution().is_none());
}

/// AC8: a `--package`-scoped query over an embedded store is strictly
/// read-only — it takes no write lease and leaves the store byte-identical.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn package_scoped_query_does_not_mutate_the_store() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);

    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(&repo)).expect("graph written");
    let data_dir = temp.path().join("store");
    ingest_embedded(&graph_path, &data_dir);

    // The embedded store rewrites its own index files whenever it is OPENED,
    // including for a plain read — pre-existing behavior, unrelated to this
    // slice. What must hold is that `--package` writes NOTHING BEYOND that: the
    // set of files, and every non-index file's bytes, are unchanged, and the
    // scoped read touches exactly the same files an unscoped read does.
    let unscoped_before = snapshot_tree(&data_dir);
    assert!(
        !unscoped_before.is_empty(),
        "anti-vacuity: the store must have files"
    );
    let unscoped = run_query(&[
        "query",
        "symbols",
        "*",
        "--data-dir",
        data_dir.to_str().unwrap(),
    ]);
    assert_eq!(unscoped.code, 0, "stderr: {}", unscoped.stderr);
    let after_unscoped = snapshot_tree(&data_dir);
    let touched_by_unscoped: BTreeSet<&String> = after_unscoped
        .iter()
        .filter(|(path, bytes)| unscoped_before.get(*path) != Some(*bytes))
        .map(|(path, _)| path)
        .collect();

    let run = run_query(&[
        "query",
        "symbols",
        "*",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(!rows(&run.stdout).is_empty(), "anti-vacuity: rows returned");
    let after_scoped = snapshot_tree(&data_dir);

    assert_eq!(
        after_unscoped.keys().collect::<BTreeSet<_>>(),
        after_scoped.keys().collect::<BTreeSet<_>>(),
        "a scoped query must create or remove no store file"
    );
    let touched_by_scoped: BTreeSet<&String> = after_scoped
        .iter()
        .filter(|(path, bytes)| after_unscoped.get(*path) != Some(*bytes))
        .map(|(path, _)| path)
        .collect();
    assert!(
        touched_by_scoped.is_subset(&touched_by_unscoped),
        "package scoping must write nothing an unscoped read does not: {touched_by_scoped:?} vs {touched_by_unscoped:?}"
    );
}

/// `--graph` and `--data-dir` must answer a scoped query identically.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn graph_and_data_dir_agree_under_package_scope() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_workspace_fixture(&repo);

    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(&repo)).expect("graph written");
    let data_dir = temp.path().join("store");
    ingest_embedded(&graph_path, &data_dir);

    let from_graph = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph_path.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    let from_store = run_query(&[
        "query",
        "symbols",
        "*",
        "--data-dir",
        data_dir.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    assert_eq!(from_graph.code, 0, "stderr: {}", from_graph.stderr);
    assert!(!rows(&from_graph.stdout).is_empty(), "anti-vacuity");
    assert_eq!(
        from_graph.stdout, from_store.stdout,
        "--graph and --data-dir must produce byte-identical scoped answers"
    );
}

/// The epistemic caveat must be the SAME SENTENCE everywhere it is asserted.
///
/// It is stated in five places — the `CrateAttribution` doc comment, the
/// resolver module docs, `docs/cli/crate-attribution.md`, `CLAUDE.md`, and the
/// `crate_attribution_disclaimer` value on every scoped row. Prose copies drift;
/// this pins them. Comparison normalizes line wrapping and the sentence-initial
/// capital, because each site wraps to its own column and two of them open a
/// sentence — nothing else may differ.
#[test]
fn containment_disclaimer_is_identical_everywhere_it_is_stated() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The canonical wording, taken from the value the CLI actually emits.
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());
    let scoped = run_query(&[
        "query",
        "symbols",
        "*",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    let canonical = rows(&scoped.stdout)
        .first()
        .and_then(|row| {
            row["crate_attribution_disclaimer"]
                .as_str()
                .map(str::to_owned)
        })
        .expect("a scoped row must carry the disclaimer");

    // Collapse line wrapping, and compare case-insensitively so the two sites
    // that open a sentence with "Attribution" still match the lowercase value
    // the CLI emits mid-sentence. Nothing else may differ.
    let normalize = |text: &str| {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let needle = normalize(&canonical);

    for relative in [
        "src/ir.rs",
        "src/crate_attribution.rs",
        "docs/cli/crate-attribution.md",
        "CLAUDE.md",
    ] {
        let body = fs::read_to_string(repo_root.join(relative))
            .unwrap_or_else(|_| panic!("{relative} should be readable"));
        // Strip comment/quote markers so a wrapped `///`, `//!`, or `>` prefix
        // does not break the match, then collapse whitespace.
        let stripped: String = body
            .lines()
            .map(|line| {
                line.trim_start()
                    .trim_start_matches("//!")
                    .trim_start_matches("///")
                    .trim_start_matches('>')
            })
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            normalize(&stripped).contains(&needle),
            "{relative} does not state the disclaimer verbatim; expected: {needle}"
        );
    }
}

/// `--package` must never be silently ignored on the transaction-time lane.
///
/// `--tx-as-of` returns through its own row type, which carries no crate
/// attribution, so a scoped transaction-time query could only emit UNSCOPED
/// rows — including records owned by other packages. Refusing is the honest
/// outcome, matching the `--daemon` refusal.
#[test]
fn package_scope_with_tx_as_of_exits_1_unsupported_combination() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let run = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph.to_str().unwrap(),
        "--tx-as-of",
        "2026-06-14T00:00:00Z",
        "--package",
        "alpha",
    ]);
    assert_eq!(
        run.code, 1,
        "the flag must be refused, never silently ignored; stdout: {}",
        run.stdout
    );
    let combined = format!("{}{}", run.stdout, run.stderr);
    assert!(
        combined.contains("unsupported_combination"),
        "expected an unsupported_combination envelope; got stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    // And no unscoped rows leaked to stdout before the refusal.
    assert!(
        !run.stdout.contains("\"record_id\""),
        "no rows may be emitted alongside the refusal: {}",
        run.stdout
    );
}

/// Cross-repository package ambiguity must be decided against the WHOLE
/// corpus, not the sidecar index's narrowed symbol closure.
///
/// The #447 index is a pure access-path optimization: an answer must be
/// byte-identical with and without one. A package owned by two repositories
/// where only ONE of them defines the queried symbol is the case that breaks
/// that: the narrowed closure sees a single owner and would happily return
/// rows, while a cold scan reports `ambiguous_package_selector`.
#[test]
fn package_ambiguity_is_detected_even_with_a_sidecar_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();
    for (repo_id, symbol) in [("repo-a", "only_in_a"), ("repo-b", "only_in_b")] {
        let repo = temp.path().join(repo_id);
        fs::create_dir_all(&repo).expect("repo dir");
        write_fixture(
            &repo,
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n",
                ),
                ("src/lib.rs", &format!("pub fn {symbol}() -> u32 {{ 1 }}\n")),
            ],
        );
        combined.push_str(
            &scan_repository_at_with_override(&repo, FIXED_TIME, Some(repo_id))
                .expect("scan")
                .to_jsonl()
                .expect("serialize"),
        );
        combined.push('\n');
    }
    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");
    let graph_str = graph.to_str().unwrap();

    // `only_in_a` is defined by repo-a alone, but BOTH repositories own a
    // package called `shared`.
    let args = [
        "query",
        "symbol",
        "only_in_a",
        "--graph",
        graph_str,
        "--package",
        "shared",
    ];
    let cold = run_query(&args);
    assert_eq!(
        cold.code, 1,
        "cold: an ambiguous package must be refused; stdout: {}",
        cold.stdout
    );
    assert!(cold.stderr.contains("ambiguous_package_selector"));

    assert_cmd::Command::cargo_bin("egregore")
        .expect("binary")
        .args(["index", graph_str])
        .assert()
        .success();

    let indexed = run_query(&args);
    assert_eq!(
        indexed.code, cold.code,
        "the sidecar index must not change the verdict; stdout: {}",
        indexed.stdout
    );
    assert_eq!(
        indexed.stderr, cold.stderr,
        "the sidecar index must not change the diagnostic"
    );
    assert_eq!(indexed.stdout, cold.stdout);
}

/// History replay must not abort on a path whose bytes are not valid UTF-8.
///
/// Git stores paths as raw bytes. Reading NUL-delimited listings hands those
/// bytes through unescaped, so an undecodable path must be SKIPPED with the
/// rest of the tree still replayed — the issue-#438 precedent for an
/// undecodable blob — never turned into a hard failure of the whole command.
/// `eg scan` copes with such a repo, so `eg scan-history` must too.
#[cfg(unix)]
#[test]
fn history_skips_non_utf8_paths_without_aborting() {
    use std::os::unix::ffi::OsStrExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"ok\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn fine() -> u32 { 1 }\n"),
        ],
    );
    // A Latin-1 `é` (0xE9) — a legal Git path, not valid UTF-8.
    let bad = repo
        .join("src")
        .join(std::ffi::OsStr::from_bytes(b"caf\xe9.rs"));
    fs::write(&bad, "pub fn latin() -> u32 { 2 }\n").expect("latin-1 path written");
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let graph = aletheia_egregore::scan_repository_history(&repo)
        .expect("history replay must not abort on a non-UTF-8 path");
    let jsonl = graph.to_jsonl().expect("serialize");
    let records = parse_jsonl(&jsonl);

    // The decodable sibling is still fully indexed and attributed.
    let by_path = attribution_by_path(&records);
    assert_eq!(
        by_path.get("src/lib.rs"),
        Some(&BTreeSet::from(["ok@Cargo.toml".to_owned()])),
        "the decodable file must still replay"
    );
}

/// Every `Change` node's path must be spelled the same way the `File` node for
/// that path is, so history joins actually connect.
///
/// The tree listing and the change listing are two different Git commands. If
/// only one reads NUL-delimited output, a path containing `"` or a tab is raw
/// on one side and C-quoted on the other: the file is indexed, but its
/// `CHANGED_IN` edges point at a path no `File` node has, silently dropping it
/// from every history lane (`churn`, `coupling`, `deltas`, `lifeline`).
#[test]
fn change_and_file_node_paths_agree_for_special_characters() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"ok\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn plain() -> u32 { 1 }\n"),
            ("src/qu\"ote.rs", "pub fn quoted() -> u32 { 2 }\n"),
            ("src/with space.rs", "pub fn spaced() -> u32 { 3 }\n"),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let jsonl = aletheia_egregore::scan_repository_history(&repo)
        .expect("history replay")
        .to_jsonl()
        .expect("serialize");
    let records = parse_jsonl(&jsonl);

    let file_paths: BTreeSet<&str> = records
        .iter()
        .filter(|r| r["kind"] == "File")
        .filter_map(|r| r["repo_relative_path"].as_str())
        .collect();
    let change_paths: BTreeSet<&str> = records
        .iter()
        .filter(|r| r["kind"] == "Change")
        .filter_map(|r| r["repo_relative_path"].as_str())
        .collect();

    for special in ["src/qu\"ote.rs", "src/with space.rs", "src/lib.rs"] {
        assert!(
            file_paths.contains(special),
            "{special} must be indexed; got {file_paths:?}"
        );
        assert!(
            change_paths.contains(special),
            "{special} must have a Change node under the SAME spelling; got {change_paths:?}"
        );
    }
    // No Change node may name a path that no File node has.
    let orphans: Vec<&&str> = change_paths
        .iter()
        .filter(|p| {
            Path::new(*p).extension().and_then(|e| e.to_str()) == Some("rs")
                && !file_paths.contains(*p)
        })
        .collect();
    assert!(
        orphans.is_empty(),
        "Change nodes name paths no File node has: {orphans:?}"
    );
}

/// A record read back from a store or graph is operator-controlled, so the
/// `attributed <=> name present` invariant must be re-checked on the READ path,
/// not assumed from the write path.
///
/// A forged record claiming `status: unattributed` while carrying a
/// `package_name` must not render as attributed, must not be matched by
/// `--package`, and must not appear in `known_packages`. Trusting the name over
/// the status would let a crafted graph assert an ownership fact the resolver
/// never produced.
#[test]
fn forged_attribution_with_inconsistent_status_is_not_trusted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let forged: String = fs::read_to_string(&graph)
        .expect("graph readable")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("JSON");
            if record["kind"] == "Symbol"
                && record["name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with("handle"))
                && let Some(object) = record.as_object_mut()
            {
                object.insert(
                    "crate_attribution".to_owned(),
                    serde_json::json!({
                        "status": "unattributed",
                        "unattributed_reason": "no_enclosing_manifest",
                        "package_name": "ghostpkg",
                        "manifest_repo_relative_path": "nowhere/Cargo.toml",
                    }),
                );
            }
            serde_json::to_string(&record).expect("serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let forged_path = temp.path().join("forged.jsonl");
    fs::write(&forged_path, forged).expect("forged graph written");
    let forged_str = forged_path.to_str().unwrap();

    // The forged package is not a real package: scoping to it is a typo, not a
    // match, and it never appears in `known_packages`.
    let scoped = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        forged_str,
        "--package",
        "ghostpkg",
    ]);
    assert_eq!(
        scoped.code, 1,
        "a forged name must not become a scopable package; stdout: {}",
        scoped.stdout
    );
    assert!(scoped.stderr.contains("unknown_package_selector"));
    assert!(
        !scoped.stderr.contains("ghostpkg\",\"known") && !scoped.stderr.contains("\"ghostpkg\"]"),
        "the forged name must not be listed as known: {}",
        scoped.stderr
    );

    // And the row renders NOTHING — neither the ownership claim nor the
    // `unattributed` one.
    //
    // An earlier round of this test asserted the row still rendered its
    // DECLARED status, on the reading that falling back to the negative claim
    // was the conservative move. It is not. "Provably no owning package" is
    // itself a FACT, and this record's shape — a status of `unattributed`
    // carrying a package name and a manifest path — is one no producer writes,
    // so it is no better evidence for the negative claim than for the positive
    // one. Rendering it would manufacture the proven-ownerless fact the
    // absent-vs-unattributed contract exists to keep separate from an unknown.
    // Printing nothing is exactly how an ABSENT attribution reads: unknown.
    // See `forged_unattributed_reason_the_resolver_could_not_produce_renders_nothing`.
    let text = run_query(&[
        "query", "symbol", "handle", "--graph", forged_str, "--format", "text",
    ]);
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        !text.stdout.contains("ghostpkg"),
        "an inconsistent row must not render a package name: {}",
        text.stdout
    );
    assert!(
        !text.stdout.contains("package:"),
        "an inconsistent row proves nothing and must render no package line: {}",
        text.stdout
    );
}

/// A symlinked `Cargo.toml` must not be read as manifest TEXT.
///
/// A Git symlink is stored as a blob whose content is the link target, so a
/// type-only filter reads `../real/Cargo.toml` as TOML, fails to parse it, and
/// reports `unparseable_manifest` — a positive, operator-actionable claim about
/// a manifest that is perfectly valid. It also stops the walk fail-closed, so
/// the file loses the real attribution `eg scan` gives it.
#[cfg(unix)]
#[test]
fn symlinked_manifest_is_not_parsed_as_manifest_text() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("linked/src")).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"rootpkg\"\nversion = \"0.1.0\"\n",
            ),
            ("linked/src/x.rs", "pub fn linked() -> u32 { 1 }\n"),
        ],
    );
    std::os::unix::fs::symlink("../Cargo.toml", repo.join("linked/Cargo.toml"))
        .expect("symlink created");
    let sha = commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let scanned = attribution_by_path(&scan_fixture(&repo));
    let history = history_attribution(&repo);
    let at_head = &history[&sha];

    assert_eq!(
        at_head.get("linked/src/x.rs"),
        scanned.get("linked/src/x.rs"),
        "scan and history must agree; a symlinked manifest is invisible to both"
    );
    assert!(
        !at_head["linked/src/x.rs"]
            .iter()
            .any(|a| a.contains("unparseable")),
        "a valid manifest must never be reported unparseable: {:?}",
        at_head["linked/src/x.rs"]
    );
}

/// `--package` must select among the candidates at a commit, not filter a
/// winner already chosen without it.
///
/// With two same-named symbols in two crates, `--at`/`--as-of` pick one record
/// first and then apply the package filter, so asking for the crate whose
/// symbol did not happen to sort first yields a false "no match" for a symbol
/// that demonstrably exists at that commit.
#[test]
fn package_scope_selects_among_candidates_at_a_commit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_workspace_fixture(&repo);
    let sha = commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");
    let graph = graph_path.to_str().unwrap();

    for package in ["alpha", "beta"] {
        let run = run_query(&[
            "query",
            "symbol",
            "handle",
            "--graph",
            graph,
            "--at",
            &sha,
            "--package",
            package,
        ]);
        assert_eq!(
            run.code, 0,
            "`handle` exists in `{package}` at {sha}; stderr: {}",
            run.stderr
        );
        let rows = rows(&run.stdout);
        assert_eq!(rows.len(), 1, "one row per package");
        assert_eq!(rows[0]["crate_attribution"]["package_name"], package);
    }
}

/// `Change` nodes carry a repo-relative path, so they must be attributed like
/// every other path-bearing code-graph node.
///
/// The rule the whole design rests on is "a path-bearing code-graph node
/// carries attribution". A `Change` that carries a path and no attribution
/// would make that rule false, and with it the inference that an absent field
/// means the record predates issue #117.
#[test]
fn change_nodes_are_attributed_like_every_other_path_bearing_node() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_workspace_fixture(&repo);
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let jsonl = aletheia_egregore::scan_repository_history(&repo)
        .expect("history replay")
        .to_jsonl()
        .expect("serialize");
    let records = parse_jsonl(&jsonl);

    let changes: Vec<&Value> = records.iter().filter(|r| r["kind"] == "Change").collect();
    assert!(
        !changes.is_empty(),
        "anti-vacuity: the commit changed files"
    );
    for change in &changes {
        assert!(
            change["repo_relative_path"].is_string(),
            "precondition: Change nodes carry a path"
        );
        assert!(
            change.get("crate_attribution").is_some(),
            "a path-bearing Change node must be attributed: {change}"
        );
    }
    // And the attribution matches the File node for the same path.
    let by_path = attribution_by_path(&records);
    let alpha_change = changes
        .iter()
        .find(|c| c["repo_relative_path"] == "crates/alpha/src/lib.rs")
        .expect("the alpha source changed in the seed commit");
    assert_eq!(
        attribution_of(alpha_change).as_deref(),
        Some("alpha@crates/alpha/Cargo.toml"),
        "a Change must agree with the File node for its path: {by_path:?}"
    );
}

/// The `--as-of` lane narrows candidates by package too.
#[test]
fn package_scope_selects_among_candidates_as_of_an_instant() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_workspace_fixture(&repo);
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");
    let graph = graph_path.to_str().unwrap();

    for package in ["alpha", "beta"] {
        let run = run_query(&[
            "query",
            "symbol",
            "handle",
            "--graph",
            graph,
            "--as-of",
            "2026-12-01T00:00:00Z",
            "--package",
            package,
        ]);
        assert_eq!(
            run.code, 0,
            "`handle` exists in `{package}`; stderr: {}",
            run.stderr
        );
        let rows = rows(&run.stdout);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["crate_attribution"]["package_name"], package);
    }
}

/// A corpus carrying NO attribution at all must say so, not report a typo.
///
/// `unknown_package_selector` with an empty `known_packages` is the same shape
/// a genuine typo produces, so an operator querying a pre-#117 store cannot
/// tell "you misspelled it" from "this store predates the feature" — collapsing
/// the absent-vs-unattributed distinction at the one surface where they would
/// act on it. The remedy differs completely: fix the spelling, versus re-scan.
#[test]
fn corpus_with_no_attribution_reports_unavailable_not_a_typo() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());
    let stripped: String = fs::read_to_string(&graph)
        .expect("graph readable")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("JSON");
            if let Some(object) = record.as_object_mut() {
                object.remove("crate_attribution");
            }
            serde_json::to_string(&record).expect("serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let legacy = temp.path().join("legacy.jsonl");
    fs::write(&legacy, stripped).expect("legacy graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        legacy.to_str().unwrap(),
        "--package",
        "alpha",
    ]);
    assert_eq!(run.code, 1, "stdout: {}", run.stdout);
    let diagnostic: Value =
        serde_json::from_str(run.stderr.trim()).expect("one JSON diagnostic line");
    assert_eq!(
        diagnostic["code"], "crate_attribution_unavailable",
        "a corpus with no attribution must be reported as such, not as a typo: {}",
        run.stderr
    );
    assert!(
        diagnostic["remedy"]
            .as_str()
            .is_some_and(|r| r.contains("re-scan") || r.contains("rescan")),
        "the diagnostic must name the remedy: {}",
        run.stderr
    );

    // Anti-vacuity: an attributed corpus still reports a real typo as a typo.
    let typo = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "nope",
    ]);
    assert_eq!(typo.code, 1);
    assert!(typo.stderr.contains("unknown_package_selector"));
}

/// The mirror of the forged-unattributed case: a record claiming
/// `status: attributed` while ALSO carrying an `unattributed_reason` is
/// internally contradictory and must own nothing.
///
/// The type contract is that the reason is present only for unattributed
/// values, so a value carrying both is not a stricter attribution — it is a
/// record no producer could have written. Fail-closed means rejecting it rather
/// than picking the half that looks like an ownership fact.
#[test]
fn forged_attributed_value_carrying_a_reason_is_not_trusted() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let forged: String = fs::read_to_string(&graph)
        .expect("graph readable")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("JSON");
            if record["kind"] == "Symbol"
                && record["name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with("handle"))
                && let Some(object) = record.as_object_mut()
            {
                object.insert(
                    "crate_attribution".to_owned(),
                    serde_json::json!({
                        "status": "attributed",
                        "package_name": "contradictory",
                        "manifest_repo_relative_path": "nowhere/Cargo.toml",
                        "unattributed_reason": "no_enclosing_manifest",
                    }),
                );
            }
            serde_json::to_string(&record).expect("serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let forged_path = temp.path().join("contradictory.jsonl");
    fs::write(&forged_path, forged).expect("forged graph written");
    let forged_str = forged_path.to_str().unwrap();

    let scoped = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        forged_str,
        "--package",
        "contradictory",
    ]);
    assert_eq!(
        scoped.code, 1,
        "a contradictory value must not become a scopable package; stdout: {}",
        scoped.stdout
    );
    // The selector is legitimately echoed back; what must not appear is the
    // name in `known_packages`.
    let diagnostic: Value =
        serde_json::from_str(scoped.stderr.trim()).expect("one JSON diagnostic line");
    assert_eq!(diagnostic["code"], "unknown_package_selector");
    let known: Vec<&str> = diagnostic["known_packages"]
        .as_array()
        .expect("known_packages array")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !known.contains(&"contradictory"),
        "the contradictory name must not be listed as known: {known:?}"
    );

    let text = run_query(&[
        "query", "symbol", "handle", "--graph", forged_str, "--format", "text",
    ]);
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        !text.stdout.contains("contradictory"),
        "a contradictory value must not render as an ownership claim: {}",
        text.stdout
    );
}

/// A `Cargo.toml` carrying neither `[package]` nor `[workspace]` is a manifest
/// Cargo REJECTS, so the walk must stop there rather than treat it as a virtual
/// workspace root and attribute the subtree to an outer package.
///
/// Cargo's own errors: "manifest is missing either a `[package]` or a
/// `[workspace]`", and for a package-less manifest with dependencies, "this
/// virtual manifest specifies a `dependencies` section, which is not allowed".
/// Walking past such a boundary would attribute files across a manifest Cargo
/// will not load — the fabrication this slice exists to prevent.
#[test]
fn manifest_with_neither_package_nor_workspace_stops_the_walk() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn at_root() -> u32 { 1 }\n"),
            // Rejected by Cargo: dependencies with no [package] and no [workspace].
            ("nested/Cargo.toml", "[dependencies]\nserde = \"1\"\n"),
            ("nested/src/lib.rs", "pub fn nested() -> u32 { 2 }\n"),
            // Also rejected: neither table at all.
            ("bare/Cargo.toml", "[profile.release]\nopt-level = 3\n"),
            ("bare/src/lib.rs", "pub fn bare() -> u32 { 3 }\n"),
        ],
    );
    let records = scan_fixture(temp.path());
    let by_path = attribution_by_path(&records);

    for path in ["nested/src/lib.rs", "bare/src/lib.rs"] {
        let attribution = by_path
            .get(path)
            .unwrap_or_else(|| panic!("{path} must be indexed; got {by_path:#?}"));
        assert_eq!(
            attribution.iter().cloned().collect::<Vec<_>>(),
            vec!["unattributed:unusable_manifest".to_owned()],
            "{path} must fail closed at the rejected manifest, never inherit `outer`"
        );
    }
    // Anti-vacuity: the healthy root still attributes.
    assert_eq!(
        by_path.get("src/lib.rs"),
        Some(&BTreeSet::from(["outer@Cargo.toml".to_owned()]))
    );
}

/// A genuine virtual workspace root is still walked past.
#[test]
fn real_virtual_workspace_root_is_still_walked_past() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
            ),
            // `[workspace]` with no `[package]`: Cargo accepts this.
            ("group/Cargo.toml", "[workspace]\nmembers = [\"m\"]\n"),
            (
                "group/m/Cargo.toml",
                "[package]\nname = \"m\"\nversion = \"0.1.0\"\n",
            ),
            ("group/m/src/lib.rs", "pub fn member() -> u32 { 1 }\n"),
            ("group/loose.rs", "pub fn loose() -> u32 { 2 }\n"),
        ],
    );
    let by_path = attribution_by_path(&scan_fixture(temp.path()));

    assert_eq!(
        by_path.get("group/m/src/lib.rs"),
        Some(&BTreeSet::from(["m@group/m/Cargo.toml".to_owned()]))
    );
    // Directly under the virtual root: walked past, so the outer package owns it.
    assert_eq!(
        by_path.get("group/loose.rs"),
        Some(&BTreeSet::from(["outer@Cargo.toml".to_owned()])),
        "a real [workspace] root declares no package and must not stop the walk"
    );
}

/// `eg query file` carries the owning package on its FIRST row.
///
/// The lane has no envelope, so omitting attribution entirely would drop the
/// owning-package fact from the whole answer — not merely deduplicate it, the
/// way omitting the per-row declaration-surface fields does. But every row of a
/// file listing shares one package, so repeating it per row is pure duplication
/// that measurably regresses the `eg audit token-cost` savings gate (the
/// `file_defines` class falls from 3.16 to 2.92 against a 3.0 floor). Carrying
/// it once, on the first row, is the same shape the lane already uses for
/// file-level `diagnostics`.
#[test]
fn query_file_carries_attribution_once_on_the_first_row() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let run = run_query(&[
        "query",
        "file",
        "crates/alpha/src/lib.rs",
        "--graph",
        graph.to_str().unwrap(),
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let rows = rows(&run.stdout);
    assert!(
        rows.len() >= 2,
        "the fixture file must define several symbols; got {}",
        rows.len()
    );

    assert_eq!(
        rows[0]["crate_attribution"]["package_name"], "alpha",
        "the first row must name the owning package: {}",
        rows[0]
    );
    assert_eq!(
        rows[0]["crate_attribution"]["manifest_repo_relative_path"],
        "crates/alpha/Cargo.toml"
    );
    for row in &rows[1..] {
        assert!(
            row.get("crate_attribution").is_none(),
            "later rows must not repeat the file-level fact: {row}"
        );
    }
}

/// A `[workspace]` manifest carrying a package-only section is one Cargo
/// REJECTS, so it must stop the walk rather than be treated as a usable
/// virtual root.
///
/// `[workspace]` presence alone is not enough to make a manifest loadable.
/// Verified against real `cargo metadata`: `dependencies`, `dev-dependencies`,
/// `build-dependencies`, `features`, `target`, `lib`, `bin`, `bench`, `test`,
/// `example`, `badges`, and `lints` are each rejected beside `[workspace]` with
/// "this virtual manifest specifies a `<section>` section, which is not
/// allowed". `[profile]` and `[patch]` are accepted.
#[test]
fn virtual_manifest_with_a_package_only_section_stops_the_walk() {
    for section in [
        "[dependencies]\nserde = \"1\"\n",
        "[dev-dependencies]\nserde = \"1\"\n",
        "[build-dependencies]\nserde = \"1\"\n",
        "[features]\ndefault = []\n",
        "[lib]\nname = \"x\"\npath = \"src/lib.rs\"\n",
        "[badges]\nmaintenance = { status = \"active\" }\n",
        // `hints` is a NEWER Cargo section; the underscore spellings are legacy
        // aliases. All three are rejected beside `[workspace]`.
        "[hints]\nmostly-unused = true\n",
        "[dev_dependencies]\nserde = \"1\"\n",
        "[build_dependencies]\nserde = \"1\"\n",
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "nested/Cargo.toml",
                    &format!("[workspace]\nmembers = []\n\n{section}"),
                ),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([
                "unattributed:unusable_manifest".to_owned()
            ])),
            "a virtual manifest carrying `{}` is rejected by Cargo and must not be walked past",
            section.lines().next().unwrap_or_default()
        );
    }
}

/// A `[workspace]` manifest carrying only sections Cargo ALLOWS is still a
/// usable virtual root and is still walked past.
#[test]
fn virtual_manifest_with_allowed_sections_is_still_walked_past() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
            ),
            (
                // `[profile]` and `[patch]` are accepted beside `[workspace]`.
                "nested/Cargo.toml",
                "[workspace]\nmembers = []\n\n[profile.release]\nopt-level = 3\n",
            ),
            ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
        ],
    );
    let by_path = attribution_by_path(&scan_fixture(temp.path()));
    assert_eq!(
        by_path.get("nested/src/lib.rs"),
        Some(&BTreeSet::from(["outer@Cargo.toml".to_owned()])),
        "a loadable virtual root must still be walked past"
    );
}

/// In a shared multi-repository store, one repo-relative path can exist in two
/// repositories with different owning packages.
///
/// Carrying the file-level attribution once for the WHOLE result set would
/// attach one repository's package to a row from another and drop the second
/// repository's fact entirely — a repo-relative path is not globally unique.
#[test]
fn query_file_carries_attribution_once_per_repository() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();
    for (repo_id, package) in [("repo-a", "pkg-a"), ("repo-b", "pkg-b")] {
        let repo = temp.path().join(repo_id);
        fs::create_dir_all(&repo).expect("repo dir");
        write_fixture(
            &repo,
            &[
                (
                    "Cargo.toml",
                    &format!("[package]\nname = \"{package}\"\nversion = \"0.1.0\"\n"),
                ),
                (
                    "src/lib.rs",
                    &format!(
                        "pub fn one_{}() -> u32 {{ 1 }}\npub fn two_{}() -> u32 {{ 2 }}\n",
                        repo_id.replace('-', "_"),
                        repo_id.replace('-', "_")
                    ),
                ),
            ],
        );
        combined.push_str(
            &scan_repository_at_with_override(&repo, FIXED_TIME, Some(repo_id))
                .expect("scan")
                .to_jsonl()
                .expect("serialize"),
        );
        combined.push('\n');
    }
    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    let run = run_query(&[
        "query",
        "file",
        "src/lib.rs",
        "--graph",
        graph.to_str().unwrap(),
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let rows = rows(&run.stdout);
    assert!(
        rows.len() >= 4,
        "both repositories' symbols: {}",
        rows.len()
    );

    // Every attribution that IS emitted must match its own row's repository.
    let mut seen_by_repo: BTreeMap<String, String> = BTreeMap::new();
    for row in &rows {
        let Some(package) = row["crate_attribution"]["package_name"].as_str() else {
            continue;
        };
        let repository = row["repository"].as_str().unwrap_or("(none)").to_owned();
        let expected = if repository.contains("repo-a") {
            "pkg-a"
        } else {
            "pkg-b"
        };
        assert_eq!(
            package, expected,
            "a row's attribution must be its OWN repository's package: {row}"
        );
        seen_by_repo.insert(repository, package.to_owned());
    }
    // And BOTH repositories' facts survive.
    assert_eq!(
        seen_by_repo.len(),
        2,
        "each repository's owning package must appear: {seen_by_repo:?}"
    );
}

/// Over a `scan-history` graph, one file's owning package can CHANGE between
/// commits, so `eg query file` must not collapse every snapshot's attribution
/// to whichever row happened to come first.
///
/// The rows of a history graph span several commits. Carrying one attribution
/// per repository would drop the later package fact entirely and — because rows
/// are sorted after projection — could attach the old package to a row from a
/// different commit.
#[test]
fn query_file_carries_each_distinct_attribution_over_history() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"before\"\nversion = \"0.1.0\"\n",
            ),
            (
                "src/lib.rs",
                "pub fn one() -> u32 { 1 }\npub fn two() -> u32 { 2 }\n",
            ),
        ],
    );
    commit(&repo, "first", "2026-06-01T00:00:00Z");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"after\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest renamed");
    commit(&repo, "rename package", "2026-06-02T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");

    let run = run_query(&[
        "query",
        "file",
        "src/lib.rs",
        "--graph",
        graph_path.to_str().unwrap(),
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    let rows = rows(&run.stdout);
    assert!(
        rows.len() >= 2,
        "history rows across commits: {}",
        rows.len()
    );

    let packages: BTreeSet<&str> = rows
        .iter()
        .filter_map(|r| r["crate_attribution"]["package_name"].as_str())
        .collect();
    assert_eq!(
        packages,
        BTreeSet::from(["before", "after"]),
        "both snapshots' owning packages must survive the answer: {packages:?}"
    );

    // And every emitted attribution belongs to the row it rides on: a row's
    // package must match the package that owned the file at ITS commit.
    //
    // Keyed on (record_id, commit): an ADR-0004 symbol ID carries no commit
    // component, so the SAME id recurs across commits with different owners —
    // keying on the id alone would collapse the two snapshots and make this
    // oracle wrong rather than the code.
    let all = parse_jsonl(&fs::read_to_string(&graph_path).expect("graph readable"));
    let truth: BTreeMap<(&str, &str), &str> = all
        .iter()
        .filter(|r| r["kind"] == "Symbol" && r["repo_relative_path"] == "src/lib.rs")
        .filter_map(|r| {
            Some((
                (r["id"].as_str()?, r["temporal"]["git_commit"].as_str()?),
                package_of(r)?,
            ))
        })
        .collect();
    assert!(
        truth.len() >= 4,
        "anti-vacuity: {} truth entries",
        truth.len()
    );
    for row in &rows {
        let Some(package) = row["crate_attribution"]["package_name"].as_str() else {
            continue;
        };
        let key = (
            row["record_id"].as_str().expect("record_id"),
            row["git_commit"].as_str().expect("git_commit"),
        );
        assert_eq!(
            truth.get(&key),
            Some(&package),
            "a row's attribution must be its OWN record's at its own commit: {row}"
        );
    }
}

/// The walk only passes a manifest it can POSITIVELY confirm is a loadable
/// virtual root; anything else stops it.
///
/// Walking past is the fail-OPEN direction — it can attribute a subtree to an
/// outer package — so it must require confidence, not merely the absence of
/// known-bad markers. Enumerating malformations is open-ended: a `package` key
/// that is present but not a table reads as "no package" to a table-only lookup,
/// yet Cargo rejects it (`invalid type: string "not-a-table", expected struct
/// TomlPackage`). Requiring a confirmed shape closes that class rather than one
/// instance of it.
#[test]
fn only_a_confirmed_virtual_root_is_walked_past() {
    for (label, manifest) in [
        // A top-level `package` key present but not a table: Cargo rejects it
        // ("invalid type: string, expected struct TomlPackage"), and a
        // table-only lookup would silently read it as package-less. The key must
        // precede the `[workspace]` header to be top-level — after it, TOML
        // nests it as `workspace.package`, which is the next case.
        (
            "top-level package = string",
            "package = \"not-a-table\"\n\n[workspace]\nmembers = []\n",
        ),
        (
            "top-level package = int",
            "package = 42\n\n[workspace]\nmembers = []\n",
        ),
        // `[workspace].package` is the inheritance table; a non-table value
        // there is equally rejected by Cargo.
        (
            "workspace.package = string",
            "[workspace]\nmembers = []\n\npackage = \"not-a-table\"\n",
        ),
        // A `workspace` key that is present but not a table.
        ("workspace = true", "workspace = true\n"),
        ("workspace = string", "workspace = \"nope\"\n"),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                ("nested/Cargo.toml", manifest),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([
                "unattributed:unusable_manifest".to_owned()
            ])),
            "`{label}` is not a confirmed virtual root and must stop the walk"
        );
    }
}

/// A read-back attribution whose fields are individually well-formed JSON but
/// could never have been PRODUCED by the resolver must own nothing.
///
/// `status: "attributed"` plus two present strings is not enough. The resolver
/// only ever emits a package name it READ from `[package].name` (gated to the
/// Cargo charset) and a repo-relative `.../Cargo.toml` path it WALKED to. A
/// value read back from a store or a hand-edited graph is operator-controlled
/// (the #104 doctrine), so trusting the shape alone lets a crafted record
/// assert ownership the resolver could never have produced — and, on the
/// `--format text` path where the value is interpolated verbatim, forge whole
/// output lines.
///
/// Each case below is checked three ways: it must not be scopable via
/// `--package`, it must not be listed in `known_packages`, and it must not
/// render as an ownership claim in text output.
#[test]
fn forged_attribution_the_resolver_could_not_produce_owns_nothing() {
    for (label, package_name, manifest_path) in [
        // Names outside the Cargo charset. The space and the newline are the
        // text-forgery vectors: `package: {name} ({manifest})` is one line, so a
        // newline in the name manufactures a second.
        ("space in name", "bad name", "crates/x/Cargo.toml"),
        (
            "newline in name",
            "one\npackage: forged",
            "crates/x/Cargo.toml",
        ),
        (
            "ansi escape in name",
            "evil\u{1b}[31m",
            "crates/x/Cargo.toml",
        ),
        ("leading digit", "9lives", "crates/x/Cargo.toml"),
        ("empty name", "", "crates/x/Cargo.toml"),
        ("path separator in name", "crates/x", "crates/x/Cargo.toml"),
        // Manifest paths that are not repo-relative citations. An absolute or
        // escaping path names a manifest OUTSIDE the scanned tree, which the
        // ancestor walk is structurally unable to reach.
        ("absolute manifest path", "forged", "/etc/Cargo.toml"),
        ("escaping manifest path", "forged", "../outside/Cargo.toml"),
        ("dot segment manifest path", "forged", "crates/./Cargo.toml"),
        (
            "windows drive manifest path",
            "forged",
            "C:/crates/Cargo.toml",
        ),
        ("backslash manifest path", "forged", "crates\\x\\Cargo.toml"),
        ("empty manifest path", "forged", ""),
        ("empty interior segment", "forged", "crates//Cargo.toml"),
        // A path that does not name a manifest at all: the citation is the
        // whole point of carrying the path, so it must actually cite one.
        ("not a manifest", "forged", "crates/x/src/lib.rs"),
        ("manifest is a directory", "forged", "crates/x/Cargo.toml/"),
        ("case-shifted manifest", "forged", "crates/x/cargo.toml"),
        // Control characters in the manifest path: the other half of the text
        // interpolation, and never present in a walked path.
        (
            "newline in manifest path",
            "forged",
            "crates/x/Cargo.toml\nforged",
        ),
    ] {
        assert_forged_attribution_owns_nothing(label, package_name, manifest_path);
    }
}

/// Stamps `(package_name, manifest_path)` onto every `handle` symbol as a
/// well-formed `status: "attributed"` payload, then asserts the corpus treats it
/// as owning nothing — not scopable, not catalogued, not rendered.
fn assert_forged_attribution_owns_nothing(label: &str, package_name: &str, manifest_path: &str) {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());

    let forged: String = fs::read_to_string(&graph)
        .expect("graph readable")
        .lines()
        .map(|line| {
            let mut record: Value = serde_json::from_str(line).expect("JSON");
            if record["kind"] == "Symbol"
                && record["name"]
                    .as_str()
                    .is_some_and(|n| n.ends_with("handle"))
                && let Some(object) = record.as_object_mut()
            {
                object.insert(
                    "crate_attribution".to_owned(),
                    serde_json::json!({
                        "status": "attributed",
                        "package_name": package_name,
                        "manifest_repo_relative_path": manifest_path,
                    }),
                );
            }
            serde_json::to_string(&record).expect("serialize")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let forged_path = temp.path().join("forged.jsonl");
    fs::write(&forged_path, forged).expect("forged graph written");
    let forged_str = forged_path.to_str().unwrap();

    // 1. Not scopable. The selector is a name no LEGITIMATE package carries, so
    //    the lane must reject it rather than return rows.
    if !package_name.is_empty() {
        let scoped = run_query(&[
            "query",
            "symbol",
            "handle",
            "--graph",
            forged_str,
            "--package",
            package_name,
        ]);
        assert_eq!(
            scoped.code, 1,
            "`{label}` must not be a scopable package; stdout: {}",
            scoped.stdout
        );
    }

    // 2. Not listed as known. `known_packages` is the operator-facing catalog;
    //    a forged name appearing there presents it as real.
    let listed = run_query(&[
        "query",
        "symbol",
        "handle",
        "--graph",
        forged_str,
        "--package",
        "definitely-not-a-package",
    ]);
    assert_eq!(listed.code, 1, "stdout: {}", listed.stdout);
    let diagnostic: Value =
        serde_json::from_str(listed.stderr.trim()).expect("one JSON diagnostic line");
    let known: Vec<&str> = diagnostic["known_packages"]
        .as_array()
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    assert!(
        !known.contains(&package_name),
        "`{label}` must not be listed as a known package: {known:?}"
    );

    // 3. Not rendered. Text output interpolates the attribution verbatim, so an
    //    unproducible value must print NOTHING rather than a claim.
    let text = run_query(&[
        "query", "symbol", "handle", "--graph", forged_str, "--format", "text",
    ]);
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    for forged_fragment in [package_name, manifest_path] {
        if forged_fragment.is_empty() {
            continue;
        }
        assert!(
            !text.stdout.contains(forged_fragment),
            "`{label}` must not render as an ownership claim: {}",
            text.stdout
        );
    }
}

/// A read-back attribution must not render a NEGATIVE ownership claim it could
/// not have produced either.
///
/// The mirror of the positive case above, and it matters for the same reason:
/// "provably no owning package" is a FACT, not a fallback. The
/// absent-vs-unattributed contract says an ABSENT field means UNKNOWN, so
/// rendering a reason carried by a self-contradictory value manufactures the
/// proven-ownerless claim the contract exists to keep separate from an unknown.
/// A value failing the consistency check must print NOTHING — neither an
/// ownership claim nor a reason.
#[test]
fn forged_unattributed_reason_the_resolver_could_not_produce_renders_nothing() {
    for (label, payload) in [
        // `status: attributed` WITH a reason: refused as an ownership claim
        // (the previous round), and it must not fall through to the negative
        // render either.
        (
            "attributed carrying a reason",
            serde_json::json!({
                "status": "attributed",
                "package_name": "contradictory",
                "manifest_repo_relative_path": "crates/alpha/Cargo.toml",
                "unattributed_reason": "no_enclosing_manifest",
            }),
        ),
        // `status: unattributed` while still carrying an attribution: the
        // producer writes one or the other, never both.
        (
            "unattributed carrying a package name",
            serde_json::json!({
                "status": "unattributed",
                "package_name": "contradictory",
                "unattributed_reason": "virtual_manifest_only",
            }),
        ),
        (
            "unattributed carrying a manifest path",
            serde_json::json!({
                "status": "unattributed",
                "manifest_repo_relative_path": "crates/alpha/Cargo.toml",
                "unattributed_reason": "virtual_manifest_only",
            }),
        ),
        // `status: attributed` with NEITHER string: no ownership claim is
        // possible, and there is no reason to fall back to.
        (
            "attributed carrying nothing",
            serde_json::json!({ "status": "attributed" }),
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        let graph = write_graph(temp.path());

        let forged: String = fs::read_to_string(&graph)
            .expect("graph readable")
            .lines()
            .map(|line| {
                let mut record: Value = serde_json::from_str(line).expect("JSON");
                if record["kind"] == "Symbol"
                    && record["name"]
                        .as_str()
                        .is_some_and(|n| n.ends_with("handle"))
                    && let Some(object) = record.as_object_mut()
                {
                    object.insert("crate_attribution".to_owned(), payload.clone());
                }
                serde_json::to_string(&record).expect("serialize")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let forged_path = temp.path().join("forged.jsonl");
        fs::write(&forged_path, forged).expect("forged graph written");

        let text = run_query(&[
            "query",
            "symbol",
            "handle",
            "--graph",
            forged_path.to_str().unwrap(),
            "--format",
            "text",
        ]);
        assert_eq!(text.code, 0, "stderr: {}", text.stderr);
        assert!(
            !text.stdout.contains("package:"),
            "`{label}` must render no package line at all: {}",
            text.stdout
        );
    }
}

/// A LEGITIMATE unattributed value still renders its reason — the check above
/// must not silence the honest negative fact it exists to protect.
#[test]
fn consistent_unattributed_value_still_renders_its_reason() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = write_graph(temp.path());
    // `scripts/gen.rs` sits outside every package, under a virtual root only.
    let text = run_query(&[
        "query",
        "symbol",
        "generate",
        "--graph",
        graph.to_str().unwrap(),
        "--format",
        "text",
    ]);
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        text.stdout
            .contains("package: (unattributed: virtual_manifest_only)"),
        "a resolver-produced unattributed value must still render: {}",
        text.stdout
    );
}

/// A package rename must not let `--as-of` resurrect the SUPERSEDED owner.
///
/// `symbol_as_of_valid_time_by_repo` picks ONE winner per repository across
/// every record carrying the queried name, so removing a record from its input
/// can PROMOTE a different one. When a symbol keeps its stable ID while its
/// enclosing manifest is renamed, filtering on the package BEFORE the snapshot
/// is resolved deletes the current version and lets the superseded one win — an
/// answer asserting ownership that ended before the queried instant.
///
/// The instant decides the owner: before the rename the symbol is `alpha`,
/// after it the symbol is `beta`, and `--package alpha` after the rename is an
/// honest no-match.
#[test]
fn package_scope_as_of_does_not_resurrect_a_renamed_packages_ownership() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    // One crate, one symbol. The SOURCE never changes, so the symbol keeps its
    // stable ID across both commits; only the manifest's package name moves.
    write_fixture(
        &repo,
        &[
            (
                "crates/only/Cargo.toml",
                "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/only/src/lib.rs",
                "pub fn renamed_owner() -> u32 { 1 }\n",
            ),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");
    write_fixture(
        &repo,
        &[(
            "crates/only/Cargo.toml",
            "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
        )],
    );
    commit(&repo, "rename the package", "2026-07-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");
    let graph = graph_path.to_str().unwrap();

    let as_of_query = |instant: &str, package: &str| {
        run_query(&[
            "query",
            "symbol",
            "renamed_owner",
            "--graph",
            graph,
            "--as-of",
            instant,
            "--package",
            package,
        ])
    };

    // BEFORE the rename the owner is `alpha` — a real match.
    let before = as_of_query("2026-06-15T00:00:00Z", "alpha");
    assert_eq!(before.code, 0, "stderr: {}", before.stderr);
    let before_rows = rows(&before.stdout);
    assert_eq!(before_rows.len(), 1);
    assert_eq!(before_rows[0]["crate_attribution"]["package_name"], "alpha");

    // AFTER the rename the owner is `beta`.
    let after_beta = as_of_query("2026-08-01T00:00:00Z", "beta");
    assert_eq!(after_beta.code, 0, "stderr: {}", after_beta.stderr);
    let after_rows = rows(&after_beta.stdout);
    assert_eq!(after_rows.len(), 1);
    assert_eq!(after_rows[0]["crate_attribution"]["package_name"], "beta");

    // And `alpha` after the rename owns nothing at that instant. Returning the
    // pre-rename version here would present ownership that had already ended.
    let after_alpha = as_of_query("2026-08-01T00:00:00Z", "alpha");
    assert_eq!(
        after_alpha.code, 2,
        "`alpha` no longer owns the symbol at this instant; stdout: {}",
        after_alpha.stdout
    );
}

/// A `[workspace]` table Cargo REFUSES TO LOAD must stop the walk.
///
/// Classifying a manifest as a virtual root is the fail-open direction: the
/// walk passes it and the subtree inherits an outer package. Cargo type-checks
/// every known `[workspace]` field, so a wrong-typed one makes the whole
/// manifest unloadable — nothing there declares a package, and no outer package
/// owns those files either.
///
/// Each rejected case below was verified against real `cargo metadata
/// --no-deps --format-version 1` on Cargo 1.94.1, as was each ACCEPTED one:
/// `metadata` takes any type and an unknown key is tolerated, so neither may
/// un-attribute a subtree.
#[test]
fn malformed_workspace_fields_stop_the_walk() {
    for (label, workspace, expected) in [
        (
            "members = string",
            "[workspace]\nmembers = \"not-an-array\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "members element = int",
            "[workspace]\nmembers = [1]\n",
            "unattributed:unusable_manifest",
        ),
        (
            "exclude = string",
            "[workspace]\nmembers = []\nexclude = \"nope\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "default-members = string",
            "[workspace]\nmembers = []\ndefault-members = \"nope\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "resolver = int",
            "[workspace]\nmembers = []\nresolver = 2\n",
            "unattributed:unusable_manifest",
        ),
        (
            "dependencies = string",
            "[workspace]\nmembers = []\ndependencies = \"nope\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "lints = string",
            "[workspace]\nmembers = []\nlints = \"nope\"\n",
            "unattributed:unusable_manifest",
        ),
        // Cargo ACCEPTS these, so they stay walked past — a false rejection
        // would un-attribute a real subtree.
        (
            "metadata = string (any type accepted)",
            "[workspace]\nmembers = []\nmetadata = \"anything\"\n",
            "outer@Cargo.toml",
        ),
        (
            "unknown key tolerated",
            "[workspace]\nmembers = []\nfuture-tool-key = \"whatever\"\n",
            "outer@Cargo.toml",
        ),
        (
            "well-typed control",
            "[workspace]\nmembers = []\n",
            "outer@Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                ("nested/Cargo.toml", workspace),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`"
        );
    }
}

/// On Unix a backslash is an ordinary FILENAME character, not a path
/// separator, so a real package directory can contain one.
///
/// `normalize_path` joins `Path::components()` with `/`, which preserves a
/// literal `\` inside a component, and the scan's `git ls-files -z` /
/// `ls-tree -z` listings read NUL-delimited bytes, so such a path reaches the
/// resolver intact. Rejecting every backslash-bearing manifest path therefore
/// dropped a REAL manifest from the index, un-attributing its whole subtree and
/// making its package unreachable via `--package`.
///
/// The forgery this guard was aimed at — a Windows-separated
/// `crates\x\Cargo.toml` — is still rejected, by the rule that already governs
/// it: the last `/`-segment must be exactly `Cargo.toml`, and a fully
/// backslash-separated path has no such segment.
#[cfg(unix)]
#[test]
fn a_literal_backslash_in_a_directory_name_is_attributed() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/odd\"]\n"),
            (
                // Declares a dependency, so the manifest itself gets a `File`
                // node (a dependency-free manifest mints none) and the
                // assertion below can prove the package is really in the index
                // rather than merely absent from the miss list.
                "crates/odd\\dir/Cargo.toml",
                "[package]\nname = \"oddball\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
            ),
            ("crates/odd\\dir/src/lib.rs", "pub fn odd() -> u32 { 1 }\n"),
        ],
    );

    let by_path = attribution_by_path(&scan_fixture(temp.path()));
    assert_eq!(
        by_path.get("crates/odd\\dir/src/lib.rs"),
        Some(&BTreeSet::from([
            "oddball@crates/odd\\dir/Cargo.toml".to_owned()
        ])),
        "a literal backslash is part of the directory NAME, not a separator: {by_path:?}"
    );
    // And the manifest node self-attributes, so the package is really in the
    // index rather than merely absent from the miss list.
    assert_eq!(
        by_path.get("crates/odd\\dir/Cargo.toml"),
        Some(&BTreeSet::from([
            "oddball@crates/odd\\dir/Cargo.toml".to_owned()
        ]))
    );

    // The Git-object harvest must agree with the working-tree one — the whole
    // point of one shared resolver. History reads paths through `ls-tree -z`,
    // whose NUL delimiters carry a backslash byte intact where the default
    // C-quoting would not.
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    for (relative, contents) in [
        ("Cargo.toml", "[workspace]\nmembers = [\"crates/odd\"]\n"),
        (
            "crates/odd\\dir/Cargo.toml",
            "[package]\nname = \"oddball\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
        ),
        ("crates/odd\\dir/src/lib.rs", "pub fn odd() -> u32 { 1 }\n"),
    ] {
        let path = repo.join(relative);
        fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        fs::write(path, contents).expect("write");
    }
    init_git(&repo);
    let sha = commit(&repo, "seed", "2026-06-01T00:00:00Z");
    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&sha].get("crates/odd\\dir/src/lib.rs"),
        Some(&BTreeSet::from([
            "oddball@crates/odd\\dir/Cargo.toml".to_owned()
        ])),
        "history must agree with the working-tree harvest: {by_commit:?}"
    );
}

/// A tab or a colon in a directory name is ORDINARY on Unix, so the manifest
/// under it must still own its subtree.
///
/// The NUL-delimited `ls-files`/`ls-tree` listings carry both bytes intact, so
/// these are manifests the walk really reaches. Rejecting them dropped the fact
/// from the index and the subtree either lost ownership or — worse — inherited
/// an OUTER package, which is the fabrication this feature exists to prevent.
///
/// Text-output safety is a separate concern, handled where the value is
/// rendered rather than by refusing to record the fact; see
/// `a_control_character_in_a_manifest_path_cannot_forge_a_text_line`.
#[cfg(unix)]
#[test]
fn control_and_colon_directory_names_are_attributed() {
    for (label, dir) in [
        ("tab", "od\td"),
        ("newline", "od\nd"),
        ("colon", "vendor:patched"),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    &format!("{dir}/Cargo.toml"),
                    "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
                ),
                (&format!("{dir}/src/lib.rs"), "pub fn f() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get(&format!("{dir}/src/lib.rs")),
            Some(&BTreeSet::from([format!("inner@{dir}/Cargo.toml")])),
            "`{label}`: the nearest manifest owns it, and `outer` must NOT: {by_path:?}"
        );
    }
}

/// Relaxing the producer-side check must not reopen the text-forgery hole.
///
/// A control character in a directory name is legal on Unix, so the path is
/// RECORDABLE and the manifest legitimately encloses its own subtree. The
/// render — not the predicate — is therefore what keeps one row to one line: the
/// citation is control-sanitized on the way out, exactly as every other
/// operator-controlled handle in this codebase is, and never truncated, because
/// a truncated handle stops being a citation.
///
/// Built on a REAL directory rather than a forged attribution, so it exercises
/// the path the resolver actually produces; a forged non-enclosing manifest is
/// refused outright and would prove nothing about rendering.
#[cfg(unix)]
#[test]
fn a_control_character_in_a_manifest_path_cannot_forge_a_text_line() {
    let temp = tempfile::tempdir().expect("temp dir");
    // A legal Unix directory name carrying a newline, the text a forged line
    // would use, and the ANSI escape that would clear a terminal.
    let dir = "x\nsymbol: INJECTED\u{1b}[2J";
    write_fixture(
        temp.path(),
        &[
            (
                &format!("{dir}/Cargo.toml"),
                "[package]\nname = \"realpkg\"\nversion = \"0.1.0\"\n",
            ),
            (
                &format!("{dir}/src/lib.rs"),
                "pub fn forged_symbol() -> u32 { 1 }\n",
            ),
        ],
    );
    let graph_path = temp.path().join("graph.jsonl");
    fs::write(&graph_path, scan_jsonl(temp.path())).expect("graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "forged_symbol",
        "--graph",
        graph_path.to_str().unwrap(),
        "--format",
        "text",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        !run.stdout.contains('\u{1b}'),
        "an ANSI escape must never reach the terminal: {:?}",
        run.stdout
    );
    assert!(
        !run.stdout
            .lines()
            .any(|line| line.trim_start().starts_with("symbol: INJECTED")),
        "the newline must not forge a second `symbol:` line: {:?}",
        run.stdout
    );
    // The citation still renders in full — sanitized, never truncated.
    assert!(
        run.stdout
            .contains("package: realpkg (x.symbol: INJECTED.[2J/Cargo.toml)"),
        "the handle must stay whole and usable: {:?}",
        run.stdout
    );
}

/// NUL is the ONE byte a POSIX path cannot contain, so a manifest path carrying
/// one is provably not a value the walk produced.
///
/// This is the line the previous round's relaxation has to stop at. A tab or a
/// newline names a REAL directory — the render displays it lossily but the
/// stored value still resolves — whereas NUL terminates a C string, is
/// forbidden in a filename by POSIX, and is the DELIMITER of the `-z` listings
/// the harvest reads, so no discovery path can deliver it. Accepting one would
/// let a crafted record claim scopable ownership and hand back a citation that
/// sanitization silently rewrites into a path that never existed.
#[test]
fn a_nul_in_a_manifest_path_is_not_a_believable_citation() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"realpkg\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn nul_target() -> u32 { 1 }\n"),
        ],
    );
    let mut lines: Vec<String> = Vec::new();
    for mut record in scan_fixture(temp.path()) {
        if record["kind"] == "Symbol"
            && let Some(attribution) = record.get_mut("crate_attribution")
        {
            // A NUL inside a DIRECTORY segment: the manifest name itself stays
            // intact, so only the NUL rule can reject this.
            attribution["manifest_repo_relative_path"] = Value::from("crates/\u{0}x/Cargo.toml");
        }
        lines.push(serde_json::to_string(&record).expect("serialize"));
    }
    let graph_path = temp.path().join("forged.jsonl");
    fs::write(&graph_path, lines.join("\n")).expect("graph written");
    let graph = graph_path.to_str().unwrap();

    // It renders NOTHING — exactly what an absent attribution renders, never a
    // sanitized citation to a path that cannot exist.
    let text = run_query(&[
        "query",
        "symbol",
        "nul_target",
        "--graph",
        graph,
        "--format",
        "text",
    ]);
    assert_eq!(text.code, 0, "stderr: {}", text.stderr);
    assert!(
        !text.stdout.contains("package:"),
        "a NUL-bearing path must not render a citation: {:?}",
        text.stdout
    );

    // And the forged row is not SCOPABLE under the package it names. `realpkg`
    // is a real package here — the file and module records carry a legitimate
    // attribution to it — so the honest verdict is the lane's ordinary "known
    // package, zero matching rows" (exit 2), not an unknown selector. What must
    // not happen is the forged symbol answering as if it belonged.
    let scoped = run_query(&[
        "query",
        "symbol",
        "nul_target",
        "--graph",
        graph,
        "--package",
        "realpkg",
    ]);
    assert_eq!(
        scoped.code, 2,
        "a forged attribution must not make the symbol scopable; stdout: {}",
        scoped.stdout
    );
    assert!(
        !scoped.stdout.contains("nul_target"),
        "no row may be returned for the forged attribution: {:?}",
        scoped.stdout
    );
}

/// The DOCUMENTED list of attributed node kinds must match the code's total
/// function, in both places it is enumerated.
///
/// `carries_crate_attribution` is an exhaustive no-wildcard match precisely so
/// presence cannot drift into a partial function — but a prose list claiming to
/// name "every path-bearing code-graph node" is a second, unchecked copy of
/// that contract, and a schema consumer reading it would skip attribution
/// handling for any kind the list forgot. `Change` was exactly that: attributed
/// by the code, absent from both lists.
///
/// The check is scoped to the parenthesized enumeration rather than the whole
/// file, so a kind merely mentioned elsewhere cannot satisfy it.
#[test]
fn documented_attributed_kinds_match_the_code() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let attributed: Vec<&str> = NodeKind::ALL
        .iter()
        .filter(|kind| aletheia_egregore::crate_attribution::carries_crate_attribution(**kind))
        .map(|kind| kind.as_str())
        .collect();
    assert!(
        attributed.len() > 1,
        "the fixture is meaningless if nothing is attributed"
    );

    // Each list is delimited explicitly, so the check reads the ENUMERATION and
    // not the whole file: a kind merely mentioned in nearby prose must not
    // satisfy it.
    for (doc, open_marker, close_marker) in [
        (
            "docs/schema/schema-versioning.md",
            "on every path-bearing code-graph node (",
            ")",
        ),
        ("CLAUDE.md", "Every path-bearing code-graph node (", ")"),
        (
            "docs/cli/crate-attribution.md",
            "### Which nodes carry it",
            "— every",
        ),
    ] {
        let text = fs::read_to_string(repo_root.join(doc))
            .unwrap_or_else(|e| panic!("{doc} should be readable: {e}"));
        let flattened = text.replace('\n', " ");
        let start = flattened
            .find(open_marker)
            .unwrap_or_else(|| panic!("{doc} should carry the enumeration anchor `{open_marker}`"));
        let rest = &flattened[start + open_marker.len()..];
        let end = rest
            .find(close_marker)
            .unwrap_or_else(|| panic!("{doc}: unterminated enumeration"));
        let list = &rest[..end];
        for kind in &attributed {
            assert!(
                list.contains(&format!("`{kind}`")),
                "{doc} claims to enumerate every attributed kind but omits `{kind}`: {list}"
            );
        }
    }
}

/// A deleted file's `Change` must be attributed to the package that OWNED it,
/// not to whatever the walk finds after the manifest is gone.
///
/// Attribution resolves against each commit's own manifest tree, which is right
/// for every record describing the tree AS IT IS. A deletion describes a path
/// that is no longer there: when the commit removes the enclosing `Cargo.toml`
/// along with its sources — a whole-package removal — the post-commit walk finds
/// the OUTER manifest and claims the deleted file belonged to a package it never
/// belonged to. The relevant tree for a deletion is the parent's.
#[test]
fn a_deleted_files_change_is_attributed_to_the_package_it_left() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn outer_fn() -> u32 { 1 }\n"),
            (
                "crates/inner/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            ),
            (
                "crates/inner/src/lib.rs",
                "pub fn inner_fn() -> u32 { 2 }\n",
            ),
        ],
    );
    let added = commit(&repo, "add both packages", "2026-06-01T00:00:00Z");

    // Whole-package removal: the manifest AND its sources go in one commit.
    fs::remove_dir_all(repo.join("crates")).expect("remove the inner package");
    let removed = commit(&repo, "remove the inner package", "2026-07-01T00:00:00Z");

    let by_commit = history_attribution(&repo);

    // Baseline: while it existed, the source belonged to `inner`.
    assert_eq!(
        by_commit[&added].get("crates/inner/src/lib.rs"),
        Some(&BTreeSet::from(
            ["inner@crates/inner/Cargo.toml".to_owned()]
        )),
        "the file belonged to `inner` when it existed: {by_commit:?}"
    );

    // The deletion must cite the package that LOST the file. `outer` never
    // owned it, and reporting no enclosing manifest would erase which package
    // shrank — either way a whole-package removal becomes unreadable.
    assert_eq!(
        by_commit[&removed].get("crates/inner/src/lib.rs"),
        Some(&BTreeSet::from(
            ["inner@crates/inner/Cargo.toml".to_owned()]
        )),
        "the deletion must be attributed to `inner`, not to the outer package \
         the walk reaches once the manifest is gone: {by_commit:?}"
    );
}

/// Deleting ONE file, leaving its manifest in place, is unchanged.
///
/// The parent-tree re-resolution must not become a second, divergent rule: for
/// the common deletion the post-commit and parent trees agree, and both name
/// the enclosing package. This pins that the fix is scoped to the case where
/// the trees actually disagree.
#[test]
fn a_lone_file_deletion_keeps_its_enclosing_package() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "crates/inner/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/inner/src/lib.rs", "pub fn kept() -> u32 { 1 }\n"),
            ("crates/inner/src/gone.rs", "pub fn gone() -> u32 { 2 }\n"),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");
    fs::remove_file(repo.join("crates/inner/src/gone.rs")).expect("remove one file");
    let removed = commit(&repo, "remove one file", "2026-07-01T00:00:00Z");

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&removed].get("crates/inner/src/gone.rs"),
        Some(&BTreeSet::from(
            ["inner@crates/inner/Cargo.toml".to_owned()]
        )),
        "the manifest survived, so both trees agree: {by_commit:?}"
    );
}

/// A symbol MOVED between packages must not answer from the package it left.
///
/// Round eleven collapsed each symbol IDENTITY to the version current at the
/// instant, which fixes a manifest rename (the ID is stable, so there is one
/// identity and its current version carries the new package). A file MOVE is
/// different: the path is part of the ADR-0004 preimage, so the old and new
/// locations are DISTINCT identities. Collapsing per identity keeps the old
/// one's last version alive forever, and the package filter — which removes the
/// current row — then lets that stale row win.
///
/// Snapshot membership has to be resolved first: at an instant after the move
/// the symbol exists only in `beta`, so `--package alpha` is an honest no-match.
#[test]
fn package_scope_as_of_does_not_answer_from_a_package_the_symbol_left() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/alpha\", \"crates/beta\"]\n",
            ),
            (
                "crates/alpha/Cargo.toml",
                "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/alpha/src/lib.rs", "pub fn moved() -> u32 { 1 }\n"),
            (
                "crates/beta/Cargo.toml",
                "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/beta/src/lib.rs", "pub fn stays() -> u32 { 2 }\n"),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    // The move: same symbol, new owning package, so a NEW stable ID.
    fs::remove_file(repo.join("crates/alpha/src/lib.rs")).expect("remove");
    fs::write(
        repo.join("crates/beta/src/lib.rs"),
        "pub fn stays() -> u32 { 2 }\npub fn moved() -> u32 { 1 }\n",
    )
    .expect("write");
    commit(&repo, "move moved() to beta", "2026-07-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");
    let graph = graph_path.to_str().unwrap();
    let after_move = "2026-08-01T00:00:00Z";

    // After the move the symbol lives in `beta`.
    let beta = run_query(&[
        "query",
        "symbol",
        "moved",
        "--graph",
        graph,
        "--as-of",
        after_move,
        "--package",
        "beta",
    ]);
    assert_eq!(beta.code, 0, "stderr: {}", beta.stderr);
    let beta_rows = rows(&beta.stdout);
    assert_eq!(beta_rows.len(), 1);
    assert_eq!(beta_rows[0]["crate_attribution"]["package_name"], "beta");

    // And `alpha` owns nothing at that instant. Returning the pre-move row here
    // would answer from a file that no longer exists.
    let alpha = run_query(&[
        "query",
        "symbol",
        "moved",
        "--graph",
        graph,
        "--as-of",
        after_move,
        "--package",
        "alpha",
    ]);
    assert_eq!(
        alpha.code, 2,
        "`alpha` no longer holds `moved` at this instant; stdout: {}",
        alpha.stdout
    );

    // Before the move it really was in `alpha` — the fix must not erase that.
    let before = run_query(&[
        "query",
        "symbol",
        "moved",
        "--graph",
        graph,
        "--as-of",
        "2026-06-15T00:00:00Z",
        "--package",
        "alpha",
    ]);
    assert_eq!(before.code, 0, "stderr: {}", before.stderr);
    let before_rows = rows(&before.stdout);
    assert_eq!(before_rows[0]["crate_attribution"]["package_name"], "alpha");
}

/// A collision that exists ONLY in history must not refuse a HEAD answer.
///
/// The package catalog decides two things — `known_packages` for a typo, and
/// cross-repository ambiguity — and it was built from the raw whole-corpus
/// record set BEFORE the corpus mode narrowed to HEAD. So a package name that
/// two repositories held at different times, but only ONE holds at HEAD, exited
/// 1 `ambiguous_package_selector` even though the default HEAD-anchored answer
/// is unambiguous: a refusal for a question with a well-defined answer.
///
/// The catalog must describe the corpus the answer is computed over.
#[test]
fn a_historical_only_package_collision_does_not_block_the_head_answer() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();

    // Repo A HELD `util`, then renamed it away. At HEAD it owns `util_a`.
    let repo_a = temp.path().join("repo-a");
    fs::create_dir_all(&repo_a).expect("repo dir");
    init_git(&repo_a);
    write_fixture(
        &repo_a,
        &[
            (
                "crates/util/Cargo.toml",
                "[package]\nname = \"util\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/util/src/lib.rs", "pub fn helper() -> u32 { 1 }\n"),
        ],
    );
    commit(&repo_a, "seed", "2026-06-01T00:00:00Z");
    write_fixture(
        &repo_a,
        &[(
            "crates/util/Cargo.toml",
            "[package]\nname = \"util_a\"\nversion = \"0.1.0\"\n",
        )],
    );
    commit(&repo_a, "rename util -> util_a", "2026-07-01T00:00:00Z");
    combined.push_str(
        &aletheia_egregore::scan_repository_history(&repo_a)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    );
    combined.push('\n');

    // Repo B owns `util` throughout, including at HEAD.
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_b).expect("repo dir");
    init_git(&repo_b);
    write_fixture(
        &repo_b,
        &[
            (
                "crates/util/Cargo.toml",
                "[package]\nname = \"util\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/util/src/lib.rs", "pub fn helper() -> u32 { 2 }\n"),
        ],
    );
    commit(&repo_b, "seed", "2026-06-15T00:00:00Z");
    combined.push_str(
        &aletheia_egregore::scan_repository_history(&repo_b)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    );
    combined.push('\n');

    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    // `query symbol` HEAD-anchors by default (issue #456), and at HEAD only
    // repo B owns `util` — so this is answerable.
    let run = run_query(&[
        "query",
        "symbol",
        "helper",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "util",
    ]);
    assert_eq!(
        run.code, 0,
        "only one repository owns `util` at HEAD; stdout: {} stderr: {}",
        run.stdout, run.stderr
    );
    for row in rows(&run.stdout) {
        assert_eq!(row["crate_attribution"]["package_name"], "util");
    }

    // The collision is genuine over the UNION corpus, so `--all-history` must
    // still refuse rather than silently merge the two repositories.
    let union = run_query(&[
        "query",
        "symbol",
        "helper",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "util",
        "--all-history",
    ]);
    assert_eq!(
        union.code, 1,
        "over the union corpus the collision is genuine; stdout: {}",
        union.stdout
    );
    assert!(
        union.stderr.contains("ambiguous_package_selector"),
        "stderr: {}",
        union.stderr
    );
}

/// Known-ness is corpus-wide; only AMBIGUITY narrows to the queried corpus.
///
/// The two `--package` verdicts answer different questions. Ambiguity asks
/// "would scoping fold two repositories together?", which is only meaningful
/// over the corpus being answered from. Known-ness is TYPO protection — "is
/// this a package name this store carries at all?" — and deliberately stays
/// corpus-wide.
///
/// So a package created after the `--as-of` instant is still KNOWN, and the
/// answer is the lane's ordinary exit-2 "known package, zero rows". Reporting
/// `unknown_package_selector` instead would print a `known_packages` list that
/// omits a package the store demonstrably carries, and steer the caller to fix
/// a spelling that was never wrong. The catalog also cannot tell that case
/// apart from a package whose last source file moved away — it still exists,
/// but a source-less manifest leaves no attributed record — so narrowing
/// known-ness would call a live package unknown too.
#[test]
fn a_package_created_after_the_as_of_instant_stays_known() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/early\"]\n"),
            (
                "crates/early/Cargo.toml",
                "[package]\nname = \"early\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/early/src/lib.rs", "pub fn shared() -> u32 { 1 }\n"),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[workspace]\nmembers = [\"crates/early\", \"crates/late\"]\n",
            ),
            (
                "crates/late/Cargo.toml",
                "[package]\nname = \"late\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/late/src/lib.rs", "pub fn shared() -> u32 { 2 }\n"),
        ],
    );
    commit(&repo, "add the late package", "2026-07-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");
    let graph = graph_path.to_str().unwrap();

    // Before `late` existed: a real package name, no rows at that instant.
    let before = run_query(&[
        "query",
        "symbol",
        "shared",
        "--graph",
        graph,
        "--as-of",
        "2026-06-15T00:00:00Z",
        "--package",
        "late",
    ]);
    assert_eq!(
        before.code, 2,
        "`late` is a real package that owns nothing at this instant, not a typo; \
         stdout: {} stderr: {}",
        before.stdout, before.stderr
    );
    assert!(
        !before.stderr.contains("unknown_package_selector"),
        "a real package must not be reported as an unknown selector: {}",
        before.stderr
    );

    // After it exists, the same query resolves normally.
    let after = run_query(&[
        "query",
        "symbol",
        "shared",
        "--graph",
        graph,
        "--as-of",
        "2026-08-01T00:00:00Z",
        "--package",
        "late",
    ]);
    assert_eq!(after.code, 0, "stderr: {}", after.stderr);
    let rows = rows(&after.stdout);
    assert_eq!(rows[0]["crate_attribution"]["package_name"], "late");
}

/// A cited manifest that does not ENCLOSE the record could not have produced
/// its attribution.
///
/// The shape checks accept any well-formed manifest path, but the resolver
/// derives attribution from the NEAREST ENCLOSING manifest — so the cited
/// manifest's directory is always an ancestor of (or equal to) the record's own
/// directory. A record at `crates/beta/src/lib.rs` citing
/// `crates/alpha/Cargo.toml` is a shape no walk produces, and believing it
/// hands back a forged ownership claim with a citation that points somewhere
/// else entirely.
///
/// Containment is segment-aware, so `crates/alpha` never encloses
/// `crates/alphabet/x.rs` — the same rule the walk itself uses.
#[test]
fn a_manifest_that_does_not_enclose_the_record_is_not_trusted() {
    for (label, forged_manifest) in [
        ("a sibling package", "crates/alpha/Cargo.toml"),
        (
            "a deeper unrelated package",
            "crates/alpha/inner/Cargo.toml",
        ),
        // Segment-aware: a prefix of the real directory name is not an ancestor.
        ("a name-prefix neighbour", "crates/bet/Cargo.toml"),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "crates/beta/Cargo.toml",
                    "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
                ),
                ("crates/beta/src/lib.rs", "pub fn owned() -> u32 { 1 }\n"),
            ],
        );
        let mut lines: Vec<String> = Vec::new();
        for mut record in scan_fixture(temp.path()) {
            if record["kind"] == "Symbol"
                && let Some(attribution) = record.get_mut("crate_attribution")
            {
                attribution["package_name"] = Value::from("alpha");
                attribution["manifest_repo_relative_path"] = Value::from(forged_manifest);
            }
            lines.push(serde_json::to_string(&record).expect("serialize"));
        }
        let graph_path = temp.path().join("forged.jsonl");
        fs::write(&graph_path, lines.join("\n")).expect("graph written");
        let graph = graph_path.to_str().unwrap();

        // Renders nothing: the claim is unbelievable, so it reads exactly as an
        // absent attribution does.
        let text = run_query(&[
            "query", "symbol", "owned", "--graph", graph, "--format", "text",
        ]);
        assert_eq!(text.code, 0, "`{label}` stderr: {}", text.stderr);
        assert!(
            !text.stdout.contains("package:"),
            "`{label}`: a non-enclosing manifest must not render a citation: {:?}",
            text.stdout
        );

        // And `alpha` never enters the catalog, so it is not a scopable package.
        let scoped = run_query(&[
            "query",
            "symbol",
            "owned",
            "--graph",
            graph,
            "--package",
            "alpha",
        ]);
        assert_eq!(
            scoped.code, 1,
            "`{label}`: forged ownership must not make `alpha` selectable; stdout: {}",
            scoped.stdout
        );
        assert!(
            scoped.stderr.contains("unknown_package_selector")
                || scoped.stderr.contains("crate_attribution_unavailable"),
            "`{label}` stderr: {}",
            scoped.stderr
        );
    }
}

/// The enclosing check must not reject what the resolver really produces.
///
/// Two shapes look unusual but are correct: a manifest's own `File` node cites
/// ITSELF (its directory encloses it, by equality), and a repo-root manifest
/// encloses every path in the repository.
#[test]
fn self_attribution_and_root_manifests_still_pass_the_enclosing_check() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
            ),
            ("src/lib.rs", "pub fn at_root() -> u32 { 1 }\n"),
            (
                "crates/inner/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
            ),
            ("crates/inner/src/lib.rs", "pub fn nested() -> u32 { 2 }\n"),
        ],
    );
    let by_path = attribution_by_path(&scan_fixture(temp.path()));
    // Root manifest encloses a deep path.
    assert_eq!(
        by_path.get("src/lib.rs"),
        Some(&BTreeSet::from(["root@Cargo.toml".to_owned()]))
    );
    // A manifest's own File node cites itself.
    assert_eq!(
        by_path.get("crates/inner/Cargo.toml"),
        Some(&BTreeSet::from(
            ["inner@crates/inner/Cargo.toml".to_owned()]
        ))
    );
    assert_eq!(
        by_path.get("Cargo.toml"),
        Some(&BTreeSet::from(["root@Cargo.toml".to_owned()]))
    );
}

/// A forged RECORD path is not owned by anything, end to end.
///
/// The mirror of the non-enclosing-manifest case: ancestry is meaningless over
/// a path no producer emits. `ancestor_dirs` always ends at the repo root, so a
/// root manifest would "enclose" `../outside.rs`; and a `..` segment reads as an
/// ordinary ancestor, so `crates/beta/../../outside.rs` would be enclosed by
/// `crates/beta/Cargo.toml` on its way out of the repository.
#[test]
fn a_forged_record_path_is_not_owned_by_the_cited_package() {
    for (label, forged_path) in [
        ("escapes the repository", "../outside.rs"),
        (
            "escapes through the cited directory",
            "crates/beta/../../outside.rs",
        ),
        ("absolute", "/etc/passwd"),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "crates/beta/Cargo.toml",
                    "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
                ),
                ("crates/beta/src/lib.rs", "pub fn owned() -> u32 { 1 }\n"),
            ],
        );
        let mut lines: Vec<String> = Vec::new();
        for mut record in scan_fixture(temp.path()) {
            if record["kind"] == "Symbol" {
                record["repo_relative_path"] = Value::from(forged_path);
            }
            lines.push(serde_json::to_string(&record).expect("serialize"));
        }
        let graph_path = temp.path().join("forged.jsonl");
        fs::write(&graph_path, lines.join("\n")).expect("graph written");
        let graph = graph_path.to_str().unwrap();

        let text = run_query(&[
            "query", "symbol", "owned", "--graph", graph, "--format", "text",
        ]);
        assert_eq!(text.code, 0, "`{label}` stderr: {}", text.stderr);
        assert!(
            !text.stdout.contains("package:"),
            "`{label}`: a path the producer cannot emit owns nothing: {:?}",
            text.stdout
        );

        // It is also not scopable under the package it still claims.
        let scoped = run_query(&[
            "query",
            "symbol",
            "owned",
            "--graph",
            graph,
            "--package",
            "beta",
        ]);
        assert_eq!(
            scoped.code, 2,
            "`{label}`: `beta` is real but owns no such row; stdout: {}",
            scoped.stdout
        );
    }
}

/// A corpus where attribution RAN but found no package is not a capability gap.
///
/// `crate_attribution_unavailable` exists for a pre-#117 corpus, and its remedy
/// is "re-scan with a build that records attribution". A freshly scanned repo
/// with no `[package]` manifest — only a virtual workspace, or none at all —
/// carries a present `status: unattributed` on every record: attribution was
/// computed and there is provably no owner. Re-scanning cannot create a package,
/// so telling the operator to re-scan is the same absent-vs-unattributed
/// collapse this feature exists to prevent, at the one surface they act on.
#[test]
fn an_all_unattributed_corpus_is_not_reported_as_a_capability_gap() {
    for (label, files) in [
        (
            "virtual workspace only",
            vec![
                ("Cargo.toml", "[workspace]\nmembers = []\n"),
                ("src/lib.rs", "pub fn orphan() -> u32 { 1 }\n"),
            ],
        ),
        (
            "no manifest at all",
            vec![("src/lib.rs", "pub fn orphan() -> u32 { 1 }\n")],
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(temp.path(), &files);
        let graph_path = temp.path().join("graph.jsonl");
        fs::write(&graph_path, scan_jsonl(temp.path())).expect("graph written");

        // Precondition: attribution really did run and record a reason.
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert!(
            by_path
                .get("src/lib.rs")
                .is_some_and(|set| set.iter().all(|value| value.starts_with("unattributed:"))),
            "`{label}`: fixture must be all-unattributed: {by_path:?}"
        );

        let run = run_query(&[
            "query",
            "symbol",
            "orphan",
            "--graph",
            graph_path.to_str().unwrap(),
            "--package",
            "anything",
        ]);
        assert_eq!(run.code, 1, "`{label}` stdout: {}", run.stdout);
        let diagnostic: Value =
            serde_json::from_str(run.stderr.trim()).expect("one JSON diagnostic line");
        assert_eq!(
            diagnostic["code"], "unknown_package_selector",
            "`{label}`: attribution ran, so this is not a capability gap: {diagnostic}"
        );
        assert_eq!(
            diagnostic["known_packages"]
                .as_array()
                .expect("known_packages")
                .len(),
            0,
            "`{label}`: no package owns anything here"
        );
    }
}

/// A merge deletion must resolve against the parent that REPORTED it.
///
/// `git diff-tree -m` diffs a merge against EACH parent, and `--no-commit-id`
/// discards which one produced a given entry. Resolving every deletion against
/// the FIRST parent is therefore wrong whenever the deleted file existed only
/// on another branch: the first-parent tree has no nested manifest, the walk
/// reaches the outer one, and the `Change` claims the file belonged to a
/// package it never belonged to — the same fabrication the deletion fix exists
/// to prevent, reached by a different route.
#[test]
fn a_merge_deletion_resolves_against_the_parent_that_reported_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    // main: an outer package only.
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn outer_fn() -> u32 { 1 }\n"),
        ],
    );
    commit(&repo, "seed main", "2026-06-01T00:00:00Z");
    let main = String::from_utf8(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .expect("utf-8")
    .trim()
    .to_owned();

    // branch: adds a NESTED package that main never had.
    git(&repo, ["checkout", "-q", "-b", "feature"]);
    write_fixture(
        &repo,
        &[
            (
                "nested/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            ),
            ("nested/src/lib.rs", "pub fn inner_fn() -> u32 { 2 }\n"),
        ],
    );
    commit(&repo, "add the nested package", "2026-06-15T00:00:00Z");

    // Merge into main, resolving by DROPPING the nested source but keeping its
    // manifest. The deletion is then reported only against the second parent.
    git(&repo, ["checkout", "-q", main.as_str()]);
    git(&repo, ["checkout", "-q", "-B", "trunk"]);
    git(&repo, ["merge", "--no-commit", "--no-ff", "-q", "feature"]);
    fs::remove_file(repo.join("nested/src/lib.rs")).expect("drop the nested source");
    let merged = commit(
        &repo,
        "merge feature, dropping the nested source",
        "2026-07-01T00:00:00Z",
    );

    let by_commit = history_attribution(&repo);
    assert_eq!(
        by_commit[&merged].get("nested/src/lib.rs"),
        Some(&BTreeSet::from(["inner@nested/Cargo.toml".to_owned()])),
        "the deletion belongs to `inner`, the package it left — not to the outer \
         package the first-parent tree would reach: {by_commit:?}"
    );
}

/// A commit-pinned ambiguity verdict must ignore records the lane cannot return.
///
/// Since the known-ness/ambiguity split, the ambiguity catalog describes the
/// corpus the ANSWER comes from. `--at` answers from `symbols_at_commit`, which
/// returns only records stamped with the requested commit — so an undated
/// record from a plain-scan repository sharing the package name adds a
/// repository that could never appear in the answer, and turns an answerable
/// query into a false `ambiguous_package_selector`.
///
/// Keeping undated records is still right for the CORPUS-WIDE known-ness
/// catalog, which is what makes the package a real name rather than a typo.
#[test]
fn a_commit_pinned_answer_ignores_undated_records_for_ambiguity() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();

    // Repository A: real history, owns `shared`.
    let repo_a = temp.path().join("repo-a");
    fs::create_dir_all(&repo_a).expect("repo dir");
    init_git(&repo_a);
    write_fixture(
        &repo_a,
        &[
            (
                "crates/shared/Cargo.toml",
                "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/shared/src/lib.rs", "pub fn helper() -> u32 { 1 }\n"),
        ],
    );
    let sha = commit(&repo_a, "seed", "2026-06-01T00:00:00Z");
    combined.push_str(
        &aletheia_egregore::scan_repository_history(&repo_a)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    );
    combined.push('\n');

    // Repository B: a plain scan — no commits, so every record is undated.
    let repo_b = temp.path().join("repo-b");
    fs::create_dir_all(&repo_b).expect("repo dir");
    write_fixture(
        &repo_b,
        &[
            (
                "crates/shared/Cargo.toml",
                "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/shared/src/lib.rs", "pub fn helper() -> u32 { 2 }\n"),
        ],
    );
    combined.push_str(
        &scan_repository_at_with_override(&repo_b, FIXED_TIME, Some("repo-b"))
            .expect("scan")
            .to_jsonl()
            .expect("serialize"),
    );
    combined.push('\n');

    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "helper",
        "--graph",
        graph.to_str().unwrap(),
        "--at",
        &sha,
        "--package",
        "shared",
    ]);
    assert_eq!(
        run.code, 0,
        "only repository A can answer at this commit; stdout: {} stderr: {}",
        run.stdout, run.stderr
    );
    let rows = rows(&run.stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["crate_attribution"]["package_name"], "shared");
    assert_eq!(
        rows[0]["repo_relative_path"], "crates/shared/src/lib.rs",
        "the row must come from the pinned commit"
    );
}

/// DECIDED LIMIT: the read-back checks are LOCAL, and do not establish that the
/// cited manifest is the NEAREST one.
///
/// A record beneath a nested package, edited to cite a valid OUTER `Cargo.toml`,
/// is accepted: the outer directory really does enclose it, and the pairing is
/// one the resolver WOULD produce for a tree where the nested manifest is absent
/// or unusable. Distinguishing the two needs the manifest tree, which a graph
/// does not carry — a dependency-free `Cargo.toml` mints no `File` node, so the
/// only evidence a nested manifest ever existed is the attribution on the
/// records beneath it, which is precisely what an edited graph rewrites.
///
/// The detectable sliver (some sibling still cites the nearer manifest) is an
/// internal-consistency signal, not a boundary — it is evaded by editing those
/// siblings too, or by a package with a single file — and buying it means
/// threading corpus-derived, snapshot-scoped state into per-row consumers whose
/// false positives UN-ATTRIBUTE real records. That failure mode is the one this
/// feature exists to prevent, so the trade is refused deliberately.
///
/// What the checks DO establish is pinned by the tests either side of this one:
/// the value has a shape the resolver emits, and the cited manifest encloses the
/// record. This test exists so the boundary is a decision with a name, not an
/// untested gap.
#[test]
fn an_enclosing_but_not_nearest_manifest_is_accepted_by_the_local_checks() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn outer_fn() -> u32 { 1 }\n"),
            (
                "nested/Cargo.toml",
                "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            ),
            ("nested/src/lib.rs", "pub fn nested_fn() -> u32 { 2 }\n"),
        ],
    );

    // Precondition, and the reason the check cannot be made sound: the nested
    // manifest mints NO record of its own, so the graph's only witness to it is
    // the attribution on the records under it.
    let scanned = scan_fixture(temp.path());
    assert!(
        !scanned
            .iter()
            .any(|record| record["repo_relative_path"] == "nested/Cargo.toml"),
        "a dependency-free manifest mints no node, so `nearest` is unknowable from the graph"
    );

    let mut lines: Vec<String> = Vec::new();
    for mut record in scanned {
        if record["kind"] == "Symbol"
            && record["repo_relative_path"] == "nested/src/lib.rs"
            && let Some(attribution) = record.get_mut("crate_attribution")
        {
            attribution["package_name"] = Value::from("outer");
            attribution["manifest_repo_relative_path"] = Value::from("Cargo.toml");
        }
        lines.push(serde_json::to_string(&record).expect("serialize"));
    }
    let graph_path = temp.path().join("forged.jsonl");
    fs::write(&graph_path, lines.join("\n")).expect("graph written");
    let graph = graph_path.to_str().unwrap();

    // Accepted — the citation is locally consistent, and the corpus holds no
    // proof to the contrary.
    let scoped = run_query(&[
        "query",
        "symbol",
        "nested_fn",
        "--graph",
        graph,
        "--package",
        "outer",
    ]);
    assert_eq!(
        scoped.code, 0,
        "the local checks accept an enclosing citation; stderr: {}",
        scoped.stderr
    );

    // The unforged neighbour is unaffected, which is what makes this a limit on
    // detection rather than a hole in the enclosing rule.
    let sibling = run_query(&[
        "query",
        "symbol",
        "outer_fn",
        "--graph",
        graph,
        "--package",
        "outer",
    ]);
    assert_eq!(sibling.code, 0, "stderr: {}", sibling.stderr);
}

/// A tombstoned record must not contribute an owner to the ambiguity verdict.
///
/// The lanes exclude deleted non-temporal records via `current_deleted_ids`, so
/// a tombstoned symbol can never appear in an answer. Counting it in the
/// ANSWERABLE catalog makes a package look owned by two repositories when only
/// one can produce a row — the same false refusal the corpus-narrowing rounds
/// fixed, reached through liveness instead of time.
#[test]
fn a_tombstoned_record_does_not_make_a_package_ambiguous() {
    use std::fmt::Write as _;
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();

    for (repo_id, tombstone) in [("repo-a", true), ("repo-b", false)] {
        let repo = temp.path().join(repo_id);
        fs::create_dir_all(&repo).expect("repo dir");
        write_fixture(
            &repo,
            &[
                (
                    "crates/util/Cargo.toml",
                    "[package]\nname = \"util\"\nversion = \"0.1.0\"\n",
                ),
                ("crates/util/src/lib.rs", "pub fn helper() -> u32 { 1 }\n"),
            ],
        );
        let records = scan_repository_at_with_override(&repo, FIXED_TIME, Some(repo_id))
            .expect("scan")
            .to_jsonl()
            .expect("serialize");
        combined.push_str(&records);
        combined.push('\n');
        if tombstone {
            // Retract repository A's every attributed record, so only B can
            // answer.
            for record in parse_jsonl(&records) {
                if record["record_type"] == "node"
                    && record["crate_attribution"]["status"] == "attributed"
                {
                    let id = record["id"].as_str().expect("record id");
                    let _ = writeln!(
                        combined,
                        "{{\"record_type\":\"tombstone\",\"id\":\"tombstone:{id}\",\
                         \"schema_version\":{SCHEMA_VERSION},\"deleted_id\":\"{id}\",\
                         \"summary\":\"retracted for test\"}}"
                    );
                }
            }
        }
    }

    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "helper",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "util",
    ]);
    assert_eq!(
        run.code, 0,
        "only the live repository can answer; stdout: {} stderr: {}",
        run.stdout, run.stderr
    );
    let rows = rows(&run.stdout);
    assert_eq!(rows.len(), 1, "one live row");
    assert_eq!(rows[0]["crate_attribution"]["package_name"], "util");
}

/// The NEGATIVE claim needs the record-path gate too.
///
/// A path no scanner can emit makes "provably no owning package" as
/// unbelievable as it makes a positive ownership claim: the resolver never ran
/// over that path, so it proved nothing about it. The positive branch is gated
/// through `manifest_encloses`; the fallback that renders the unattributed
/// reason was not, so a record at `../outside.rs` still printed a
/// resolver-produced fact.
#[test]
fn an_unattributed_reason_on_a_forged_record_path_is_not_rendered() {
    let temp = tempfile::tempdir().expect("temp dir");
    write_fixture(
        temp.path(),
        &[("src/lib.rs", "pub fn orphan() -> u32 { 1 }\n")],
    );
    // No manifest anywhere, so the scan records a legitimate unattributed value.
    let mut lines: Vec<String> = Vec::new();
    let mut saw_reason = false;
    for mut record in scan_fixture(temp.path()) {
        if record["kind"] == "Symbol" {
            saw_reason = record["crate_attribution"]["unattributed_reason"].is_string();
            record["repo_relative_path"] = Value::from("../outside.rs");
        }
        lines.push(serde_json::to_string(&record).expect("serialize"));
    }
    assert!(
        saw_reason,
        "fixture must carry a real unattributed reason before the path is forged"
    );
    let graph_path = temp.path().join("forged.jsonl");
    fs::write(&graph_path, lines.join("\n")).expect("graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "orphan",
        "--graph",
        graph_path.to_str().unwrap(),
        "--format",
        "text",
    ]);
    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(
        !run.stdout.contains("package:"),
        "a forged record path proves nothing, positively or negatively: {:?}",
        run.stdout
    );
}

/// A `[package]` table Cargo REFUSES TO LOAD must not own its subtree.
///
/// The mirror of the `[workspace]` field type-checking: a wrong-typed known
/// field makes the WHOLE manifest unloadable, so the package it names does not
/// exist for Cargo, and attributing a subtree to it — and making the name
/// selectable via `--package` — claims ownership by a package that cannot be
/// built.
///
/// The accepted cases matter as much as the rejected ones. Workspace
/// INHERITANCE (`version.workspace = true`) is a TABLE and is ubiquitous in
/// real workspaces, so a naive "version must be a string" rule would
/// un-attribute them wholesale. Every arm below was verified against real
/// `cargo metadata --no-deps --format-version 1` on the pinned toolchain.
#[test]
fn malformed_package_fields_do_not_own_their_subtree() {
    for (label, package, expected) in [
        (
            "version = int",
            "[package]\nname = \"inner\"\nversion = 1\n",
            "unattributed:unusable_manifest",
        ),
        (
            "edition = int",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nedition = 1\n",
            "unattributed:unusable_manifest",
        ),
        (
            "authors = string",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nauthors = \"solo\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "keywords element = int",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nkeywords = [1]\n",
            "unattributed:unusable_manifest",
        ),
        (
            "links = int",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nlinks = 1\n",
            "unattributed:unusable_manifest",
        ),
        // Cargo ACCEPTS all of these, so they must keep owning the subtree.
        (
            "version inherited from the workspace",
            "[package]\nname = \"inner\"\nversion.workspace = true\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "version inherited, inline table",
            "[package]\nname = \"inner\"\nversion = { workspace = true }\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "readme = false",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nreadme = false\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "publish = false",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\npublish = false\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "build = false",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nbuild = false\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "unknown key tolerated",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nfuture-key = \"x\"\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "control",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            "inner@nested/Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                ("nested/Cargo.toml", package),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`: {by_path:?}"
        );
    }
}

/// DECIDED: `--package` must AGREE with the unscoped `--as-of` answer, including
/// where that answer is itself debatable.
///
/// A symbol deleted outright, with no same-named replacement, still resolves to
/// its final pre-deletion version — because `--as-of` means "the newest version
/// at or before this instant" and history replay never tombstones a removed
/// symbol, it simply stops re-emitting it. That is the lane's PRE-EXISTING
/// contract, shared by the unscoped and `--repo` paths.
///
/// Issue #117's job is to make the SCOPED answer agree with the snapshot the
/// unscoped lane already chooses, not to redefine what `--as-of` means. Making
/// the scoped lane alone drop the row would put the two answers in conflict:
/// `--package alpha` would report no match for a row `eg query symbol --as-of`
/// hands back unscoped. Changing the deletion semantics is a change to a shipped
/// lane's contract and belongs in its own issue, evaluated across every lane
/// that shares the resolver.
#[test]
fn package_scope_as_of_agrees_with_the_unscoped_answer_after_a_deletion() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    write_fixture(
        &repo,
        &[
            (
                "crates/alpha/Cargo.toml",
                "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\n",
            ),
            ("crates/alpha/src/lib.rs", "pub fn doomed() -> u32 { 1 }\n"),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");
    fs::remove_file(repo.join("crates/alpha/src/lib.rs")).expect("delete");
    fs::write(
        repo.join("crates/alpha/src/other.rs"),
        "pub fn survivor() -> u32 { 2 }\n",
    )
    .expect("write");
    commit(&repo, "delete doomed", "2026-07-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");
    let graph = graph_path.to_str().unwrap();
    let after = "2026-08-01T00:00:00Z";

    let unscoped = run_query(&[
        "query", "symbol", "doomed", "--graph", graph, "--as-of", after,
    ]);
    let scoped = run_query(&[
        "query",
        "symbol",
        "doomed",
        "--graph",
        graph,
        "--as-of",
        after,
        "--package",
        "alpha",
    ]);

    assert_eq!(
        unscoped.code, scoped.code,
        "scoped and unscoped must agree; unscoped: {} scoped: {}",
        unscoped.stdout, scoped.stdout
    );
    let unscoped_rows = rows(&unscoped.stdout);
    let scoped_rows = rows(&scoped.stdout);
    assert_eq!(unscoped_rows.len(), scoped_rows.len());
    for (u, s) in unscoped_rows.iter().zip(scoped_rows.iter()) {
        assert_eq!(
            u["record_id"], s["record_id"],
            "the scoped answer must be a subsequence of the unscoped one"
        );
    }
}

/// Cargo's automatic-target `auto*` fields are bool-only, and NOT inheritable.
///
/// They were missing from the package shape table, so a wrong-typed one read as
/// an unknown key and the manifest stayed a usable package — attributing the
/// subtree to a package Cargo cannot load. Unlike every other checked field
/// these take no `x.workspace = true` form: Cargo rejects a table with
/// "invalid type: map, expected a boolean", verified alongside the rest.
#[test]
fn malformed_auto_target_fields_do_not_own_their_subtree() {
    for field in [
        "autolib",
        "autobins",
        "autoexamples",
        "autotests",
        "autobenches",
    ] {
        for (label, value, expected) in [
            ("string", "\"bad\"", "unattributed:unusable_manifest"),
            // Not inheritable: a table is a type error here, unlike `version`.
            (
                "inheritance table",
                "{ workspace = true }",
                "unattributed:unusable_manifest",
            ),
            ("bool", "true", "inner@nested/Cargo.toml"),
        ] {
            let temp = tempfile::tempdir().expect("temp dir");
            write_fixture(
                temp.path(),
                &[
                    (
                        "Cargo.toml",
                        "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                    ),
                    (
                        "nested/Cargo.toml",
                        &format!(
                            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n{field} = {value}\n"
                        ),
                    ),
                    ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
                ],
            );
            let by_path = attribution_by_path(&scan_fixture(temp.path()));
            assert_eq!(
                by_path.get("nested/src/lib.rs"),
                Some(&BTreeSet::from([expected.to_owned()])),
                "`{field} = {value}` ({label}): {by_path:?}"
            );
        }
    }
}

/// An ALL-TOMBSTONED corpus still ran attribution, so it is not a capability gap.
///
/// The liveness skip must not swallow the field-presence observation: a store
/// whose every attributed record has been retracted does carry current-schema
/// attribution, and no re-scan can restore a deleted package. Reporting
/// `crate_attribution_unavailable` there re-opens the absent-vs-unattributed
/// collapse that the ownerless-corpus fix closed, just through liveness instead
/// of an empty manifest tree.
#[test]
fn an_all_tombstoned_corpus_is_not_reported_as_a_capability_gap() {
    use std::fmt::Write as _;

    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"gone\"\nversion = \"0.1.0\"\n",
            ),
            ("src/lib.rs", "pub fn orphan() -> u32 { 1 }\n"),
        ],
    );
    let scanned = scan_repository_at_with_override(&repo, FIXED_TIME, Some("repo"))
        .expect("scan")
        .to_jsonl()
        .expect("serialize");
    let mut combined = scanned.clone();
    combined.push('\n');
    for record in parse_jsonl(&scanned) {
        if record["record_type"] == "node" && record["crate_attribution"].is_object() {
            let id = record["id"].as_str().expect("record id");
            let _ = writeln!(
                combined,
                "{{\"record_type\":\"tombstone\",\"id\":\"tombstone:{id}\",\
                 \"schema_version\":{SCHEMA_VERSION},\"deleted_id\":\"{id}\",\
                 \"summary\":\"retracted for test\"}}"
            );
        }
    }
    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "orphan",
        "--graph",
        graph.to_str().unwrap(),
        "--package",
        "gone",
    ]);
    assert_eq!(run.code, 1, "stdout: {}", run.stdout);
    let diagnostic: Value =
        serde_json::from_str(run.stderr.trim()).expect("one JSON diagnostic line");
    assert_eq!(
        diagnostic["code"], "unknown_package_selector",
        "attribution ran here; a re-scan cannot restore a retracted package: {diagnostic}"
    );
}

/// The `[package].workspace` POINTER is a string, and not inheritable.
///
/// It names the workspace root a package belongs to (`workspace = "../.."`).
/// Cargo rejects a non-string with "invalid type: integer, expected a string",
/// so a manifest carrying one cannot be loaded and must not own its subtree.
/// Distinct from the `x.workspace = true` INHERITANCE form, which appears as a
/// table INSIDE another field — this is a top-level `[package]` key.
#[test]
fn a_malformed_package_workspace_pointer_does_not_own_its_subtree() {
    for (label, value, expected) in [
        ("int", "1", "unattributed:unusable_manifest"),
        ("bool", "true", "unattributed:unusable_manifest"),
        ("string", "\"..\"", "inner@nested/Cargo.toml"),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "nested/Cargo.toml",
                    &format!(
                        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\nworkspace = {value}\n"
                    ),
                ),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`workspace = {value}` ({label}): {by_path:?}"
        );
    }
}

/// A malformed `--as-of` must report ITS OWN error, not a package verdict.
///
/// Package validation runs before the lane parses the timestamp, so with an
/// ambiguous package name the caller was told to disambiguate a selector while
/// the actual problem — an unparseable instant — went unmentioned. The
/// timestamp is the input that cannot be interpreted at all, so it must be
/// diagnosed first; a package verdict computed over a corpus that could not be
/// narrowed is not meaningful anyway.
#[test]
fn a_malformed_as_of_reports_the_timestamp_not_the_package() {
    let temp = tempfile::tempdir().expect("temp dir");
    let mut combined = String::new();
    // Two repositories owning the same package name, so an unguarded package
    // validation would exit 1 with `ambiguous_package_selector`.
    for repo_id in ["repo-a", "repo-b"] {
        let repo = temp.path().join(repo_id);
        fs::create_dir_all(&repo).expect("repo dir");
        write_fixture(
            &repo,
            &[
                (
                    "crates/shared/Cargo.toml",
                    "[package]\nname = \"shared\"\nversion = \"0.1.0\"\n",
                ),
                ("crates/shared/src/lib.rs", "pub fn helper() -> u32 { 1 }\n"),
            ],
        );
        combined.push_str(
            &scan_repository_at_with_override(&repo, FIXED_TIME, Some(repo_id))
                .expect("scan")
                .to_jsonl()
                .expect("serialize"),
        );
        combined.push('\n');
    }
    let graph = temp.path().join("combined.jsonl");
    fs::write(&graph, &combined).expect("graph written");

    let run = run_query(&[
        "query",
        "symbol",
        "helper",
        "--graph",
        graph.to_str().unwrap(),
        "--as-of",
        "not-a-timestamp",
        "--package",
        "shared",
    ]);
    assert!(
        !run.stderr.contains("ambiguous_package_selector"),
        "the unparseable instant must be diagnosed, not the package: {}",
        run.stderr
    );
    assert!(
        run.stderr.contains("not-a-timestamp"),
        "the diagnostic must name the bad timestamp: {}",
        run.stderr
    );
}

/// DECIDED: a wrong-typed TOP-LEVEL section does not disqualify a manifest,
/// because Cargo accepts it.
///
/// Verified on the pinned toolchain (cargo 1.94.1): `lib = 1` beside a valid
/// `[package]` loads cleanly under both `cargo metadata --no-deps
/// --format-version 1` and `cargo build`. Only a well-formed `[lib]` TABLE with
/// a bad inner value is rejected, which is value-level validation this resolver
/// does not reimplement.
///
/// Rejecting these would un-attribute crates Cargo builds happily — the
/// over-rejection failure this feature guards against — so the manifest stays a
/// usable package and keeps owning its subtree.
#[test]
fn a_wrong_typed_top_level_section_still_owns_its_subtree() {
    for section in [
        "lib",
        "bin",
        "features",
        "dependencies",
        "profile",
        "badges",
        "target",
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "nested/Cargo.toml",
                    &format!("[package]\nname = \"inner\"\nversion = \"0.1.0\"\n{section} = 1\n"),
                ),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from(["inner@nested/Cargo.toml".to_owned()])),
            "`{section} = 1` is accepted by Cargo, so it must keep owning: {by_path:?}"
        );
    }
}

/// Top-level `cargo-features` is type-checked by Cargo, unlike `lib = 1`.
///
/// It must be an array of strings, and the requirement applies to BOTH manifest
/// shapes — a virtual workspace root with `cargo-features = 1` is equally
/// unloadable, and must stop the walk rather than be passed over. Verified
/// against real `cargo metadata --no-deps --format-version 1` on the pinned
/// toolchain, which is also what separates this from the tolerated top-level
/// sections: `lib = 1` loads, `cargo-features = 1` does not.
///
/// An unknown feature NAME (`cargo-features = ["totally-made-up"]`) is also a
/// Cargo error, but that is VALUE-level validation — the documented bound this
/// resolver does not cross — so it stays accepted here.
#[test]
fn a_malformed_cargo_features_stops_the_walk() {
    for (label, prefix, nested_body, expected) in [
        (
            "package, int",
            "cargo-features = 1\n",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "package, int element",
            "cargo-features = [1]\n",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "virtual root, int",
            "cargo-features = 1\n",
            "[workspace]\nmembers = []\n",
            "unattributed:unusable_manifest",
        ),
        (
            "package, empty array",
            "cargo-features = []\n",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "package, unknown feature name is VALUE-level",
            "cargo-features = [\"totally-made-up\"]\n",
            "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "virtual root, empty array walks past to outer",
            "cargo-features = []\n",
            "[workspace]\nmembers = []\n",
            "outer@Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                ("nested/Cargo.toml", &format!("{prefix}{nested_body}")),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`: {by_path:?}"
        );
    }
}

/// The JSON row must carry no attribution the checked accessors refuse.
///
/// JSON is the machine-facing contract, so it is where a fabricated ownership
/// claim does the most damage: an agent consuming the row has no reason to
/// re-derive the enclosure rule, and will simply believe the package name. It
/// previously echoed the stored payload verbatim on the reasoning that a
/// malformed shape should stay VISIBLE to a machine reader — but that left the
/// two output formats disagreeing about the same record, with the safe answer
/// only on the human-facing one.
///
/// The row itself is still returned; only the unbelievable CLAIM is dropped, so
/// the field reads as absent — attribution UNKNOWN — which is exactly what it
/// is. An operator inspecting a corrupt store still has `eg export` and the raw
/// JSONL, which are the stored bytes rather than a derived answer.
#[test]
fn a_forged_attribution_is_absent_from_the_json_row_too() {
    type Forge = fn(&mut Value);
    let forgeries: [(&str, Forge); 3] = [
        ("manifest does not enclose the record", |attribution| {
            attribution["package_name"] = Value::from("alpha");
            attribution["manifest_repo_relative_path"] = Value::from("crates/alpha/Cargo.toml");
        }),
        (
            "attributed while carrying an unattributed reason",
            |attribution| {
                attribution["unattributed_reason"] = Value::from("no_enclosing_manifest");
            },
        ),
        ("Cargo-invalid package name", |attribution| {
            attribution["package_name"] = Value::from("bad name");
        }),
    ];
    for (label, mutate) in forgeries {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "crates/beta/Cargo.toml",
                    "[package]\nname = \"beta\"\nversion = \"0.1.0\"\n",
                ),
                ("crates/beta/src/lib.rs", "pub fn owned() -> u32 { 1 }\n"),
            ],
        );
        let mut lines: Vec<String> = Vec::new();
        for mut record in scan_fixture(temp.path()) {
            if record["kind"] == "Symbol"
                && let Some(attribution) = record.get_mut("crate_attribution")
            {
                mutate(attribution);
            }
            lines.push(serde_json::to_string(&record).expect("serialize"));
        }
        let graph_path = temp.path().join("forged.jsonl");
        fs::write(&graph_path, lines.join("\n")).expect("graph written");
        let graph = graph_path.to_str().unwrap();

        for lane in [
            vec!["query", "symbol", "owned", "--graph", graph],
            vec!["query", "file", "crates/beta/src/lib.rs", "--graph", graph],
        ] {
            let run = run_query(&lane);
            assert_eq!(run.code, 0, "`{label}` {lane:?} stderr: {}", run.stderr);
            for row in rows(&run.stdout) {
                if row["name"] != "owned" {
                    continue;
                }
                assert!(
                    row.get("crate_attribution").is_none() || row["crate_attribution"].is_null(),
                    "`{label}` {lane:?}: a claim the text path refuses must not \
                     ship in JSON: {row}"
                );
            }
        }
    }
}

/// A COMBINED `[package]` + `[workspace]` root must validate BOTH tables.
///
/// This layout — a root crate that is also the workspace root — is common in
/// real repositories, and it takes the package branch, which checked only the
/// package table. A malformed `[workspace]` beside a valid `[package]` makes
/// the whole manifest unloadable, so the package it names does not exist and
/// cannot own the subtree. Verified against real `cargo metadata`.
#[test]
fn a_combined_package_workspace_root_validates_both_tables() {
    for (label, root, expected) in [
        (
            "workspace members malformed",
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n[workspace]\nmembers = \"not-an-array\"\n",
            "unattributed:unusable_manifest",
        ),
        (
            "workspace resolver malformed",
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n[workspace]\nmembers = []\nresolver = 2\n",
            "unattributed:unusable_manifest",
        ),
        (
            "both tables well-typed",
            "[package]\nname = \"root\"\nversion = \"0.1.0\"\n\n[workspace]\nmembers = []\n",
            "root@Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                ("Cargo.toml", root),
                ("src/lib.rs", "pub fn at_root() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`: {by_path:?}"
        );
    }
}

/// A dependency entry Cargo cannot interpret makes the manifest unloadable.
///
/// The parser ALREADY detects this — `declared_dependency` returns `None` for
/// exactly the forms Cargo rejects (non-string/non-table entry, a table naming
/// no usable source, a wrong-typed known key, an unparseable requirement, an
/// invalid name) and the caller records it — but the shape reduction ignored the
/// flag and still returned `Package`. Reusing the existing signal costs no new
/// field list and cannot drift from the dependency rules it already encodes.
#[test]
fn an_uninterpretable_dependency_entry_stops_the_walk() {
    for (label, deps, expected) in [
        (
            "bool entry",
            "serde = true\n",
            "unattributed:unusable_manifest",
        ),
        ("int entry", "serde = 1\n", "unattributed:unusable_manifest"),
        (
            "table naming no source",
            "serde = { features = [\"derive\"] }\n",
            "unattributed:unusable_manifest",
        ),
        // Every legitimate form must keep owning the subtree.
        (
            "version string",
            "serde = \"1\"\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "path source",
            "serde = { path = \"../serde\" }\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "workspace inheritance",
            "serde = { workspace = true }\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "version plus features",
            "serde = { version = \"1\", features = [\"derive\"] }\n",
            "inner@nested/Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "nested/Cargo.toml",
                    &format!(
                        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n\n[dependencies]\n{deps}"
                    ),
                ),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`: {by_path:?}"
        );
    }
}

/// A TARGET-SPECIFIC dependency entry counts toward manifest usability too.
///
/// `[target.'cfg(unix)'.dependencies]` and its dev/build siblings are the only
/// other place dependencies live, so covering them CLOSES the set rather than
/// adding one more case. Cargo rejects a malformed entry there exactly as it
/// does at the top level, and an unloadable manifest declares no package.
///
/// Emitting rows for target-specific dependencies stays out of scope — this is
/// about whether the manifest LOADS, which is what attribution rests on.
#[test]
fn a_malformed_target_specific_dependency_stops_the_walk() {
    for (label, table, entry, expected) in [
        (
            "cfg dependencies",
            "target.\"cfg(unix)\".dependencies",
            "serde = true",
            "unattributed:unusable_manifest",
        ),
        (
            "cfg dev-dependencies",
            "target.\"cfg(unix)\".dev-dependencies",
            "serde = true",
            "unattributed:unusable_manifest",
        ),
        (
            "cfg build-dependencies",
            "target.\"cfg(unix)\".build-dependencies",
            "serde = true",
            "unattributed:unusable_manifest",
        ),
        (
            "named triple",
            "target.x86_64-unknown-linux-gnu.dependencies",
            "serde = true",
            "unattributed:unusable_manifest",
        ),
        // Valid target-specific entries must keep owning the subtree.
        (
            "cfg dependencies, version string",
            "target.\"cfg(unix)\".dependencies",
            "serde = \"1\"",
            "inner@nested/Cargo.toml",
        ),
        (
            "cfg dependencies, path source",
            "target.\"cfg(unix)\".dependencies",
            "serde = { path = \"../serde\" }",
            "inner@nested/Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "nested/Cargo.toml",
                    &format!(
                        "[package]\nname = \"inner\"\nversion = \"0.1.0\"\n\n[{table}]\n{entry}\n"
                    ),
                ),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`: {by_path:?}"
        );
    }
}

/// An all-`Change` corpus still ran attribution, so it is not a capability gap.
///
/// The sibling of the all-tombstoned case: the `Change` skip also ran before
/// the field-presence observation. A history corpus whose only attributed
/// records are `Change` nodes — a repository whose commits touch a
/// dependency-free `Cargo.toml` and no supported source files — reported
/// `crate_attribution_unavailable` and told the operator to re-scan, though the
/// current producer demonstrably ran attribution.
///
/// The observation now happens before EVERY skip, so a future exclusion cannot
/// re-open this by adding another `continue`.
#[test]
fn an_all_change_corpus_is_not_reported_as_a_capability_gap() {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");
    init_git(&repo);
    // A dependency-free manifest mints no `File` node, and `.md` is not an
    // indexed source — so the only attributed records are the `Change` nodes.
    write_fixture(
        &repo,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"lonely\"\nversion = \"0.1.0\"\n",
            ),
            ("README.md", "# nothing indexed here\n"),
        ],
    );
    commit(&repo, "seed", "2026-06-01T00:00:00Z");

    let graph_path = temp.path().join("history.jsonl");
    fs::write(
        &graph_path,
        aletheia_egregore::scan_repository_history(&repo)
            .expect("history replay")
            .to_jsonl()
            .expect("serialize"),
    )
    .expect("graph written");

    // Precondition: attribution exists, and ONLY on `Change` records.
    let records = parse_jsonl(&fs::read_to_string(&graph_path).expect("read"));
    let attributed_kinds: BTreeSet<&str> = records
        .iter()
        .filter(|r| r["crate_attribution"].is_object())
        .filter_map(|r| r["kind"].as_str())
        .collect();
    assert_eq!(
        attributed_kinds,
        BTreeSet::from(["Change"]),
        "fixture must carry attribution only on Change records: {attributed_kinds:?}"
    );

    let run = run_query(&[
        "query",
        "symbol",
        "anything",
        "--graph",
        graph_path.to_str().unwrap(),
        "--package",
        "lonely",
    ]);
    assert_eq!(run.code, 1, "stdout: {}", run.stdout);
    let diagnostic: Value =
        serde_json::from_str(run.stderr.trim()).expect("one JSON diagnostic line");
    assert_eq!(
        diagnostic["code"], "unknown_package_selector",
        "attribution ran here, so this is not the pre-#117 gap: {diagnostic}"
    );
}

/// Target-specific dependency TABLES are type-checked; top-level ones are not.
///
/// A genuine Cargo asymmetry, verified on the pinned toolchain rather than
/// assumed symmetric:
///
/// - top-level `dependencies = 1` beside a valid `[package]` LOADS (Cargo
///   tolerates the scalar), so rejecting it would un-attribute a real crate;
/// - `[target."cfg(unix)"] dependencies = 1` is REJECTED ("expected a map"),
///   as is a target SPEC that is not a table ("expected struct `TomlPlatform`").
///
/// The two loops therefore cannot be unified, and this test exists to make that
/// concrete for anyone tempted to.
#[test]
fn target_dependency_tables_are_typed_but_top_level_ones_are_tolerated() {
    for (label, nested_extra, expected) in [
        (
            "target dependencies = 1",
            "\n[target.\"cfg(unix)\"]\ndependencies = 1\n",
            "unattributed:unusable_manifest",
        ),
        (
            "target dev-dependencies = 1",
            "\n[target.\"cfg(unix)\"]\ndev-dependencies = 1\n",
            "unattributed:unusable_manifest",
        ),
        (
            "target build-dependencies = 1",
            "\n[target.\"cfg(unix)\"]\nbuild-dependencies = 1\n",
            "unattributed:unusable_manifest",
        ),
        (
            "target spec is not a table",
            "\n[target]\n\"cfg(unix)\" = 1\n",
            "unattributed:unusable_manifest",
        ),
        // Cargo ACCEPTS these, so they must keep owning the subtree.
        (
            "top-level dependencies = 1 is tolerated",
            "dependencies = 1\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "top-level dev-dependencies = 1 is tolerated",
            "dev-dependencies = 1\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "unknown key under a target spec is tolerated",
            "\n[target.\"cfg(unix)\"]\nfuture-key = 1\n",
            "inner@nested/Cargo.toml",
        ),
        (
            "well-formed target dependencies",
            "\n[target.\"cfg(unix)\".dependencies]\nserde = \"1\"\n",
            "inner@nested/Cargo.toml",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp dir");
        write_fixture(
            temp.path(),
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"outer\"\nversion = \"0.1.0\"\n",
                ),
                (
                    "nested/Cargo.toml",
                    &format!("[package]\nname = \"inner\"\nversion = \"0.1.0\"\n{nested_extra}"),
                ),
                ("nested/src/lib.rs", "pub fn nested() -> u32 { 1 }\n"),
            ],
        );
        let by_path = attribution_by_path(&scan_fixture(temp.path()));
        assert_eq!(
            by_path.get("nested/src/lib.rs"),
            Some(&BTreeSet::from([expected.to_owned()])),
            "`{label}`: {by_path:?}"
        );
    }
}
