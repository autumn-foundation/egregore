use super::*;

// ---------------------------------------------------------------------------
// query diagnostics — extractor-coverage-gap triage lane (issue #246)
// ---------------------------------------------------------------------------

/// One extractor-coverage-gap diagnostic row: a persisted `Diagnostic` graph
/// node carrying both a repo-relative path and a span, so the row is always
/// resolvable to the flagged source region.
#[derive(Serialize)]
pub(crate) struct DiagnosticGapJson<'a> {
    record_id: &'a str,
    schema_version: u32,
    kind: &'static str,
    /// The diagnostic subject: the macro invocation (`println!`), the
    /// unresolved dispatch (`unresolved_dispatch: Trait::method`), or the
    /// unresolved-call display text.
    name: &'a str,
    /// Human summary, e.g. `unsupported macro invocation println!`.
    summary: &'a str,
    repo_relative_path: &'a str,
    span: SourceSpan,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    /// History-backed rows only.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    /// Stable `Repository` record ID owning this row; absent when the store
    /// carries no repository topology for the record (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    /// Human-usable repository identity handle (e.g. `owner/name`).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Every row is a deterministic extractor fact, advisory by contract.
    trust: &'static str,
    /// Corpus this row was read from (issue #456). The lane emits bare NDJSON
    /// rows with no summary envelope, so the disclosure rides each row — the
    /// same contract as `eg query symbol`.
    corpus_mode: &'static str,
    corpus_mode_source: &'static str,
    corpus_disclaimer: String,
}

impl PrintText for DiagnosticGapJson<'_> {
    fn as_text(&self) -> String {
        // A repo-relative path is a real filesystem path, and on Unix that can
        // contain a newline or an ESC. Sanitize it for the TEXT render so no
        // row can forge an output line or drive the reader's terminal. Never
        // truncated — a truncated path stops being a citation — and
        // `--format json` still carries the exact bytes.
        let path = crate::embeddings::sanitized_handle(self.repo_relative_path);
        format!(
            "{} @ {path}:{} [{}]",
            self.summary, self.span.start_line, self.record_id
        )
    }
}

/// Machine-readable empty reason: the scope exists and the extractor flagged
/// zero coverage gaps in it — a citable positive fact, not a no-match.
pub(crate) const DIAGNOSTICS_CLEAN_REASON: &str = "no_gaps_in_scope";

/// Machine-readable disclaimer for the clean-scope marker: absence of gap
/// markers is not proof the code is fully understood for any other purpose.
pub(crate) const DIAGNOSTICS_CLEAN_MESSAGE: &str = "No extraction-gap diagnostics in scope";

/// Projects one gap `Diagnostic` record onto its citable row. Returns `None`
/// for any record outside the lane's closed set (never reached when fed by
/// [`query::diagnostic_gap_records`], but the gate is explicit so a future
/// caller cannot widen the lane by accident).
fn diagnostic_gap_row<'a>(
    record: &'a GraphRecord,
    index: &'a query::RepositoryIndex,
    corpus_mode: query::CorpusMode,
    corpus_mode_source: query::CorpusModeSource,
    corpus_disclaimer: &str,
) -> Option<DiagnosticGapJson<'a>> {
    let GraphRecord::Node {
        id,
        kind: NodeKind::Diagnostic,
        schema_version,
        repo_relative_path: Some(path),
        span: Some(span),
        name,
        summary,
        language,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    let repository_id = index.owner_of(id);
    Some(DiagnosticGapJson {
        record_id: id,
        schema_version: *schema_version,
        kind: "Diagnostic",
        name: name.as_deref().unwrap_or(""),
        summary,
        repo_relative_path: path,
        span: *span,
        language: language.as_deref(),
        git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        repository_id,
        repository: repository_id.and_then(|repo| index.display_of(repo)),
        trust: "source_fact",
        corpus_mode: corpus_mode.as_str(),
        corpus_mode_source: corpus_mode_source.as_str(),
        corpus_disclaimer: corpus_disclaimer.to_owned(),
    })
}

/// Validates the `--file` scope for `query diagnostics`: a path with no live
/// `File` node in the selected store slice is a scope error (exit 2,
/// machine-readable) — a typo is never a silent "clean". Mirrors `query
/// file`'s existence check, including the repository-scope gate so a
/// colliding path in another repository does not read as present.
fn check_file_scope(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    file: Option<&str>,
    deleted: &std::collections::BTreeSet<&str>,
) -> Result<()> {
    let Some(path) = file else {
        return Ok(());
    };
    let file_exists = records.iter().any(|record| {
        let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path,
            temporal,
            ..
        } = record
        else {
            return false;
        };
        repo_relative_path.as_deref() == Some(path)
            && (temporal.is_some() || !deleted.contains(id.as_str()))
            && repo_scope.is_none_or(|repo| index.owner_of(id) == Some(repo))
    });
    if !file_exists {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "unknown_file",
                "file": path,
                "message": format!("no file `{path}` in the selected store slice"),
            },
        });
        println!(
            "{}",
            serde_json::to_string(&envelope).context("failed to serialize scope error")?
        );
        std::process::exit(2);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_diagnostics_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    file: Option<&str>,
    at_head: bool,
    all_history: bool,
    format: OutputFormat,
) -> Result<()> {
    let deleted = current_deleted_ids(records);
    // Issue #472: targets of ACTIVE repository-eviction tombstones are
    // suppressed from this current-state lane, including their temporal
    // snapshots. Ordinary `forget` tombstones keep the issue #231 temporal
    // exemption — only eviction tombstones suppress history.
    let evicted = crate::repo_evict::active_eviction_tombstoned_ids(records);

    // `--file` scoping: a path with no live `File` node in the selected store
    // slice is a scope error (exit 2, machine-readable) — a typo is never a
    // silent "clean". Mirrors `query file`'s existence check, including the
    // repository-scope gate so a colliding path in another repository does not
    // read as present.
    check_file_scope(records, index, repo_scope, file, &deleted)?;

    // Corpus-mode selection (issue #456): head-anchor by default over a
    // scan-history store so a gap resolved before HEAD does not reappear;
    // `--all-history` opts into the union (the same gap then appears once per
    // commit, distinguished by `git_commit`). The HEAD-anchor pre-filter runs
    // BEFORE the pure inventory fn.
    let (corpus_mode, corpus_mode_source, filtered) =
        resolve_current_state_corpus(records, index, false, at_head, all_history)?;
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);
    let corpus_disclaimer = corpus_mode.disclaimer().to_owned();

    // The pure fn returns the closed set in canonical order; the filters below
    // only narrow, so the surviving rows stay canonically ordered.
    let rows: Vec<DiagnosticGapJson<'_>> = query::diagnostic_gap_records(records)
        .into_iter()
        .filter(|record| {
            if let GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            } = record
                && evicted.contains(id.as_str())
            {
                return false;
            }
            if let GraphRecord::Node {
                id, temporal: None, ..
            } = record
            {
                !deleted.contains(id.as_str())
            } else {
                true
            }
        })
        .filter(|record| repo_scope.is_none_or(|repo| index.owner_of(record.id()) == Some(repo)))
        .filter_map(|record| {
            if let Some(path) = file {
                let matches_path = match record {
                    GraphRecord::Node {
                        repo_relative_path: Some(record_path),
                        ..
                    } => record_path == path,
                    _ => false,
                };
                if !matches_path {
                    return None;
                }
            }
            diagnostic_gap_row(
                record,
                index,
                corpus_mode,
                corpus_mode_source,
                &corpus_disclaimer,
            )
        })
        .collect();

    if rows.is_empty() {
        // A scope with zero gap diagnostics is an explicit success: absence of
        // gaps is itself a citable fact, distinct from the no-match / error
        // envelopes (exits 1/2). The required epistemic caveat rides the
        // marker and the lane doc: "no gaps in scope" is not proof the code is
        // fully understood for any other purpose, only that the extractor
        // flagged nothing it could not parse.
        match format {
            OutputFormat::Json => {
                let marker = serde_json::json!({
                    "ok": true,
                    "lane": "diagnostics",
                    "diagnostics": [],
                    "empty_reason": DIAGNOSTICS_CLEAN_REASON,
                    "message": DIAGNOSTICS_CLEAN_MESSAGE,
                    "scope": { "repo": repo_scope, "file": file },
                    "corpus_mode": corpus_mode.as_str(),
                    "corpus_mode_source": corpus_mode_source.as_str(),
                    "corpus_disclaimer": corpus_disclaimer,
                });
                println!(
                    "{}",
                    serde_json::to_string(&marker).context("failed to serialize empty result")?
                );
            }
            OutputFormat::Text => {
                println!("# {DIAGNOSTICS_CLEAN_REASON}: {DIAGNOSTICS_CLEAN_MESSAGE}");
            }
        }
        return Ok(());
    }

    for row in &rows {
        print_result(row, format)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gap_record() -> GraphRecord {
        GraphRecord::syntax_node(
            "codegraph:v5:abc123".to_owned(),
            NodeKind::Diagnostic,
            "src/main.rs".to_owned(),
            SourceSpan {
                start_byte: 120,
                end_byte: 143,
                start_line: 5,
                end_line: 5,
                start_column: None,
                end_column: None,
            },
            "println!".to_owned(),
            "rust",
            "unsupported macro invocation println!".to_owned(),
        )
    }

    #[test]
    fn gap_row_projects_stable_citable_fields() {
        let record = gap_record();
        let index = query::RepositoryIndex::build(std::slice::from_ref(&record));
        let row = diagnostic_gap_row(
            &record,
            &index,
            query::CorpusMode::SingleSnapshot,
            query::CorpusModeSource::Default,
            "single snapshot",
        )
        .expect("gap record should project");
        assert_eq!(row.record_id, "codegraph:v5:abc123");
        assert_eq!(row.kind, "Diagnostic");
        assert_eq!(row.name, "println!");
        assert_eq!(row.summary, "unsupported macro invocation println!");
        assert_eq!(row.repo_relative_path, "src/main.rs");
        assert_eq!(row.span.start_byte, 120);
        assert_eq!(row.span.end_byte, 143);
        assert_eq!(row.span.start_line, 5);
        assert_eq!(row.trust, "source_fact");
        let json = serde_json::to_string(&row).expect("row should serialize");
        for field in [
            "record_id",
            "schema_version",
            "kind",
            "name",
            "summary",
            "repo_relative_path",
            "span",
            "trust",
        ] {
            assert!(json.contains(field), "row JSON should carry {field}");
        }
    }

    #[test]
    fn gap_row_rejects_non_gap_records() {
        let marker = GraphRecord::node(
            "codegraph:v5:todo".to_owned(),
            NodeKind::DebtMarker,
            Some("src/main.rs".to_owned()),
            None,
            Some("todo".to_owned()),
            "TODO: something".to_owned(),
        );
        let index = query::RepositoryIndex::build(std::slice::from_ref(&marker));
        assert!(
            diagnostic_gap_row(
                &marker,
                &index,
                query::CorpusMode::SingleSnapshot,
                query::CorpusModeSource::Default,
                "single snapshot",
            )
            .is_none(),
            "a DebtMarker must never project as a gap row"
        );
    }

    #[test]
    fn gap_text_render_cites_path_line_and_record() {
        let record = gap_record();
        let index = query::RepositoryIndex::build(std::slice::from_ref(&record));
        let row = diagnostic_gap_row(
            &record,
            &index,
            query::CorpusMode::SingleSnapshot,
            query::CorpusModeSource::Default,
            "single snapshot",
        )
        .expect("gap record should project");
        let text = row.as_text();
        assert!(
            text.contains("unsupported macro invocation println!"),
            "text should carry the summary: {text}"
        );
        assert!(
            text.contains("src/main.rs:5"),
            "text should cite path:line: {text}"
        );
        assert!(
            text.contains("codegraph:v5:abc123"),
            "text should cite the record id: {text}"
        );
        assert_eq!(text.lines().count(), 1, "one row must stay one line");
    }
}
