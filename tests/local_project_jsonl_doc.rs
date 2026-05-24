#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]

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

fn assert_contains_none(path: &str, needles: &[&str]) {
    let text = read_repo_text(path);
    for needle in needles {
        assert!(
            !text.contains(needle),
            "{path} must not document stale contract `{needle}`"
        );
    }
}

#[test]
fn local_project_jsonl_schema_doc_locks_file_contract() {
    assert_contains_all(
        "docs/schema/local-project-jsonl.md",
        &[
            "# Local Project/Task JSONL File Format - v1",
            "single source of truth",
            "source_kind: local_jsonl",
            ".egregore/tasks/<project-slug>.jsonl",
            "the path is the project identity",
            "duplicate_project_slug",
            "`kind`",
            "`task`",
            "`acceptance_criterion`",
            "`external_link`",
            "`header`",
            "unknown `kind` MUST emit a `Diagnostic` and skip the line",
            "`product`",
            "`project`",
            "`plan`",
            "`review`",
            "`local_task`",
            r#"{"kind": "header", "schema_version": 1, "project_slug": "<slug>", "created_at": "<rfc3339>"}"#,
            "missing_header",
            "`local_id` is the stable identifier",
            "duplicate `local_id` lines are valid only as revisions of the same record kind",
            "`valid_time_source` to `local_jsonl_updated_at`",
            "`open`",
            "`in_progress`",
            "`blocked`",
            "`closed_completed`",
            "`closed_dropped`",
            "`urgent`",
            "`bytes`",
            "missing_body_bytes",
            "omitted `task.body` projects as the canonical empty body",
            "BLAKE3 hash of the empty byte string",
            "`bytes` = 0",
            "parent_task_local_id",
            "unresolved_parent_task",
            "verification_handle",
            "verified `acceptance_criterion` lines without `verification_handle` are skipped",
            "verified `acceptance_criterion` lines with unresolved `verification_handle` are skipped",
            "acceptance_criterion_missing_verification",
            "unresolved_verification_handle",
            "acceptance_criterion.updated_at",
            "`github`",
            "`gitlab`",
            "`local_file`",
            "`harness_legacy`",
            "`other`",
            "external_link.updated_at",
            "unresolved_parent_local_id",
            "Explicit `external_link` rows are optional only for additional, non-source links",
            "importer MUST materialize one source `ExternalLink` for every `task`",
            "`source_external_link_id`",
            "latest line for a given `local_id`",
            "task, acceptance_criterion, or external_link",
            "identity fields MUST NOT change across revisions with the same `local_id`",
            "`AcceptanceCriterion` identity fields are `parent_task_local_id` and `ordinal`",
            "`ExternalLink` identity fields are `system` and `system_native_id`",
            "revision_identity_mismatch",
            "file order is the only current-state winner rule",
            "append-with-same-entity-id",
            "`transaction_time` set to import time",
            "`valid_time` set to the line's `updated_at`",
            ".egregore/tasks/<project>.jsonl.tmp-<uuid>",
            "fsync the containing directory",
            ".tmp-*",
            "NOT redacted at rest",
            "local file is the source of truth",
            "task.title",
            "task.body",
            "task.labels",
            "task.assignees",
            "acceptance_criterion.text",
            "external_link.url",
            "project:v<schema_version>:<blake3(domain || kind || source_kind || source_native_id || entity_kind_identity)>",
            "`<encoded_file_path>:<encoded_local_id>`",
            "`source_handle` uses `<encoded_file_path>:<encoded_local_id>:<record_hash>`",
            "`record_hash` is the BLAKE3 hash of the canonical source line bytes",
            "literal `:` MUST be percent-encoded as `%3A`",
            "ASCII alphanumeric plus `-`, `.`, `_`, and `~` are the only unescaped bytes",
            "literal `/` MUST be percent-encoded as `%2F`",
            "source_handle_encoding_error",
            "explicit source-link refinement field precedence",
            "explicit `external_link` row wins for `url`, `discovered_at`, and `updated_at`",
            "materialized source link retains identity and task parent wiring",
            "`AcceptanceCriterion` | `(parent_task_id, ordinal)`",
            "`ExternalLink` | `(system, system_native_id)`",
            "renaming a local JSONL file produces new in-graph IDs",
            ".egregore%2Ftasks%2Fsample.jsonl:sample-task",
            "idempotent re-import",
            ".egregore/tasks/sample.jsonl",
            "byte-equal graph output",
            "one new row per appended line",
            "inspect-tasks .egregore/tasks/",
            "Versioning rules",
            "schema_version` = `1`",
            "schema_version` = `2`",
        ],
    );

    assert_contains_none(
        "docs/schema/local-project-jsonl.md",
        &[
            "Required, unique within the file",
            "file modification time source",
            "leaves the in-graph `verification_link_id` null",
            "Rule: external_link lines are optional; their absence does not block import",
            "ties broken by `updated_at`",
            "project:v<schema_version>:<blake3(domain || kind || source_kind || file_path || local_id)>",
            r#"`system_native_id": ".egregore/tasks/sample.jsonl:sample-task""#,
        ],
    );
}

#[test]
fn local_project_jsonl_schema_doc_is_linked_and_coordinated() {
    assert_contains_all("README.md", &["docs/schema/local-project-jsonl.md"]);
    assert_contains_all(
        "docs/prd/0000-egregore-vision.md",
        &[
            "docs/schema/local-project-jsonl.md",
            "Local project/task JSONL format",
        ],
    );
    assert_contains_all(
        "docs/schema/project-graph.md",
        &[
            "docs/schema/local-project-jsonl.md",
            "`source_kind: local_jsonl`",
            "<file_path>:<local_id>",
            "percent-encoded source handle",
            "`source_handle` is `<encoded_file_path>:<encoded_local_id>:<record_hash>`",
        ],
    );
    assert_contains_all(
        "docs/schema/redaction.md",
        &[
            "task.title",
            "task.body",
            "task.labels",
            "task.assignees",
            "acceptance_criterion.text",
            "external_link.url",
            "does not redact at rest",
        ],
    );
    assert_contains_all(
        "docs/schema/agent-memory.md",
        &[
            "REFERENCES_TASK",
            "file path + local_id",
            "not by guessing the file format",
        ],
    );

    assert_contains_all("docs/schema/project-graph.md", &["local_jsonl_updated_at"]);
    assert_contains_none(
        "docs/schema/project-graph.md",
        &["file modification time source"],
    );
}
