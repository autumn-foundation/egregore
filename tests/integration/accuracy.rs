#![allow(missing_docs, clippy::float_cmp, clippy::uninlined_format_args)]

use assert_cmd::Command;
use serde_json::Value;
use std::path::{Path, PathBuf};

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/accuracy")
}

fn labels_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/accuracy_labels.json")
}

#[test]
fn test_accuracy_command_passes_with_expected_metrics() {
    let assert = egregore()
        .args([
            "audit",
            "accuracy",
            "--corpus-dir",
            &corpus_dir().to_string_lossy(),
            "--labels",
            &labels_path().to_string_lossy(),
        ])
        .assert();

    let output = assert.get_output();
    let stdout_str = String::from_utf8(output.stdout.clone()).unwrap();
    let stderr_str = String::from_utf8(output.stderr.clone()).unwrap();

    println!("STDOUT:\n{}", stdout_str);
    println!("STDERR:\n{}", stderr_str);

    assert.success();

    let report: Value = serde_json::from_str(&stdout_str).unwrap();
    assert_eq!(report["ok"], true, "Gate should pass");

    // Symbol and Import node precision/recall should be 1.0
    let symbol_metrics = &report["metrics"]["nodes"]["Symbol"];
    assert_eq!(symbol_metrics["precision"].as_f64().unwrap(), 1.0);
    assert_eq!(symbol_metrics["recall"].as_f64().unwrap(), 1.0);

    let defines_metrics = &report["metrics"]["edges"]["DEFINES"];
    assert_eq!(defines_metrics["precision"].as_f64().unwrap(), 1.0);
    assert_eq!(defines_metrics["recall"].as_f64().unwrap(), 1.0);

    // Diagnostic node for macro should be present
    let diagnostic_metrics = &report["metrics"]["nodes"]["Diagnostic"];
    assert_eq!(diagnostic_metrics["true_positives"].as_u64().unwrap(), 1);

    // Verify comment and string-literal decoy calls emitted 0 CALLS edges
    // The actual lib.rs contains "free_function(10);" inside a comment and "free_function(20);" in a string literal.
    // The CALLS edges true_positives should be exactly 1 (trait_method calls free_function), and false_positives 0.
    let calls_metrics = &report["metrics"]["edges"]["CALLS"];
    assert_eq!(calls_metrics["true_positives"].as_u64().unwrap(), 1);
    assert_eq!(calls_metrics["false_positives"].as_u64().unwrap(), 0);
}

#[test]
fn test_accuracy_fails_below_threshold() {
    let assert = egregore()
        .args([
            "audit",
            "accuracy",
            "--corpus-dir",
            &corpus_dir().to_string_lossy(),
            "--labels",
            &labels_path().to_string_lossy(),
            "--min-precision",
            "1.1", // impossible precision
        ])
        .assert();

    let output = assert.get_output();
    assert_eq!(
        output.status.code(),
        Some(1),
        "Gate should fail with code 1"
    );

    let stderr_str = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(stderr_str.contains("below the threshold of 1.100"));
}

#[test]
fn test_accuracy_exits_with_2_on_usage_error() {
    let assert = egregore()
        .args([
            "audit",
            "accuracy",
            "--corpus-dir",
            "corpus/nonexistent_dir",
            "--labels",
            "corpus/nonexistent_labels.json",
        ])
        .assert();

    let output = assert.get_output();
    assert_eq!(
        output.status.code(),
        Some(2),
        "Should exit with 2 on usage/load error"
    );
}

#[test]
fn test_accuracy_is_deterministic_and_byte_identical() {
    let first = egregore()
        .args([
            "audit",
            "accuracy",
            "--corpus-dir",
            &corpus_dir().to_string_lossy(),
            "--labels",
            &labels_path().to_string_lossy(),
        ])
        .assert()
        .get_output()
        .stdout
        .clone();

    for _ in 0..5 {
        let again = egregore()
            .args([
                "audit",
                "accuracy",
                "--corpus-dir",
                &corpus_dir().to_string_lossy(),
                "--labels",
                &labels_path().to_string_lossy(),
            ])
            .assert()
            .get_output()
            .stdout
            .clone();
        assert_eq!(first, again, "Output must be byte-identical across runs");
    }
}
