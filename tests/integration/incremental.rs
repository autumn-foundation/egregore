#![allow(missing_docs)]

use std::fs;

use aletheia_egregore::incremental::{scan_repository_incremental, scan_repository_incremental_at};
use aletheia_egregore::{EdgeLabel, GraphRecord, NodeKind, SCHEMA_VERSION, SourceSpan, stable_id};

#[test]
fn incremental_reuses_unchanged_files_and_tombstones_removed_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    let lib = src.join("lib.rs");
    fs::write(&lib, "pub fn answer() -> usize { 42 }\n").expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    let tx_time = "2026-05-19T00:00:00Z";
    let first = scan_repository_incremental_at(&repo, &cache_path, tx_time)
        .expect("first scan should work");
    assert_eq!(first.rebuilt_files, ["src/lib.rs"]);
    assert!(first.reused_files.is_empty());
    assert!(first.tombstoned_files.is_empty());

    let unchanged = scan_repository_incremental_at(&repo, &cache_path, tx_time)
        .expect("unchanged scan should work");
    assert_eq!(unchanged.reused_files, ["src/lib.rs"]);
    assert!(unchanged.rebuilt_files.is_empty());
    assert!(unchanged.tombstoned_files.is_empty());
    assert_eq!(
        first.graph.to_jsonl().expect("first graph JSONL"),
        unchanged.graph.to_jsonl().expect("unchanged graph JSONL")
    );

    fs::write(
        &lib,
        "pub fn answer() -> usize { helper() }\nfn helper() -> usize { 7 }\n",
    )
    .expect("fixture should update");
    let changed =
        scan_repository_incremental(&repo, &cache_path).expect("changed scan should work");
    assert_eq!(changed.rebuilt_files, ["src/lib.rs"]);
    assert!(changed.reused_files.is_empty());
    assert!(changed.tombstoned_files.is_empty());

    fs::remove_file(&lib).expect("fixture should delete source");
    let removed =
        scan_repository_incremental(&repo, &cache_path).expect("removed scan should work");
    assert!(removed.rebuilt_files.is_empty());
    assert!(removed.reused_files.is_empty());
    assert_eq!(removed.tombstoned_files, ["src/lib.rs"]);
    assert!(
        removed
            .graph
            .to_jsonl()
            .expect("removed graph JSONL")
            .contains(r#""record_type":"tombstone""#)
    );
}

#[test]
fn incremental_ignores_old_cache_when_extractor_output_schema_changes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    let foo = src.join("foo.rs");
    fs::write(&foo, "pub fn bar() -> usize { 42 }\n").expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");
    let hash = blake3::hash(&fs::read(&foo).expect("fixture should read"))
        .to_hex()
        .to_string();
    let stale_symbol = GraphRecord::symbol(
        stable_id(&["node", "symbol", "function", "src/foo.rs", "bar", "0"]),
        "function",
        "src/foo.rs".to_owned(),
        SourceSpan {
            start_byte: 0,
            end_byte: 28,
            start_line: 1,
            end_line: 1,
            start_column: None,
            end_column: None,
        },
        "bar".to_owned(),
        "Rust function bar".to_owned(),
    );
    let stale_symbol_id = stale_symbol.id().to_owned();
    let stale_cache = serde_json::json!({
        "schema_version": 1,
        "files": {
            "src/foo.rs": {
                "hash": hash,
                "records": [stale_symbol],
            },
        },
    });
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&stale_cache).expect("cache should serialize"),
    )
    .expect("fixture should write cache");

    let scan = scan_repository_incremental(&repo, &cache_path)
        .expect("incremental scan should ignore old cache schema");

    assert_eq!(scan.rebuilt_files, ["src/foo.rs"]);
    assert!(scan.reused_files.is_empty());
    assert!(
        scan.graph.records().iter().any(|record| matches!(
            record,
            GraphRecord::Node {
                name: Some(name),
                ..
            } if name == "foo::bar"
        )),
        "split-module file should be rebuilt with qualified symbol names"
    );
    assert!(
        scan.graph.records().iter().all(|record| !matches!(
            record,
            GraphRecord::Node {
                name: Some(name),
                ..
            } if name == "bar"
        )),
        "stale unqualified cached symbol must not survive cache schema invalidation"
    );
    assert!(
        scan.graph.records().iter().any(|record| matches!(
            record,
            GraphRecord::Tombstone {
                deleted_id,
                ..
            } if deleted_id == &stale_symbol_id
        )),
        "invalidated cached symbol IDs must be tombstoned so persisted stores can retire stale records"
    );
}

#[test]
fn incremental_rebuilds_when_cache_record_schema_version_is_unknown() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    fs::write(src.join("lib.rs"), "pub fn answer() -> usize { 42 }\n")
        .expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    scan_repository_incremental(&repo, &cache_path).expect("first scan should write cache");
    let mut cache_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).expect("cache should be readable"))
            .expect("cache should parse");
    cache_json["files"]["src/lib.rs"]["records"][0]["schema_version"] =
        serde_json::json!(SCHEMA_VERSION + 1);
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&cache_json).expect("cache should serialize"),
    )
    .expect("fixture should write future-version cache");

    let scan = scan_repository_incremental(&repo, &cache_path)
        .expect("future cache record version should degrade to a rebuild");

    assert_eq!(scan.rebuilt_files, ["src/lib.rs"]);
    assert!(scan.reused_files.is_empty());
}

#[test]
fn incremental_rebuilds_when_cache_producer_signature_differs() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    fs::write(src.join("lib.rs"), "pub fn answer() -> usize { 42 }\n")
        .expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    scan_repository_incremental(&repo, &cache_path).expect("first scan should write cache");
    // Simulate a cache written by a binary with an older Rust grammar: if
    // reuse survived this, unchanged files would be re-stamped with the
    // running binary's producer envelope and `eg query producer-drift`
    // would report them as current — a false negative (issue #234).
    let mut cache_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).expect("cache should be readable"))
            .expect("cache should parse");
    cache_json["producer_components"]["tree_sitter_rust"] =
        serde_json::json!("0.0.0-superseded-grammar");
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&cache_json).expect("cache should serialize"),
    )
    .expect("fixture should write tampered cache");

    let scan = scan_repository_incremental(&repo, &cache_path)
        .expect("producer signature mismatch should degrade to a rebuild");
    assert_eq!(scan.rebuilt_files, ["src/lib.rs"]);
    assert!(scan.reused_files.is_empty());

    // A rescan with the (now re-written) matching signature reuses again.
    let rescan = scan_repository_incremental(&repo, &cache_path)
        .expect("matching producer signature should reuse the cache");
    assert_eq!(rescan.reused_files, ["src/lib.rs"]);
    assert!(rescan.rebuilt_files.is_empty());
}

#[test]
fn incremental_rebuilds_when_cache_predates_producer_signature() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    fs::write(src.join("lib.rs"), "pub fn answer() -> usize { 42 }\n")
        .expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    scan_repository_incremental(&repo, &cache_path).expect("first scan should write cache");
    // A cache written before the producer signature existed carries no
    // signature fields; the writing binary is unknown, so reuse must not
    // vouch for its producer identity.
    let mut cache_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).expect("cache should be readable"))
            .expect("cache should parse");
    let removed_version = cache_json
        .as_object_mut()
        .expect("cache should be a JSON object")
        .remove("producer_egregore_version");
    let removed_components = cache_json
        .as_object_mut()
        .expect("cache should be a JSON object")
        .remove("producer_components");
    assert!(
        removed_version.is_some() && removed_components.is_some(),
        "current caches must record the writing binary's producer signature"
    );
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&cache_json).expect("cache should serialize"),
    )
    .expect("fixture should write signature-less cache");

    let scan = scan_repository_incremental(&repo, &cache_path)
        .expect("signature-less cache should degrade to a rebuild");
    assert_eq!(scan.rebuilt_files, ["src/lib.rs"]);
    assert!(scan.reused_files.is_empty());
}

#[test]
fn incremental_tombstones_stale_cross_file_records_when_identity_changes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    fs::write(src.join("alpha.rs"), "pub fn provide() -> usize { 1 }\n")
        .expect("fixture should write");
    fs::write(
        src.join("beta.rs"),
        "pub fn consume() -> usize { provide() }\n",
    )
    .expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    // Establish a real cache (with real cross_file_record_ids) via a first scan.
    scan_repository_incremental(&repo, &cache_path).expect("first scan should work");

    // Simulate a cache persisted under a previous repository identity: rewrite the
    // cached repository_id and record the cross-file records that identity emitted.
    // Their IDs embed the old repository_id (e.g. unresolved-call diagnostics), so
    // the current identity's recomputed pass will never re-emit them.
    let old_repo_id = stable_id(&[
        "repository",
        "local-path",
        "/old/path/that/no/longer/exists",
    ]);
    let stale_cross_file_id = stable_id(&[
        "node",
        "diagnostic",
        "unresolved-call",
        &old_repo_id,
        "src/beta.rs",
        "provide",
    ]);
    let mut cache_json: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&cache_path).expect("cache should be readable"))
            .expect("cache should parse");
    cache_json["repository_id"] = serde_json::json!(old_repo_id);
    cache_json["cross_file_record_ids"] = serde_json::json!([stale_cross_file_id]);
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&cache_json).expect("cache should serialize"),
    )
    .expect("fixture should write stale cache");

    let second =
        scan_repository_incremental(&repo, &cache_path).expect("second scan after id change");

    assert!(
        second.graph.records().iter().any(|record| matches!(
            record,
            GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &stale_cross_file_id
        )),
        "cross-file records from a previous repository identity must be tombstoned \
         even though cache reuse is disabled by the identity mismatch"
    );
}

#[test]
fn incremental_tombstones_stale_repository_when_identity_changes() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    fs::write(src.join("lib.rs"), "pub fn answer() -> usize { 42 }\n")
        .expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    // Establish the current repository_id by doing a first scan.
    let first = scan_repository_incremental(&repo, &cache_path).expect("first scan should work");
    let actual_repo_id = first
        .graph
        .records()
        .iter()
        .find_map(|r| {
            if let GraphRecord::Node {
                kind: NodeKind::Repository,
                id,
                ..
            } = r
            {
                Some(id.clone())
            } else {
                None
            }
        })
        .expect("first scan should contain a Repository node");

    // Inject a stale cache with a different repository_id at the same schema version.
    let old_repo_id = stable_id(&[
        "repository",
        "local-path",
        "/old/path/that/no/longer/exists",
    ]);
    assert_ne!(
        old_repo_id, actual_repo_id,
        "old and new repo IDs must differ"
    );
    let stale_cache = serde_json::json!({
        "schema_version": 4,
        "repository_id": old_repo_id,
        "files": {},
    });
    fs::write(
        &cache_path,
        serde_json::to_string_pretty(&stale_cache).expect("stale cache should serialize"),
    )
    .expect("fixture should write stale cache");

    let second =
        scan_repository_incremental(&repo, &cache_path).expect("second scan after id change");

    assert!(
        second.graph.records().iter().any(|record| matches!(
            record,
            GraphRecord::Tombstone { deleted_id, .. } if deleted_id == &old_repo_id
        )),
        "must emit a tombstone for the stale Repository node when identity changes"
    );
}

/// Issue #444: the import-target pass folds into the same recomputed
/// cross-file record stream as CALLS/IMPLEMENTS, so removing an import must
/// tombstone its `File —IMPORTS→ Module|File` edge on the next incremental
/// scan instead of leaving it live.
#[test]
fn incremental_tombstones_stale_import_target_edges() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    fs::write(src.join("lib.rs"), "pub mod b;\n").expect("fixture should write");
    fs::write(src.join("b.rs"), "pub fn f() {}\n").expect("fixture should write");
    let consumer = src.join("consumer.rs");
    fs::write(&consumer, "use crate::b;\n").expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    let first = scan_repository_incremental(&repo, &cache_path).expect("first scan should work");
    let first_target_edges = import_target_edge_ids(first.graph.records());
    assert_eq!(
        first_target_edges.len(),
        1,
        "the first scan should mint exactly one import-target edge"
    );
    let edge_id = &first_target_edges[0];

    // Remove the import; the next incremental scan must retire the edge.
    fs::write(&consumer, "pub fn no_imports_here() {}\n").expect("fixture should rewrite");
    let second = scan_repository_incremental(&repo, &cache_path).expect("second scan should work");

    assert!(
        second.graph.records().iter().any(
            |record| matches!(record, GraphRecord::Tombstone { deleted_id, .. } if deleted_id == edge_id)
        ),
        "removing the import must tombstone its import-target edge"
    );
    assert!(
        !import_target_edge_ids(second.graph.records()).contains(edge_id),
        "the tombstoned edge must not be re-emitted as live"
    );
}

/// The target-side IMPORTS edges the issue-#444 pass mints: an `IMPORTS` edge
/// whose target is a Module/File node. The extractor's containment-shaped
/// `File —IMPORTS→ Import` edges (target kind `Import`) are excluded.
fn import_target_edge_ids(records: &[GraphRecord]) -> Vec<String> {
    let node_kind: std::collections::BTreeMap<&str, NodeKind> = records
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Node { id, kind, .. } => Some((id.as_str(), *kind)),
            GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => None,
        })
        .collect();
    records
        .iter()
        .filter_map(|record| match record {
            GraphRecord::Edge {
                id,
                label: EdgeLabel::Imports,
                target,
                ..
            } if node_kind
                .get(target.as_str())
                .is_some_and(|kind| *kind == NodeKind::Module || *kind == NodeKind::File) =>
            {
                Some(id.clone())
            }
            _ => None,
        })
        .collect()
}
