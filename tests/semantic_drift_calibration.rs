#![allow(missing_docs)]

use assert_cmd::Command;
use std::fs;
use tempfile::tempdir;

#[test]
fn eval_drift_fails_when_corpus_missing() {
    Command::cargo_bin("egregore")
        .unwrap()
        .args([
            "eval-drift",
            "--corpus",
            "nonexistent_corpus_file_12345.json",
        ])
        .assert()
        .failure();
}

#[test]
fn eval_drift_calibrates_successfully_and_is_deterministic_across_five_runs() {
    let temp = tempdir().expect("create temp dir");
    let corpus_path = temp.path().join("test_corpus.json");

    let corpus_json = r#"{
  "corpus_version": "1.0",
  "description": "Mini test corpus for semantic drift determinism",
  "scenarios": [
    {
      "id": "meaning_mini",
      "class": "meaning_changed",
      "file_path": "src/lib.rs",
      "before": "pub fn add(a: i32, b: i32) -> i32 { a + b }",
      "after": "pub fn add(a: i32, b: i32) -> i32 { a - b }"
    },
    {
      "id": "unchanged_mini",
      "class": "unchanged",
      "file_path": "src/lib.rs",
      "before": "pub fn nop() {}",
      "after": "pub fn nop() {}"
    }
  ]
}"#;

    fs::write(&corpus_path, corpus_json).expect("write test corpus");

    // First run
    let assert1 = Command::cargo_bin("egregore")
        .unwrap()
        .args(["eval-drift", "--corpus"])
        .arg(&corpus_path)
        .arg("--threshold")
        .arg("0.0005")
        .assert()
        .success();

    let stdout1 = assert1.get_output().stdout.clone();

    // Verify output structure
    let out_str = String::from_utf8(stdout1.clone()).unwrap();
    assert!(out_str.contains("Semantic Drift Calibration Report"));
    assert!(out_str.contains("meaning_mini"));
    assert!(out_str.contains("unchanged_mini"));

    // Run 4 more times to verify determinism / byte-equivalence
    for _ in 0..4 {
        let assert_n = Command::cargo_bin("egregore")
            .unwrap()
            .args(["eval-drift", "--corpus"])
            .arg(&corpus_path)
            .arg("--threshold")
            .arg("0.0005")
            .assert()
            .success();

        let stdout_n = assert_n.get_output().stdout.clone();
        assert_eq!(
            stdout1, stdout_n,
            "Outputs of consecutive runs must be exactly byte-equivalent"
        );
    }
}
