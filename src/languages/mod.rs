//! Language-specific source extractors.

use std::{ffi::OsStr, path::Path};

/// Shared, language-neutral extraction helpers.
pub mod common;
/// Deterministic repo-wide cross-file call resolution (issue #152).
pub mod cross_file;
/// Go source extraction.
pub mod go;
/// Python source extraction.
pub mod python;
/// Rust source extraction.
pub mod rust;
/// TypeScript (and TSX) source extraction.
pub mod typescript;

/// A source language the scanner can extract a code graph from.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Language {
    /// Rust (`.rs`).
    Rust,
    /// Python (`.py`).
    Python,
    /// TypeScript (`.ts`) and TSX (`.tsx`).
    TypeScript,
    /// Go (`.go`).
    Go,
}

impl Language {
    /// Every source language the scanner can extract, in a stable order.
    ///
    /// The single source of truth for "which languages does `eg scan` index?",
    /// consumed by the scan-coverage summary (issue #135) so the named language
    /// scope can never drift from the extractor's real capability.
    pub const ALL: [Self; 4] = [Self::Rust, Self::Python, Self::TypeScript, Self::Go];

    /// The stable language tag stored on graph records.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Go => "go",
        }
    }

    /// Human-facing display name used in File node summaries.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Rust => "Rust",
            Self::Python => "Python",
            Self::TypeScript => "TypeScript",
            Self::Go => "Go",
        }
    }

    /// The producer-component key and version string for this language's
    /// Tree-sitter grammar, recorded in the producer envelope.
    #[must_use]
    pub const fn tree_sitter_component(self) -> (&'static str, &'static str) {
        match self {
            Self::Rust => ("tree_sitter_rust", env!("TREE_SITTER_RUST_VERSION")),
            Self::Python => ("tree_sitter_python", env!("TREE_SITTER_PYTHON_VERSION")),
            Self::TypeScript => (
                "tree_sitter_typescript",
                env!("TREE_SITTER_TYPESCRIPT_VERSION"),
            ),
            Self::Go => ("tree_sitter_go", env!("TREE_SITTER_GO_VERSION")),
        }
    }

    /// Returns the language for a record `language` tag, if recognized.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "typescript" => Some(Self::TypeScript),
            "go" => Some(Self::Go),
            _ => None,
        }
    }
}

/// Detects the source language of a repo-relative path by file extension.
///
/// Returns `None` for paths that are not a supported source file.
#[must_use]
pub fn detect(repo_relative_path: &str) -> Option<Language> {
    detect_path(Path::new(repo_relative_path))
}

/// Detects the source language of a path by file extension.
#[must_use]
pub fn detect_path(path: &Path) -> Option<Language> {
    match path.extension().and_then(OsStr::to_str) {
        Some("rs") => Some(Language::Rust),
        Some("py") => Some(Language::Python),
        Some("ts" | "tsx") => Some(Language::TypeScript),
        Some("go") => Some(Language::Go),
        _ => None,
    }
}

/// Returns `true` when `path` is a source file the scanner indexes.
///
/// Extension matching is case-sensitive and lowercase, matching the live
/// scanner's discovery filter so history replay and the freshness probe index
/// exactly the same set.
#[must_use]
pub fn is_supported_source(path: &Path) -> bool {
    detect_path(path).is_some()
}

/// Normalizes line endings so scans are byte-stable across checkouts (issue
/// #242).
///
/// CRLF (`\r\n`) and lone CR (`\r`) both become LF (`\n`). Each language's
/// `extract_file_source` applies this at its parse boundary — before
/// Tree-sitter sees the source — so byte spans, symbol text, signatures,
/// summaries, and content hashes are computed over canonical content no
/// matter which line-ending convention the checkout used (Git
/// `core.autocrlf=true` on Windows vs LF elsewhere). The scan funnel in
/// `crate::scan_source_text_records` normalizes once more ahead of it for the
/// File-node summary text; normalization is idempotent, so the parse
/// boundary's pass costs already-normalized callers only the fast path.
///
/// This mirrors the byte-level `normalize_newlines` used for log artifacts
/// (`crate::log_graph`), but operates on decoded source text at the scan
/// boundary.
#[must_use]
pub fn normalize_line_endings(source: &str) -> String {
    if !source.contains('\r') {
        return source.to_owned();
    }
    source.replace("\r\n", "\n").replace('\r', "\n")
}

#[cfg(test)]
mod tests {
    use super::normalize_line_endings;

    #[test]
    fn crlf_becomes_lf() {
        assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb\n");
    }

    #[test]
    fn lone_cr_becomes_lf() {
        assert_eq!(normalize_line_endings("a\rb\r"), "a\nb\n");
    }

    #[test]
    fn mixed_endings_normalize() {
        assert_eq!(normalize_line_endings("a\r\nb\rc\n"), "a\nb\nc\n");
    }

    #[test]
    fn lf_source_passes_through_unchanged() {
        let source = "fn main() {\n    println!(\"hi\");\n}\n";
        assert_eq!(normalize_line_endings(source), source);
    }

    #[test]
    fn empty_source_normalizes_to_empty() {
        assert_eq!(normalize_line_endings(""), "");
    }

    #[test]
    fn normalization_is_idempotent() {
        let once = normalize_line_endings("a\r\nb\rc\n");
        assert_eq!(normalize_line_endings(&once), once);
    }
}
