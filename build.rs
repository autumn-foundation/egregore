//! Build script: exposes tree-sitter dependency versions as compile-time env vars.

fn main() {
    let lock = std::fs::read_to_string("Cargo.lock").unwrap_or_default();
    println!(
        "cargo:rustc-env=TREE_SITTER_VERSION={}",
        version_for_dep(&lock, "aletheia-egregore", "tree-sitter")
    );
    println!(
        "cargo:rustc-env=TREE_SITTER_RUST_VERSION={}",
        version_for_dep(&lock, "aletheia-egregore", "tree-sitter-rust")
    );
    println!(
        "cargo:rustc-env=TREE_SITTER_PYTHON_VERSION={}",
        version_for_dep(&lock, "aletheia-egregore", "tree-sitter-python")
    );
    println!(
        "cargo:rustc-env=TREE_SITTER_TYPESCRIPT_VERSION={}",
        version_for_dep(&lock, "aletheia-egregore", "tree-sitter-typescript")
    );
    println!(
        "cargo:rustc-env=TREE_SITTER_GO_VERSION={}",
        version_for_dep(&lock, "aletheia-egregore", "tree-sitter-go")
    );
    println!("cargo:rerun-if-changed=Cargo.lock");
}

/// Finds the version of `dep_name` that `root_pkg` directly depends on.
///
/// When the root package's dependency list includes a version
/// (e.g. `"tree-sitter 0.26.8"` — which Cargo emits when multiple versions of
/// the same crate are present), that version is used to locate the exact package
/// block.  When no version is present in the dep entry (single-version case),
/// the first matching `[[package]]` block is returned, which is always correct
/// because there is only one.
fn version_for_dep(lock: &str, root_pkg: &str, dep_name: &str) -> String {
    let pinned = version_from_root_deps(lock, root_pkg, dep_name);
    extract_package_version(lock, dep_name, pinned.as_deref())
}

/// Extracts the explicit version of `dep_name` from the root package's
/// `dependencies = [...]` block in the lock file, if one is present.
///
/// Returns `Some("X.Y.Z")` when the dep entry looks like `"dep-name X.Y.Z ..."`,
/// `None` when the entry is just `"dep-name"` (single-version case).
fn version_from_root_deps(lock: &str, root_pkg: &str, dep_name: &str) -> Option<String> {
    let root_header = format!("name = \"{root_pkg}\"");
    let dep_prefix = format!("\"{dep_name} ");
    let dep_bare = format!("\"{dep_name}\"");

    let mut in_root = false;
    let mut in_deps = false;

    for line in lock.lines() {
        let trimmed = line.trim();
        // Cargo.lock v4 appends a trailing comma to every dep entry; strip it for comparisons.
        let trimmed = trimmed.trim_end_matches(',');

        if trimmed == root_header.as_str() {
            in_root = true;
            continue;
        }

        if in_root {
            if trimmed == "dependencies = [" {
                in_deps = true;
                continue;
            }
            if in_deps {
                if trimmed == "]" {
                    break;
                }
                // bare entry — single-version crate, no pinning needed
                if trimmed == dep_bare.as_str() {
                    return None;
                }
                // versioned entry: `"tree-sitter 0.26.8 (registry+...)"` or `"tree-sitter 0.26.8"`
                if let Some(rest) = trimmed.strip_prefix(dep_prefix.as_str()) {
                    let version = rest
                        .split_whitespace()
                        .next()?
                        .trim_matches(|c| c == '"' || c == ',');
                    return Some(version.to_owned());
                }
            }
            // New [[package]] block means we've left the root block
            if trimmed == "[[package]]" {
                break;
            }
        }
    }
    None
}

/// Returns the version string for the first `[[package]]` block with the given
/// `name`, optionally filtered to a specific `version`.
fn extract_package_version(lock: &str, name: &str, version: Option<&str>) -> String {
    let target_name = format!("name = \"{name}\"");
    let mut found_name = false;

    for line in lock.lines() {
        let trimmed = line.trim();
        if trimmed == target_name.as_str() {
            found_name = true;
        } else if found_name {
            if let Some(v) = trimmed
                .strip_prefix("version = \"")
                .and_then(|s| s.strip_suffix('"'))
            {
                if version.is_none_or(|want| want == v) {
                    return v.to_owned();
                }
                // Wrong version — keep scanning for the right one
                found_name = false;
            } else if trimmed.starts_with("[[") {
                found_name = false;
            }
        }
    }
    "unknown".to_owned()
}
