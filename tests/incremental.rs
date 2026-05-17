#![allow(missing_docs)]

use std::fs;

use aletheia_egregore::incremental::scan_repository_incremental;

#[test]
fn incremental_reuses_unchanged_files_and_tombstones_removed_files() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    let src = repo.join("src");
    fs::create_dir_all(&src).expect("fixture src dir should be created");
    let lib = src.join("lib.rs");
    fs::write(&lib, "pub fn answer() -> usize { 42 }\n").expect("fixture should write");
    let cache_path = temp.path().join("codegraph-cache.json");

    let first = scan_repository_incremental(&repo, &cache_path).expect("first scan should work");
    assert_eq!(first.rebuilt_files, ["src/lib.rs"]);
    assert!(first.reused_files.is_empty());
    assert!(first.tombstoned_files.is_empty());

    let unchanged =
        scan_repository_incremental(&repo, &cache_path).expect("unchanged scan should work");
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
