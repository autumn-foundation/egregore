//! `eg schema export` / `eg schema list` (issue #226).
//!
//! Emits JSON Schema (draft 2020-12) documents for every persisted
//! `(domain, kind, schema_version)` record contract, derived from the Rust
//! types in `src/ir.rs` via the [`crate::schema_export`] registry. See
//! `docs/cli/schema.md` and `docs/schema/README.md`.

use super::*;
use crate::schema_export::{all_schema_refs, schema_document, schema_json};
use serde_json::json;

/// Emit schema documents for the selected contracts.
///
/// Selection is the intersection of the given filters; with no filters every
/// addressable tuple is emitted as a JSON array, and with a single matching
/// tuple the document is emitted bare. `--out <dir>` writes one
/// `<domain>.<record_type>.<kind>.v<version>.schema.json` file per tuple and
/// prints a JSON summary instead. Unknown filter values fail with exit code 2
/// and a machine-readable `{"ok": false, ...}` diagnostic on stderr.
///
/// # Errors
///
/// Returns an error when the output directory cannot be created or a schema
/// file cannot be written.
pub(crate) fn schema_export_cmd(
    domain: Option<&str>,
    kind: Option<&str>,
    schema_version: Option<u32>,
    out: Option<&Path>,
) -> Result<()> {
    let matching: Vec<_> = all_schema_refs()
        .into_iter()
        .filter(|r| domain.is_none_or(|d| r.domain == d))
        .filter(|r| kind.is_none_or(|k| r.kind == k))
        .filter(|r| schema_version.is_none_or(|v| r.version == v))
        .collect();

    if matching.is_empty() {
        let detail = match (domain, kind, schema_version) {
            (Some(d), Some(k), Some(v)) => {
                format!("unknown contract (domain={d}, kind={k}, schema_version={v})")
            }
            (Some(d), Some(k), None) => format!("unknown contract (domain={d}, kind={k})"),
            (Some(d), None, _) => format!("unknown domain {d}"),
            (None, Some(k), _) => format!("unknown kind {k}"),
            (None, None, Some(v)) => format!("unknown schema_version {v}"),
            (None, None, None) => "no contracts addressable".to_owned(),
        };
        let diagnostic = serde_json::to_string(&json!({
            "ok": false,
            "code": "schema_unknown_contract",
            "detail": detail,
        }))
        .unwrap_or_else(|_| "{\"ok\":false}".to_owned());
        eprintln!("{diagnostic}");
        std::process::exit(2);
    }

    if let Some(dir) = out {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create output directory {}", dir.display()))?;
        let mut files: Vec<String> = Vec::with_capacity(matching.len());
        for spec in &matching {
            let Some(text) = schema_json(spec.domain, spec.kind, spec.version) else {
                continue;
            };
            let file_name = spec.file_name();
            let path = dir.join(&file_name);
            std::fs::write(&path, text)
                .with_context(|| format!("failed to write schema file {}", path.display()))?;
            files.push(file_name);
        }
        files.sort();
        let summary = json!({
            "ok": true,
            "out": dir.display().to_string(),
            "count": files.len(),
            "files": files,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).context("failed to serialize schema summary")?
        );
        return Ok(());
    }

    if matching.len() == 1 {
        let spec = matching[0];
        if let Some(doc) = schema_document(spec.domain, spec.kind, spec.version) {
            println!(
                "{}",
                serde_json::to_string_pretty(&doc).context("failed to serialize schema")?
            );
        } else {
            eprintln!(
                "{{\"ok\": false, \"code\": \"schema_unknown_contract\", \
                  \"detail\": \"contract unresolvable\"}}"
            );
            std::process::exit(2);
        }
    } else {
        let docs: Vec<serde_json::Value> = matching
            .iter()
            .filter_map(|spec| schema_document(spec.domain, spec.kind, spec.version))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&docs).context("failed to serialize schemas")?
        );
    }
    Ok(())
}

/// List every addressable contract tuple as JSON.
///
/// # Errors
///
/// Returns an error when the listing cannot be serialized.
pub(crate) fn schema_list_cmd(domain: Option<&str>) -> Result<()> {
    let entries: Vec<serde_json::Value> = all_schema_refs()
        .into_iter()
        .filter(|r| domain.is_none_or(|d| r.domain == d))
        .map(|r| {
            json!({
                "domain": r.domain,
                "record_type": r.record_type.as_str(),
                "kind": r.kind,
                "schema_version": r.version,
                "$id": r.id(),
                "file": r.file_name(),
            })
        })
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&entries).context("failed to serialize schema list")?
    );
    Ok(())
}
