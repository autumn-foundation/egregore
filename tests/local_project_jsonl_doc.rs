#![allow(missing_docs)]

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
            "parent_task_local_id",
            "unresolved_parent_task",
            "verification_handle",
            "`github`",
            "`gitlab`",
            "`local_file`",
            "`harness_legacy`",
            "`other`",
            "external_link lines are optional",
            "latest line for a given `local_id`",
            "append-with-same-entity-id",
            "`transaction_time` set to import time",
            "`valid_time` set to the line's `updated_at`",
            ".egregore/tasks/<project>.jsonl.tmp-<uuid>",
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
            "`AcceptanceCriterion` | `(parent_task_id, ordinal)`",
            "`ExternalLink` | `(system, system_native_id)`",
            "renaming a local JSONL file produces new in-graph IDs",
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
            "project:v<schema_version>:<blake3(domain || kind || source_kind || file_path || local_id)>",
        ],
    );
}

#[test]
fn local_project_jsonl_schema_doc_is_linked_and_coordinated() {
    assert_contains_all(
        "README.md",
        &["docs/schema/local-project-jsonl.md"],
    );
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

    assert_contains_all(
        "docs/schema/project-graph.md",
        &["local_jsonl_updated_at"],
    );
    assert_contains_none(
        "docs/schema/project-graph.md",
        &["file modification time source"],
    );
}
