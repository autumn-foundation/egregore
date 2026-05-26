//! Build script: exposes tree-sitter dependency versions as compile-time env vars.

fn main() {
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    println!(
        "cargo:rustc-env=TREE_SITTER_VERSION={}",
        extract_version(&lock, "tree-sitter")
    );
    println!(
        "cargo:rustc-env=TREE_SITTER_RUST_VERSION={}",
        extract_version(&lock, "tree-sitter-rust")
    );
    println!("cargo:rerun-if-changed=Cargo.lock");
}

fn extract_version(lock: &str, name: &str) -> String {
    let target = format!("name = \"{name}\"");
    let mut in_block = false;
    for line in lock.lines() {
        let trimmed = line.trim();
        if trimmed == target.as_str() {
            in_block = true;
        } else if in_block {
            if let Some(v) = trimmed
                .strip_prefix("version = \"")
                .and_then(|s| s.strip_suffix('"'))
            {
                return v.to_owned();
            }
            if trimmed.starts_with('[') {
                break;
            }
        }
    }
    "unknown".to_owned()
}
