//! Documentation locks for Git author attribution (issue #116).
//!
//! Issue #116's final acceptance criterion requires the documentation to
//! state that authorship is a deterministic VCS-derived fact, name the new
//! field(s), and note the redaction behavior. These tests lock that contract
//! the same way `local_project_jsonl_doc.rs` locks the local JSONL schema doc.

use std::{fs, path::Path};

fn read_repo_text(path: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
        .unwrap_or_else(|error| panic!("{path} should be readable: {error}"))
}

fn assert_contains_all(path: &str, needles: &[&str]) {
    let text = read_repo_text(path);
    for needle in needles {
        assert!(
            text.contains(needle),
            "{path} must document or link `{needle}`"
        );
    }
}

#[test]
fn query_doc_documents_who_subcommand_and_author_fields() {
    assert_contains_all(
        "docs/cli/query.md",
        &[
            // The subcommand exists with its synopsis and section.
            "## eg query who",
            "eg query who",
            // The new fields are named.
            "`author_name`",
            "`author_email`",
            "`commit_sha`",
            "`symbol_name`",
            // Authorship is typed as a deterministic VCS-derived fact,
            // distinct from `author_time`, and not an ownership claim.
            "deterministic VCS-derived fact",
            "`author_time`",
            "not an ownership claim",
            // Temporal selectors are documented for the who answer.
            "`--at <COMMIT>`",
            "`--as-of <RFC3339>`",
            // The answer carries citable handles.
            "`repo_relative_path`",
            // Redaction behavior is noted and linked.
            "redaction-eligible",
            "../schema/redaction.md",
        ],
    );
}

#[test]
fn redaction_doc_names_author_email_as_redaction_eligible_pii() {
    assert_contains_all(
        "docs/schema/redaction.md",
        &[
            // The email secret class shipped with issue #116.
            "`email`",
            // The concrete Commit-record fields are named.
            "`author_email`",
            "`author_name`",
            // Export scrubs to a marker; the local store retains the raw
            // address at rest.
            "<REDACTED:email:",
            "retains the raw author email",
            "zero raw author email addresses",
        ],
    );
}

#[test]
fn bundle_doc_notes_author_email_scrub_on_export() {
    assert_contains_all(
        "docs/cli/bundle.md",
        &["`author_email`", "<REDACTED:email:"],
    );
}
