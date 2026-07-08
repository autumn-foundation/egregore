#![allow(missing_docs)]

use std::fs;

use aletheia_egregore::incremental::{scan_repository_incremental, scan_repository_incremental_at};
use aletheia_egregore::{GraphRecord, NodeKind, SCHEMA_VERSION, SourceSpan, stable_id};

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
