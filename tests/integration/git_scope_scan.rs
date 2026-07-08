#![allow(missing_docs)]

use std::fs;
use std::process::Command;
use tempfile::TempDir;

#[test]
fn test_git_scope_scan_behavior() {
    let temp = TempDir::new().expect("temp dir");
    let repo_path = temp.path();

    // Initialize git repo
    let run_git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo_path)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success());
    };

    run_git(&["init"]);
    run_git(&["config", "user.name", "Test User"]);
    run_git(&["config", "user.email", "test@example.com"]);

    // Create .gitignore
    fs::write(repo_path.join(".gitignore"), "generated/\nvendor/\n").unwrap();

    // Create directories
    fs::create_dir_all(repo_path.join("src")).unwrap();
    fs::create_dir_all(repo_path.join("generated")).unwrap();
    fs::create_dir_all(repo_path.join("vendor/dep")).unwrap();

    // Create files
    // Rust
    fs::write(repo_path.join("src/lib.rs"), "pub fn tracked_fn() {}").unwrap();
    fs::write(repo_path.join("generated/build.rs"), "fn main() {}").unwrap();
    fs::write(repo_path.join("vendor/dep/lib.rs"), "fn internal() {}").unwrap();
    fs::write(repo_path.join("scratch.rs"), "fn scratch() {}").unwrap();

    // Python
    fs::write(repo_path.join("src/main.py"), "def test(): pass").unwrap();
    fs::write(repo_path.join("scratch.py"), "def scratch(): pass").unwrap();

    // TypeScript
    fs::write(repo_path.join("src/index.ts"), "export const x = 1;").unwrap();
    fs::write(repo_path.join("scratch.tsx"), "export const y = 2;").unwrap();

    // Go
    fs::write(repo_path.join("src/main.go"), "package main").unwrap();
    fs::write(repo_path.join("scratch.go"), "package main").unwrap();

    // Track src/ files and .gitignore
    run_git(&[
        "add",
        "src/lib.rs",
        "src/main.py",
        "src/index.ts",
        "src/main.go",
        ".gitignore",
    ]);
    run_git(&["commit", "-m", "initial commit"]);

    // Run eg scan command
    let output = assert_cmd::Command::cargo_bin("egregore")
        .unwrap()
        .arg("scan")
        .arg(repo_path)
        .arg("--out")
        .arg(repo_path.join("graph.jsonl"))
        .output()
        .expect("eg scan run");

    assert!(output.status.success());

    // Verify stderr output reports skipped count for all languages
    let stderr_str = String::from_utf8_lossy(&output.stderr);
    assert!(stderr_str.contains("Skipped 3 .rs, 1 .py, 1 .ts/.tsx, 1 .go files by ignore rules"));

    // Verify only tracked files are scanned
    let graph_content = fs::read_to_string(repo_path.join("graph.jsonl")).unwrap();
    assert!(graph_content.contains("src/lib.rs"));
    assert!(graph_content.contains("src/main.py"));
    assert!(graph_content.contains("src/index.ts"));
    assert!(graph_content.contains("src/main.go"));

    assert!(!graph_content.contains("generated/build.rs"));
    assert!(!graph_content.contains("vendor/dep/lib.rs"));
    assert!(!graph_content.contains("scratch.rs"));
    assert!(!graph_content.contains("scratch.py"));
    assert!(!graph_content.contains("scratch.tsx"));
    assert!(!graph_content.contains("scratch.go"));
}
