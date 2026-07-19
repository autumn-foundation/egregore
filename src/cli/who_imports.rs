use super::*;

// ---------------------------------------------------------------------------
// who-imports query — `eg query who-imports <module-path>` (issue #444)
//
// A read-only importer lookup over the Import nodes the extractors already
// mint. NDJSON envelope + one row per importing file, mirroring the `deps`
// (#123) / `path` (#225) output shape. The segment-aware matching and the
// `--crate` unification boundary live in `query::who_imports`.
// ---------------------------------------------------------------------------

/// One importing-file row emitted as its own NDJSON line.
#[derive(Serialize)]
pub(crate) struct WhoImportsRowJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    repo_relative_path: &'a str,
    span: Option<SourceSpan>,
    import_path: &'a str,
    trust: &'static str,
}

/// Summary envelope emitted as the first NDJSON line.
#[derive(Serialize)]
pub(crate) struct WhoImportsHeaderJson<'a> {
    ok: bool,
    query_path: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    crate_name: Option<&'a str>,
    total_importers: usize,
    disclaimer: &'static str,
}

pub(crate) const WHO_IMPORTS_DISCLAIMER: &str = "Rows are the files whose recorded `use` declarations name a module path with the query as a \
     segment-aware prefix (`foo::bar` matches `foo::bar::Baz`, never `foo::barbell`). Only \
     extractor-minted Import nodes are considered, so a doc-comment or string mention of the path \
     is invisible here. Without `--crate`, a `crate::`-relative import and an absolute \
     `<crate>::` import are distinct (the graph carries no per-file owning-crate name); pass \
     `--crate <name>` to unify a leading `crate::` with that crate name. Rows are import-site \
     LEADS, never proof the imported item is used.";

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_who_imports_cmd(
    records: &[GraphRecord],
    module_path: &str,
    crate_name: Option<&str>,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let result = match query::who_imports(records, module_path, crate_name, index, repo_scope) {
        Ok(result) => result,
        Err(err) => {
            // Malformed module path → machine-readable diagnostic on stderr, exit 1.
            let diag = serde_json::json!({
                "code": err.code(),
                "module_path": module_path,
                "message": "module path must be a non-empty `::`-separated path with no empty or \
                            whitespace-bearing segments",
            });
            eprintln!("{diag}");
            std::process::exit(1);
        }
    };

    let rows_json: Vec<WhoImportsRowJson<'_>> = result
        .rows
        .iter()
        .map(|row| WhoImportsRowJson {
            record_id: row.record_id,
            schema_version: row.schema_version,
            repo_relative_path: row.repo_relative_path,
            span: row.span,
            import_path: row.import_path,
            trust: "source_fact",
        })
        .collect();

    if result.is_empty() {
        // A well-formed query with zero importers is an explicit `no_match`
        // success-shaped diagnostic (exit 2), mirroring `query subsystem`.
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "query_path": result.query_path,
                "message": "no live import declares this module path as a segment-aware prefix",
            },
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let header = WhoImportsHeaderJson {
        ok: true,
        query_path: &result.query_path,
        crate_name: result.crate_name.as_deref(),
        total_importers: rows_json.len(),
        disclaimer: WHO_IMPORTS_DISCLAIMER,
    };

    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&header).context("failed to serialize who-imports header")?
            );
            for row in &rows_json {
                println!(
                    "{}",
                    serde_json::to_string(row).context("failed to serialize who-imports row")?
                );
            }
        }
        OutputFormat::Text => {
            // Human-readable only; the exact format is unstable by contract.
            let scope = result
                .crate_name
                .as_deref()
                .map_or_else(String::new, |c| format!(" (--crate {c})"));
            println!(
                "importers of {}{scope}: {} — import-site leads, not proof of use",
                result.query_path,
                rows_json.len(),
            );
            for row in &rows_json {
                let location = row.span.map_or_else(
                    || row.repo_relative_path.to_owned(),
                    |s| format!("{}:{}", row.repo_relative_path, s.start_line),
                );
                println!("  {location}  {}", row.import_path);
            }
        }
    }
    Ok(())
}
