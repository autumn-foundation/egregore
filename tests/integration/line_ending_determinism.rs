#![allow(missing_docs)]

use aletheia_egregore::scan_repository_at_with_override;
use std::fs;

#[test]
fn test_line_ending_and_path_separator_determinism() {
    let temp_lf = tempfile::tempdir().expect("temp dir LF should be created");
    let temp_crlf = tempfile::tempdir().expect("temp dir CRLF should be created");

    let lf_dir = temp_lf.path();
    let crlf_dir = temp_crlf.path();

    // Create subdirectories to exercise nested paths
    let lf_src = lf_dir.join("src/nested");
    let crlf_src = crlf_dir.join("src/nested");
    fs::create_dir_all(&lf_src).unwrap();
    fs::create_dir_all(&crlf_src).unwrap();

    let code = "pub struct Widget {\n    pub value: usize,\n}\n\nimpl Widget {\n    pub fn new(value: usize) -> Self {\n        Self { value }\n    }\n}\n";
    let code_lf = code.replace("\r\n", "\n");
    let code_crlf = code_lf.replace('\n', "\r\n");

    fs::write(lf_src.join("another.rs"), &code_lf).unwrap();
    fs::write(crlf_src.join("another.rs"), &code_crlf).unwrap();

    let fixed_time = "2026-06-30T00:00:00Z";
    let repo_id = "test-line-ending-repo";

    let mut last_jsonl_lf = None;
    let mut last_jsonl_crlf = None;

    for i in 0..5 {
        let graph_lf = scan_repository_at_with_override(lf_dir, fixed_time, Some(repo_id))
            .expect("LF scan should succeed");
        let graph_crlf = scan_repository_at_with_override(crlf_dir, fixed_time, Some(repo_id))
            .expect("CRLF scan should succeed");

        let jsonl_lf = graph_lf.to_jsonl().expect("serialize LF");
        let jsonl_crlf = graph_crlf.to_jsonl().expect("serialize CRLF");

        if let Some(ref last_lf) = last_jsonl_lf {
            assert_eq!(jsonl_lf, *last_lf, "LF scan run {i} is not deterministic");
        }
        if let Some(ref last_crlf) = last_jsonl_crlf {
            assert_eq!(
                jsonl_crlf, *last_crlf,
                "CRLF scan run {i} is not deterministic"
            );
        }

        last_jsonl_lf = Some(jsonl_lf);
        last_jsonl_crlf = Some(jsonl_crlf);
    }

    let jsonl_lf = last_jsonl_lf.unwrap();
    let jsonl_crlf = last_jsonl_crlf.unwrap();

    // Print details if they differ to help debugging
    if jsonl_lf != jsonl_crlf {
        println!("LF JSONL:\n{jsonl_lf}");
        println!("CRLF JSONL:\n{jsonl_crlf}");
    }

    // 1. Path separator parity: verify no backslashes in any repo_relative_path
    for line in jsonl_lf.lines().chain(jsonl_crlf.lines()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        if let Some(path) = v.get("repo_relative_path").and_then(|p| p.as_str()) {
            assert!(
                !path.contains('\\'),
                "repo_relative_path contains backslash: {path}"
            );
        }
    }

    // 2. Byte-for-byte stability
    assert_eq!(
        jsonl_lf, jsonl_crlf,
        "LF and CRLF checkouts must produce byte-for-byte identical canonical JSONL output"
    );
}
