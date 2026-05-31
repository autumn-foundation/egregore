//! Token resolution for the GitHub importer (`docs/schema/import-github.md` §2).
//!
//! The auth surface is a closed enumeration: `GH_TOKEN`, then `GITHUB_TOKEN`,
//! then an operator `--token-file`, then `gh auth token` via the local CLI
//! keychain. No config-file storage and no daemon-runtime storage. Token bytes
//! are used in-process only and never logged.

use std::{path::Path, process::Command};

/// Resolves a GitHub token from the closed auth enumeration, or `None` when no
/// source yields one (anonymous import).
///
/// Resolution order: `GH_TOKEN` env, `GITHUB_TOKEN` env, `--token-file`
/// contents, then `gh auth token`. The first non-empty source wins.
#[must_use]
pub fn resolve_token(token_file: Option<&Path>) -> Option<String> {
    if let Some(t) = env_token("GH_TOKEN") {
        return Some(t);
    }
    if let Some(t) = env_token("GITHUB_TOKEN") {
        return Some(t);
    }
    if let Some(path) = token_file
        && let Ok(contents) = std::fs::read_to_string(path)
    {
        let t = contents.trim();
        if !t.is_empty() {
            return Some(t.to_owned());
        }
    }
    gh_cli_token()
}

/// Reads a non-empty, trimmed token from environment variable `name`.
fn env_token(name: &str) -> Option<String> {
    let v = std::env::var(name).ok()?;
    let v = v.trim();
    (!v.is_empty()).then(|| v.to_owned())
}

/// Invokes `gh auth token`, returning the trimmed token on success.
///
/// Any failure (missing `gh`, not logged in, non-zero exit) yields `None`; the
/// importer then proceeds anonymously. Output never reaches a log.
fn gh_cli_token() -> Option<String> {
    let output = Command::new("gh").args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_file_is_read_when_env_absent() {
        // This test runs without GH_TOKEN/GITHUB_TOKEN in scope by reading a
        // file directly; env precedence is exercised by the CLI integration
        // tests where the process environment is controlled.
        let dir = std::env::temp_dir().join(format!("egauth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tok");
        std::fs::write(&path, "  filetoken123  \n").unwrap();
        // Only assert the file path is honoured when env vars are unset; we
        // cannot safely mutate process env in parallel tests, so check the
        // file-read helper indirectly by ensuring the trimmed value is used
        // when env lookups miss.
        if std::env::var("GH_TOKEN").is_err() && std::env::var("GITHUB_TOKEN").is_err() {
            // gh CLI may or may not exist; only assert when the file wins.
            let resolved = resolve_token(Some(&path));
            assert_eq!(resolved.as_deref(), Some("filetoken123"));
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
