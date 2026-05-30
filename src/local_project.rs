//! Local project/task JSONL importer — project-domain source (issue #42).
//!
//! Parses `.egregore/tasks/<slug>.jsonl` files and emits typed project-graph
//! records following the file-format spec in
//! `docs/schema/local-project-jsonl.md` and the record shapes in
//! `docs/schema/project-graph.md`.
//!
//! # Contract
//!
//! - Every line of a valid file produces exactly one graph record (node +
//!   edges).
//! - Invalid lines emit a `Diagnostic` node and do not block valid lines.
//! - Re-importing an unchanged file with the same `transaction_time` produces
//!   byte-for-byte identical JSONL after canonical ordering.
//! - Source handles are always repo-relative; absolute machine paths never
//!   appear in stable IDs or source handles.
//! - No network access or daemon is required.

use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{
    error::{CodegraphError, Result},
    ir::{
        EdgeLabel, Graph, GraphRecord, NodeKind, OutputHandle, PROJECT_SCHEMA_VERSION,
        project_stable_id,
    },
};

// ── Importer identity ─────────────────────────────────────────────────────────

/// Stable importer ID stamped on every emitted record.
pub const IMPORTER_ID: &str = "local-jsonl";
/// Importer version string; bump when the output contract changes.
pub const IMPORTER_VERSION: &str = "0.1.0";
/// Domain value carried on every project-domain record.
pub const DOMAIN: &str = "project";
/// Source kind value for local-JSONL-derived records.
pub const SOURCE_KIND: &str = "local_jsonl";
/// Maximum bytes to inline in a body handle.
const INLINE_BODY_CEILING: usize = 16 * 1024;

// ── Import options ────────────────────────────────────────────────────────────

/// Options controlling local-JSONL import behaviour.
pub struct ImportOptions {
    /// Redaction closure applied to every free-text field before storage.
    pub redact: Box<dyn Fn(&str) -> String + Send + Sync>,
    /// Fixed RFC 3339 `transaction_time` for deterministic / test output.
    /// When `None`, the current wall-clock instant is used.
    pub transaction_time: Option<String>,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            redact: Box::new(str::to_owned),
            transaction_time: None,
        }
    }
}

// ── Import result ─────────────────────────────────────────────────────────────

/// Result of importing one or more local task JSONL files.
pub struct ImportResult {
    /// Project-graph records emitted by the importer.
    pub graph: Graph,
    /// Number of `Diagnostic` records emitted due to invalid input.
    pub diagnostic_count: usize,
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Import local task JSONL files from a directory or a single file.
///
/// When `tasks_path` is a directory every `*.jsonl` file (excluding
/// `.tmp-*` tempfiles) is imported in sorted order. When it is a single
/// file, only that file is imported.
///
/// `repo_root` anchors the repo-relative source handles. Pass the
/// repository root directory so that handles never contain absolute
/// machine paths.
///
/// # Errors
///
/// Returns an error if `tasks_path` cannot be read from disk.
#[allow(clippy::redundant_closure_for_method_calls)]
pub fn import_local_tasks(
    tasks_path: &Path,
    repo_root: &Path,
    opts: &ImportOptions,
) -> Result<ImportResult> {
    let transaction_time = opts
        .transaction_time
        .clone()
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

    let mut graph = Graph::new();
    let mut total_diags: usize = 0;
    let mut seen_slugs: HashMap<String, PathBuf> = HashMap::new();

    if tasks_path.is_dir() {
        let mut files: Vec<PathBuf> = fs::read_dir(tasks_path)
            .map_err(|source| CodegraphError::ReadDirectory {
                path: tasks_path.to_path_buf(),
                source,
            })?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && !p
                        .file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.contains(".tmp-"))
            })
            .collect();
        files.sort();

        for file_path in &files {
            let slug = file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_owned();

            if let Some(existing) = seen_slugs.get(&slug) {
                let file_rel = repo_relative(file_path, repo_root);
                let diag_id = project_stable_id(&[
                    "project",
                    "Diagnostic",
                    SOURCE_KIND,
                    &file_rel,
                    "duplicate_project_slug",
                ]);
                push_diagnostic(
                    &mut graph,
                    diag_id,
                    Some(&file_rel),
                    &format!(
                        "[duplicate_project_slug] slug '{slug}' from '{}' already seen in '{}'",
                        file_rel,
                        existing.display()
                    ),
                    &transaction_time,
                );
                total_diags += 1;
                continue;
            }
            seen_slugs.insert(slug, file_path.clone());

            total_diags += import_file(file_path, repo_root, opts, &transaction_time, &mut graph)?;
        }
    } else {
        total_diags += import_file(tasks_path, repo_root, opts, &transaction_time, &mut graph)?;
    }

    Ok(ImportResult {
        graph,
        diagnostic_count: total_diags,
    })
}

// ── Source handle encoding ────────────────────────────────────────────────────

/// Percent-encode a string per the source-handle encoding rules.
///
/// ASCII alphanumeric plus `-`, `.`, `_`, and `~` are the only unescaped
/// bytes; every other byte is encoded as `%XX` with uppercase hex digits.
#[must_use]
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            other => {
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
}

/// Hashless source identity handle: `<encoded_path>:<encoded_local_id>`.
fn source_identity_handle(file_rel_path: &str, local_id: &str) -> String {
    format!(
        "{}:{}",
        percent_encode(file_rel_path),
        percent_encode(local_id)
    )
}

/// Full source handle with record hash: `<encoded_path>:<encoded_local_id>:<blake3>`.
fn source_handle_for_line(file_rel_path: &str, local_id: &str, line_bytes: &[u8]) -> String {
    let hash = blake3::hash(line_bytes).to_hex().to_string();
    format!(
        "{}:{}:{}",
        percent_encode(file_rel_path),
        percent_encode(local_id),
        hash
    )
}

/// Compute the repo-relative path of `file_path` relative to `repo_root`.
/// Falls back to the raw display path when `strip_prefix` fails (e.g.
/// when the file lives outside the repo root in tests).
fn repo_relative(file_path: &Path, repo_root: &Path) -> String {
    file_path
        .strip_prefix(repo_root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/")
}

// ── Body handle ───────────────────────────────────────────────────────────────

fn body_handle_for(body: &str) -> OutputHandle {
    let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
    let bytes = body.len() as u64;
    let inline = (body.len() <= INLINE_BODY_CEILING).then(|| body.to_owned());
    OutputHandle {
        inline,
        hash,
        bytes,
    }
}

// ── Diagnostic helpers ────────────────────────────────────────────────────────

fn push_diagnostic(
    graph: &mut Graph,
    id: String,
    file_rel_path: Option<&str>,
    message: &str,
    transaction_time: &str,
) {
    let mut record = GraphRecord::node(
        id,
        NodeKind::Diagnostic,
        file_rel_path.map(str::to_owned),
        None,
        None,
        message.to_owned(),
    );
    set_project_base_fields(
        &mut record,
        transaction_time,
        None, // no entity_id for diagnostics
        None, // no valid_time
        None, // no source_handle
    );
    graph.push(record);
}

/// Set the shared project-domain metadata fields on a node record.
fn set_project_base_fields(
    record: &mut GraphRecord,
    transaction_time: &str,
    entity_id_val: Option<&str>,
    valid_time_val: Option<&str>,
    source_handle_val: Option<&str>,
) {
    if let GraphRecord::Node {
        schema_version,
        domain,
        entity_id,
        valid_time,
        valid_time_source,
        transaction_time: tt,
        source_handle,
        importer_id,
        importer_version,
        ..
    } = record
    {
        *schema_version = PROJECT_SCHEMA_VERSION;
        *domain = Some(DOMAIN.to_owned());
        *entity_id = entity_id_val.map(str::to_owned);
        *valid_time = valid_time_val.map(str::to_owned);
        *valid_time_source = valid_time_val.map(|_| "local_jsonl_updated_at".to_owned());
        *tt = Some(transaction_time.to_owned());
        *source_handle = source_handle_val.map(str::to_owned);
        *importer_id = Some(IMPORTER_ID.to_owned());
        *importer_version = Some(IMPORTER_VERSION.to_owned());
    }
}

// ── Input line types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct KindDiscriminator {
    kind: String,
}

#[derive(Deserialize)]
struct HeaderLine {
    schema_version: u32,
    project_slug: String,
    #[allow(dead_code)]
    created_at: String,
}

#[derive(Deserialize)]
struct TaskLine {
    local_id: String,
    title: String,
    #[serde(default)]
    body: Option<serde_json::Value>,
    status: String,
    priority: String,
    #[serde(default)]
    assignees: Vec<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[allow(dead_code)]
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct AcLine {
    local_id: String,
    parent_task_local_id: String,
    ordinal: u32,
    text: String,
    status: String,
    #[serde(default)]
    verification_handle: Option<serde_json::Value>,
    updated_at: String,
}

#[derive(Deserialize)]
struct ExternalLinkLine {
    local_id: String,
    parent_local_id: String,
    system: String,
    url: String,
    system_native_id: String,
    discovered_at: String,
    updated_at: String,
}

// ── Source-link refinement ────────────────────────────────────────────────────

/// Override fields from an explicit `external_link` row that refines the
/// materialized local source link for a task.
struct SrcLinkRefinement {
    url: String,
    discovered_at: String,
    updated_at: String,
}

// ── Parsed record accumulator ─────────────────────────────────────────────────

enum ParsedRecord {
    Task {
        line: TaskLine,
        raw: Vec<u8>,
    },
    AcceptanceCriterion {
        line: AcLine,
        raw: Vec<u8>,
    },
    ExternalLink {
        line: ExternalLinkLine,
        raw: Vec<u8>,
    },
}

// ── File-level importer ───────────────────────────────────────────────────────

/// Import a single JSONL file. Returns the number of diagnostics emitted.
#[allow(clippy::too_many_lines)]
fn import_file(
    file_path: &Path,
    repo_root: &Path,
    opts: &ImportOptions,
    transaction_time: &str,
    graph: &mut Graph,
) -> Result<usize> {
    let content = fs::read_to_string(file_path).map_err(|source| CodegraphError::ReadFile {
        path: file_path.to_path_buf(),
        source,
    })?;
    let file_rel = repo_relative(file_path, repo_root);
    let mut diag_count: usize = 0;

    // ── First line: must be a header ──────────────────────────────────────────
    let mut lines_iter = content.lines().enumerate();
    let Some((_, first_line)) = lines_iter.next() else {
        let diag_id = project_stable_id(&[
            "project",
            "Diagnostic",
            SOURCE_KIND,
            &file_rel,
            "missing_header_empty",
        ]);
        push_diagnostic(
            graph,
            diag_id,
            Some(&file_rel),
            &format!("[missing_header] '{file_rel}' is empty — first line must be a header"),
            transaction_time,
        );
        return Ok(1);
    };

    // Validate that first line parses as a KindDiscriminator and has kind=header
    let kind_disc: KindDiscriminator = match serde_json::from_str(first_line) {
        Ok(k) => k,
        Err(e) => {
            let diag_id = project_stable_id(&[
                "project",
                "Diagnostic",
                SOURCE_KIND,
                &file_rel,
                "missing_header_parse_error",
            ]);
            push_diagnostic(
                graph,
                diag_id,
                Some(&file_rel),
                &format!("[missing_header] first line of '{file_rel}' is not valid JSON: {e}"),
                transaction_time,
            );
            return Ok(1);
        }
    };

    if kind_disc.kind != "header" {
        let diag_id = project_stable_id(&[
            "project",
            "Diagnostic",
            SOURCE_KIND,
            &file_rel,
            "missing_header_wrong_kind",
        ]);
        push_diagnostic(
            graph,
            diag_id,
            Some(&file_rel),
            &format!(
                "[missing_header] first line of '{file_rel}' has kind='{}', expected 'header'",
                kind_disc.kind
            ),
            transaction_time,
        );
        diag_count += 1;
        return Ok(diag_count);
    }

    let header: HeaderLine = match serde_json::from_str(first_line) {
        Ok(h) => h,
        Err(e) => {
            let diag_id = project_stable_id(&[
                "project",
                "Diagnostic",
                SOURCE_KIND,
                &file_rel,
                "missing_header_fields",
            ]);
            push_diagnostic(
                graph,
                diag_id,
                Some(&file_rel),
                &format!("[missing_header] header line of '{file_rel}' has invalid fields: {e}"),
                transaction_time,
            );
            return Ok(1);
        }
    };

    // schema_version must be 1
    if header.schema_version != 1 {
        let diag_id = project_stable_id(&[
            "project",
            "Diagnostic",
            SOURCE_KIND,
            &file_rel,
            "unsupported_schema_version",
        ]);
        push_diagnostic(
            graph,
            diag_id,
            Some(&file_rel),
            &format!(
                "[unsupported_schema_version] '{file_rel}' has schema_version={}, expected 1",
                header.schema_version
            ),
            transaction_time,
        );
        diag_count += 1;
        return Ok(diag_count);
    }

    // project_slug must match filename stem
    let file_stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if header.project_slug != file_stem {
        let diag_id = project_stable_id(&[
            "project",
            "Diagnostic",
            SOURCE_KIND,
            &file_rel,
            "project_slug_mismatch",
        ]);
        push_diagnostic(
            graph,
            diag_id,
            Some(&file_rel),
            &format!(
                "[project_slug_mismatch] project_slug='{}' does not match filename stem '{file_stem}' in '{file_rel}'",
                header.project_slug
            ),
            transaction_time,
        );
        diag_count += 1;
        return Ok(diag_count);
    }

    // ── Parse remaining lines ─────────────────────────────────────────────────
    // Two-pass: first collect all records, validating parents and identity
    // constraints as we go (ordering enforced in first pass), then emit graph
    // nodes in the second pass.

    // local_id → kind string (for duplicate-kind detection)
    let mut seen_local_ids: HashMap<String, &'static str> = HashMap::new();
    // local_id → stable graph record ID for tasks
    let mut task_ids: HashMap<String, String> = HashMap::new();
    // local_id → stable graph record ID for ACs (built during first pass)
    let mut ac_ids: HashMap<String, String> = HashMap::new();
    // AC identity fields: local_id → (parent_task_local_id, ordinal)
    let mut ac_identity: HashMap<String, (String, u32)> = HashMap::new();
    // ExternalLink identity fields: local_id → (system, system_native_id)
    let mut link_identity: HashMap<String, (String, String)> = HashMap::new();
    // Source-link refinements: task local_id → refinement fields
    let mut src_link_refinements: HashMap<String, SrcLinkRefinement> = HashMap::new();
    // Ordered list of successfully-parsed records
    let mut parsed: Vec<(usize, ParsedRecord)> = Vec::new();

    for (line_idx, line) in lines_iter {
        if line.trim().is_empty() {
            continue;
        }
        let raw = line.as_bytes().to_vec();

        // Parse kind discriminator
        let disc: KindDiscriminator = match serde_json::from_str(line) {
            Ok(d) => d,
            Err(e) => {
                let diag_id = project_stable_id(&[
                    "project",
                    "Diagnostic",
                    SOURCE_KIND,
                    &file_rel,
                    "invalid_json",
                    &line_idx.to_string(),
                ]);
                push_diagnostic(
                    graph,
                    diag_id,
                    Some(&file_rel),
                    &format!(
                        "[invalid_json] line {} of '{file_rel}' is not valid JSON: {e}",
                        line_idx + 1
                    ),
                    transaction_time,
                );
                diag_count += 1;
                continue;
            }
        };

        match disc.kind.as_str() {
            "task" => {
                let task: TaskLine = match serde_json::from_str(line) {
                    Ok(t) => t,
                    Err(e) => {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "invalid_task_fields",
                            &line_idx.to_string(),
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[invalid_json] task at line {} of '{file_rel}' has invalid fields: {e}",
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                };

                if let Some(&existing_kind) = seen_local_ids.get(&task.local_id) {
                    if existing_kind != "task" {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "duplicate_local_id_kind_mismatch",
                            &task.local_id,
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[duplicate_local_id_kind_mismatch] local_id='{}' at line {} has kind='task' but was previously seen with kind='{existing_kind}'",
                                task.local_id,
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                    // Same kind = valid revision; fall through
                } else {
                    seen_local_ids.insert(task.local_id.clone(), "task");
                }

                // Compute stable entity ID (same for all revisions of this local_id)
                let identity_handle = source_identity_handle(&file_rel, &task.local_id);
                let task_id = project_stable_id(&[
                    "project",
                    "Task",
                    SOURCE_KIND,
                    &file_rel,
                    &identity_handle,
                ]);
                task_ids.insert(task.local_id.clone(), task_id);

                parsed.push((line_idx, ParsedRecord::Task { line: task, raw }));
            }

            "acceptance_criterion" => {
                let ac: AcLine = match serde_json::from_str(line) {
                    Ok(a) => a,
                    Err(e) => {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "invalid_ac_fields",
                            &line_idx.to_string(),
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[invalid_json] acceptance_criterion at line {} of '{file_rel}' has invalid fields: {e}",
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                };

                // P1: verified ACs without verification_handle are skipped
                if ac.status == "verified" && ac.verification_handle.is_none() {
                    let diag_id = project_stable_id(&[
                        "project",
                        "Diagnostic",
                        SOURCE_KIND,
                        &file_rel,
                        "acceptance_criterion_missing_verification",
                        &ac.local_id,
                    ]);
                    push_diagnostic(
                        graph,
                        diag_id,
                        Some(&file_rel),
                        &format!(
                            "[acceptance_criterion_missing_verification] acceptance_criterion '{}' at line {} has status='verified' but no verification_handle",
                            ac.local_id,
                            line_idx + 1
                        ),
                        transaction_time,
                    );
                    diag_count += 1;
                    continue;
                }

                // Parent-before-child: parent task must have been seen EARLIER
                let Some(parent_task_id) = task_ids.get(&ac.parent_task_local_id).cloned() else {
                    let diag_id = project_stable_id(&[
                        "project",
                        "Diagnostic",
                        SOURCE_KIND,
                        &file_rel,
                        "unresolved_parent_task",
                        &ac.local_id,
                    ]);
                    push_diagnostic(
                        graph,
                        diag_id,
                        Some(&file_rel),
                        &format!(
                            "[unresolved_parent_task] acceptance_criterion '{}' at line {} references unknown task '{}'",
                            ac.local_id,
                            line_idx + 1,
                            ac.parent_task_local_id
                        ),
                        transaction_time,
                    );
                    diag_count += 1;
                    continue;
                };

                if let Some(&existing_kind) = seen_local_ids.get(&ac.local_id) {
                    if existing_kind != "acceptance_criterion" {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "duplicate_local_id_kind_mismatch",
                            &ac.local_id,
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[duplicate_local_id_kind_mismatch] local_id='{}' at line {} has kind='acceptance_criterion' but was previously seen with kind='{existing_kind}'",
                                ac.local_id,
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                    // Same kind = revision; check identity fields
                    if let Some((prev_parent, prev_ordinal)) = ac_identity.get(&ac.local_id)
                        && (*prev_parent != ac.parent_task_local_id || *prev_ordinal != ac.ordinal)
                    {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "revision_identity_mismatch",
                            &ac.local_id,
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[revision_identity_mismatch] acceptance_criterion '{}' at line {} changes identity fields (parent_task_local_id or ordinal)",
                                ac.local_id,
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                } else {
                    seen_local_ids.insert(ac.local_id.clone(), "acceptance_criterion");
                    // Record identity fields on first occurrence
                    ac_identity.insert(
                        ac.local_id.clone(),
                        (ac.parent_task_local_id.clone(), ac.ordinal),
                    );
                    // Compute and store the AC's stable ID for use by external_links
                    let ac_stable_id = project_stable_id(&[
                        "project",
                        "AcceptanceCriterion",
                        SOURCE_KIND,
                        &file_rel,
                        &parent_task_id,
                        &ac.ordinal.to_string(),
                    ]);
                    ac_ids.insert(ac.local_id.clone(), ac_stable_id);
                }

                parsed.push((
                    line_idx,
                    ParsedRecord::AcceptanceCriterion { line: ac, raw },
                ));
            }

            "external_link" => {
                let link: ExternalLinkLine = match serde_json::from_str(line) {
                    Ok(l) => l,
                    Err(e) => {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "invalid_link_fields",
                            &line_idx.to_string(),
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[invalid_json] external_link at line {} of '{file_rel}' has invalid fields: {e}",
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                };

                // Source-link refinement check: collect and skip from parsed
                if link.system == "local_file" {
                    let materialized_native =
                        source_identity_handle(&file_rel, &link.parent_local_id);
                    if link.system_native_id == materialized_native {
                        src_link_refinements.insert(
                            link.parent_local_id.clone(),
                            SrcLinkRefinement {
                                url: link.url.clone(),
                                discovered_at: link.discovered_at.clone(),
                                updated_at: link.updated_at.clone(),
                            },
                        );
                        seen_local_ids.insert(link.local_id.clone(), "external_link");
                        continue;
                    }
                }

                // Parent-before-child: parent must have been seen EARLIER
                let parent_is_task = task_ids.contains_key(&link.parent_local_id);
                let parent_is_ac = ac_ids.contains_key(&link.parent_local_id);
                if !parent_is_task && !parent_is_ac {
                    let diag_id = project_stable_id(&[
                        "project",
                        "Diagnostic",
                        SOURCE_KIND,
                        &file_rel,
                        "unresolved_parent_local_id",
                        &link.local_id,
                    ]);
                    push_diagnostic(
                        graph,
                        diag_id,
                        Some(&file_rel),
                        &format!(
                            "[unresolved_parent_local_id] external_link '{}' at line {} references unknown parent '{}'",
                            link.local_id,
                            line_idx + 1,
                            link.parent_local_id
                        ),
                        transaction_time,
                    );
                    diag_count += 1;
                    continue;
                }

                if let Some(&existing_kind) = seen_local_ids.get(&link.local_id) {
                    if existing_kind != "external_link" {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "duplicate_local_id_kind_mismatch",
                            &link.local_id,
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[duplicate_local_id_kind_mismatch] local_id='{}' at line {} has kind='external_link' but was previously seen with kind='{existing_kind}'",
                                link.local_id,
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                    // Same kind = revision; check identity fields
                    if let Some((prev_system, prev_native_id)) = link_identity.get(&link.local_id)
                        && (*prev_system != link.system || *prev_native_id != link.system_native_id)
                    {
                        let diag_id = project_stable_id(&[
                            "project",
                            "Diagnostic",
                            SOURCE_KIND,
                            &file_rel,
                            "revision_identity_mismatch",
                            &link.local_id,
                        ]);
                        push_diagnostic(
                            graph,
                            diag_id,
                            Some(&file_rel),
                            &format!(
                                "[revision_identity_mismatch] external_link '{}' at line {} changes identity fields (system or system_native_id)",
                                link.local_id,
                                line_idx + 1
                            ),
                            transaction_time,
                        );
                        diag_count += 1;
                        continue;
                    }
                } else {
                    seen_local_ids.insert(link.local_id.clone(), "external_link");
                    // Record identity fields on first occurrence
                    link_identity.insert(
                        link.local_id.clone(),
                        (link.system.clone(), link.system_native_id.clone()),
                    );
                }

                parsed.push((line_idx, ParsedRecord::ExternalLink { line: link, raw }));
            }

            other => {
                let diag_id = project_stable_id(&[
                    "project",
                    "Diagnostic",
                    SOURCE_KIND,
                    &file_rel,
                    "unknown_kind",
                    other,
                    &line_idx.to_string(),
                ]);
                push_diagnostic(
                    graph,
                    diag_id,
                    Some(&file_rel),
                    &format!(
                        "[unknown_kind] line {} of '{file_rel}' has unknown kind='{other}', skipping",
                        line_idx + 1
                    ),
                    transaction_time,
                );
                diag_count += 1;
            }
        }
    }

    // ── Second pass: emit graph nodes ─────────────────────────────────────────
    // Parent validation was done in the first pass; all records in `parsed`
    // have valid parents. The second pass only needs to look up parent IDs to
    // wire edges.
    for (_line_idx, record) in &parsed {
        match record {
            ParsedRecord::Task { line: task, raw } => {
                emit_task_records(
                    graph,
                    task,
                    raw,
                    &file_rel,
                    opts,
                    transaction_time,
                    src_link_refinements.get(&task.local_id),
                );
            }
            ParsedRecord::AcceptanceCriterion { line: ac, raw } => {
                // Parent was validated in first pass; unwrap is safe.
                let parent_task_id = task_ids
                    .get(&ac.parent_task_local_id)
                    .expect("parent validated in first pass");
                emit_ac_record(
                    graph,
                    ac,
                    raw,
                    &file_rel,
                    parent_task_id,
                    opts,
                    transaction_time,
                );
            }
            ParsedRecord::ExternalLink { line: link, raw } => {
                // Parent was validated in first pass; unwrap is safe.
                emit_external_link_record(
                    graph,
                    link,
                    raw,
                    &file_rel,
                    &task_ids,
                    &ac_ids,
                    opts,
                    transaction_time,
                );
            }
        }
    }

    Ok(diag_count)
}

// ── Record emitters ───────────────────────────────────────────────────────────

/// Emit a Task node, its materialized source `ExternalLink`, and the `ExternalHandle` edge.
///
/// When `src_link_refinement` is `Some`, its `url`, `discovered_at`, and
/// `updated_at` fields override the defaults for the materialized source link.
#[allow(clippy::too_many_lines)]
fn emit_task_records(
    graph: &mut Graph,
    task: &TaskLine,
    raw: &[u8],
    file_rel: &str,
    opts: &ImportOptions,
    transaction_time: &str,
    src_link_refinement: Option<&SrcLinkRefinement>,
) {
    let identity_handle = source_identity_handle(file_rel, &task.local_id);
    let task_id = project_stable_id(&["project", "Task", SOURCE_KIND, file_rel, &identity_handle]);
    let full_source_handle = source_handle_for_line(file_rel, &task.local_id, raw);

    // Materialized source ExternalLink
    let src_link_native_id = identity_handle;
    let src_link_id = project_stable_id(&[
        "project",
        "ExternalLink",
        SOURCE_KIND,
        file_rel,
        "local_file",
        &src_link_native_id,
    ]);
    // Explicit refinement wins for url; otherwise default to file:// URL
    let src_link_url =
        src_link_refinement.map_or_else(|| format!("file://{file_rel}"), |r| r.url.clone());

    // Body handle
    let body_str = match &task.body {
        None => String::new(),
        Some(serde_json::Value::String(s)) => (opts.redact)(s),
        Some(other) => (opts.redact)(&other.to_string()),
    };
    let body_h = body_handle_for(&body_str);
    let title = (opts.redact)(&task.title);
    let assignees: Vec<String> = task.assignees.iter().map(|a| (opts.redact)(a)).collect();
    let labels: Vec<String> = task.labels.iter().map(|l| (opts.redact)(l)).collect();

    // Task node
    let mut task_node = GraphRecord::node(
        task_id.clone(),
        NodeKind::Task,
        Some(file_rel.to_owned()),
        None,
        Some(task.local_id.clone()),
        format!("Task: {title}"),
    );
    set_project_base_fields(
        &mut task_node,
        transaction_time,
        Some(&task_id),
        Some(&task.updated_at),
        Some(&full_source_handle),
    );
    if let GraphRecord::Node {
        title: t,
        body_handle,
        source_kind,
        source_external_link_id,
        assignees: a,
        labels: l,
        priority,
        status,
        ..
    } = &mut task_node
    {
        *t = Some(title);
        *body_handle = Some(Box::new(body_h));
        *source_kind = Some(SOURCE_KIND.to_owned());
        *source_external_link_id = Some(src_link_id.clone());
        *a = Some(assignees);
        *l = Some(labels);
        *priority = Some(task.priority.clone());
        *status = Some(task.status.clone());
    }
    graph.push(task_node);

    // Materialized source ExternalLink node
    // Refinement wins for valid_time (updated_at) and discovered_at
    let src_link_valid_time =
        src_link_refinement.map_or(task.updated_at.as_str(), |r| r.updated_at.as_str());
    let src_link_discovered_at =
        src_link_refinement.map_or_else(|| task.updated_at.clone(), |r| r.discovered_at.clone());
    let src_link_source_handle = source_handle_for_line(file_rel, &task.local_id, raw);
    let mut src_link_node = GraphRecord::node(
        src_link_id.clone(),
        NodeKind::ExternalLink,
        Some(file_rel.to_owned()),
        None,
        None,
        format!("ExternalLink: local_file:{src_link_native_id}"),
    );
    set_project_base_fields(
        &mut src_link_node,
        transaction_time,
        Some(&src_link_id),
        Some(src_link_valid_time),
        Some(&src_link_source_handle),
    );
    if let GraphRecord::Node {
        source_kind,
        system,
        url,
        system_native_id,
        discovered_at,
        ..
    } = &mut src_link_node
    {
        *source_kind = Some(SOURCE_KIND.to_owned());
        *system = Some("local_file".to_owned());
        *url = Some(src_link_url);
        *system_native_id = Some(src_link_native_id);
        *discovered_at = Some(src_link_discovered_at);
    }
    graph.push(src_link_node);

    // Task → ExternalLink edge (ExternalHandle)
    let edge_id = project_stable_id(&["project", "edge", "ExternalHandle", &task_id, &src_link_id]);
    graph.push(GraphRecord::Edge {
        id: edge_id,
        schema_version: PROJECT_SCHEMA_VERSION,
        label: EdgeLabel::ExternalHandle,
        source: task_id.clone(),
        target: src_link_id,
        confidence: None,
        temporal: None,
        summary: format!(
            "Task '{}' has local-file source ExternalLink",
            task.local_id
        ),
        producer: None,
    });
}

/// Emit an `AcceptanceCriterion` node and its `OwnedByTask` edge.
fn emit_ac_record(
    graph: &mut Graph,
    ac: &AcLine,
    raw: &[u8],
    file_rel: &str,
    parent_task_id: &str,
    opts: &ImportOptions,
    transaction_time: &str,
) {
    let ac_id = project_stable_id(&[
        "project",
        "AcceptanceCriterion",
        SOURCE_KIND,
        file_rel,
        parent_task_id,
        &ac.ordinal.to_string(),
    ]);
    let full_source_handle = source_handle_for_line(file_rel, &ac.local_id, raw);
    let text = (opts.redact)(&ac.text);

    let mut ac_node = GraphRecord::node(
        ac_id.clone(),
        NodeKind::AcceptanceCriterion,
        Some(file_rel.to_owned()),
        None,
        Some(ac.local_id.clone()),
        format!("AcceptanceCriterion: {text}"),
    );
    set_project_base_fields(
        &mut ac_node,
        transaction_time,
        Some(&ac_id),
        Some(&ac.updated_at),
        Some(&full_source_handle),
    );
    if let GraphRecord::Node {
        text: t,
        source_kind,
        parent_task_id: ptid,
        ordinal,
        status,
        ..
    } = &mut ac_node
    {
        *t = Some(text);
        *source_kind = Some(SOURCE_KIND.to_owned());
        *ptid = Some(parent_task_id.to_owned());
        *ordinal = Some(ac.ordinal);
        *status = Some(ac.status.clone());
    }
    graph.push(ac_node);

    // AcceptanceCriterion → Task edge (OwnedByTask)
    let edge_id = project_stable_id(&["project", "edge", "OwnedByTask", &ac_id, parent_task_id]);
    graph.push(GraphRecord::Edge {
        id: edge_id,
        schema_version: PROJECT_SCHEMA_VERSION,
        label: EdgeLabel::OwnedByTask,
        source: ac_id,
        target: parent_task_id.to_owned(),
        confidence: None,
        temporal: None,
        summary: format!("AcceptanceCriterion '{}' owned by task", ac.local_id),
        producer: None,
    });
}

/// Emit an explicit `ExternalLink` node and its `ExternalHandle` edge.
///
/// The parent can be a task or an acceptance criterion; `task_ids` and
/// `ac_ids` are both consulted. Source-link refinements are handled in the
/// first pass and never reach this function.
#[allow(clippy::too_many_arguments)]
fn emit_external_link_record(
    graph: &mut Graph,
    link: &ExternalLinkLine,
    raw: &[u8],
    file_rel: &str,
    task_ids: &HashMap<String, String>,
    ac_ids: &HashMap<String, String>,
    opts: &ImportOptions,
    transaction_time: &str,
) {
    let link_id = project_stable_id(&[
        "project",
        "ExternalLink",
        SOURCE_KIND,
        file_rel,
        &link.system,
        &link.system_native_id,
    ]);
    let full_source_handle = source_handle_for_line(file_rel, &link.local_id, raw);
    let url = (opts.redact)(&link.url);

    let mut link_node = GraphRecord::node(
        link_id.clone(),
        NodeKind::ExternalLink,
        Some(file_rel.to_owned()),
        None,
        None,
        format!("ExternalLink: {}:{}", link.system, link.system_native_id),
    );
    set_project_base_fields(
        &mut link_node,
        transaction_time,
        Some(&link_id),
        Some(&link.updated_at),
        Some(&full_source_handle),
    );
    if let GraphRecord::Node {
        source_kind,
        system,
        url: u,
        system_native_id,
        discovered_at,
        ..
    } = &mut link_node
    {
        *source_kind = Some(SOURCE_KIND.to_owned());
        *system = Some(link.system.clone());
        *u = Some(url);
        *system_native_id = Some(link.system_native_id.clone());
        *discovered_at = Some(link.discovered_at.clone());
    }
    graph.push(link_node);

    // Parent → ExternalLink edge (ExternalHandle)
    // Parent was validated in first pass; it is either a task or an AC.
    let parent_stable_id = task_ids
        .get(&link.parent_local_id)
        .or_else(|| ac_ids.get(&link.parent_local_id))
        .expect("parent validated in first pass");
    let edge_id = project_stable_id(&[
        "project",
        "edge",
        "ExternalHandle",
        parent_stable_id,
        &link_id,
    ]);
    graph.push(GraphRecord::Edge {
        id: edge_id,
        schema_version: PROJECT_SCHEMA_VERSION,
        label: EdgeLabel::ExternalHandle,
        source: parent_stable_id.clone(),
        target: link_id,
        confidence: None,
        temporal: None,
        summary: format!("'{}' has ExternalLink", link.parent_local_id),
        producer: None,
    });
}
