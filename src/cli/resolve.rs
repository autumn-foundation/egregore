//! `eg query resolve <record_id>` (issue #160): dereference a cited record-id
//! handle back to its live source record with a typed drift verdict.
//!
//! This is the read-back half of the citation contract documented in
//! `docs/cli/query.md`: ids emitted by `query symbol|file|semantic`
//! round-trip to byte-identical `repo_relative_path` + `span` at the same
//! commit, and the lane says exactly what happened when they no longer do.
//!
//! Out of scope (issue text): non-codegraph id domains (refused as
//! `unsupported_handle_domain`, never misread), fuzzy nearest-match recovery,
//! migration/auto-healing, and cross-store identity recovery.

use super::*;

use crate::query::resolve::{
    ResolveHandleError, ResolveVerdict, ResolvedRecordFields, drift_verdict_for,
    find_current_record, find_record_as_of, find_record_at_commit, resolved_record_fields,
    validate_resolve_handle,
};

/// The single-answer JSON body for `eg query resolve` (issue #160).
#[derive(Debug, Serialize)]
struct ResolveAnswer<'a> {
    record_id: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    valid_time: Option<&'a str>,
    verdict: ResolveVerdict,
    /// Where the record lives in the current view (`drifted` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    current_repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_span: Option<SourceSpan>,
    /// Why the verdict is what it is (`drifted` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corpus_mode: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corpus_mode_source: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    corpus_disclaimer: Option<String>,
}

/// Prints the machine-readable `ok: false` envelope for a resolve input
/// error and exits 1. The handle is echoed only for handle-shape errors —
/// after validation there is no unvalidated input left to echo.
pub(crate) fn fail_resolve_input(code: &str, record_id: Option<&str>, message: &str) -> ! {
    let mut error = serde_json::json!({ "code": code, "message": message });
    if let Some(id) = record_id {
        error["record_id"] = serde_json::json!(id);
    }
    println!("{}", serde_json::json!({ "ok": false, "error": error }));
    std::process::exit(1);
}

/// Prints the structured dangling-handle envelope (issue #160, acceptance
/// criterion 4) and exits 2. The envelope names the id and says plainly that
/// no fuzzy guess was made — this lane never guesses.
pub(crate) fn fail_resolve_dangling(record_id: &str) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": {
            "code": "dangling_handle",
            "record_id": record_id,
            "message": "no record with this id exists in the requested view; \
                        the handle is dangling (this lane never makes a fuzzy guess)",
        },
    });
    println!("{envelope}");
    std::process::exit(2);
}

/// Prints one dereferenced record with its drift verdict. `current` is the
/// record's current-view counterpart (for `drifted`); `detail` explains the
/// verdict; `corpus` is the `(mode, source, disclaimer)` disclosure triple —
/// `None` on the daemon path, which answers from the live store directly.
#[allow(clippy::too_many_lines)]
pub(crate) fn print_resolve_answer<'a>(
    fields: ResolvedRecordFields<'a>,
    record_id: &'a str,
    verdict: ResolveVerdict,
    current: Option<(Option<&'a str>, Option<SourceSpan>)>,
    detail: Option<String>,
    corpus: Option<(&'a str, &'a str, String)>,
    format: OutputFormat,
) -> Result<()> {
    let (current_path, current_span) = current.unwrap_or((None, None));
    let (corpus_mode, corpus_mode_source, corpus_disclaimer) = corpus
        .map_or((None, None, None), |(mode, source, disclaimer)| {
            (Some(mode), Some(source), Some(disclaimer))
        });
    let answer = ResolveAnswer {
        record_id,
        kind: fields.kind,
        name: fields.name,
        repo_relative_path: fields.repo_relative_path,
        span: fields.span,
        git_commit: fields.git_commit,
        valid_time: fields.valid_time,
        verdict,
        current_repo_relative_path: current_path,
        current_span,
        detail,
        corpus_mode,
        corpus_mode_source,
        corpus_disclaimer,
    };
    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string(&answer)?);
        }
        OutputFormat::Text => {
            println!("{}", resolve_text_line(&answer));
        }
    }
    Ok(())
}

/// One human-readable line for `--format text`. Not script-stable: parse the
/// JSON form instead.
fn resolve_text_line(answer: &ResolveAnswer<'_>) -> String {
    use std::fmt::Write as _;
    let sanitize = crate::embeddings::sanitized_handle;
    let name = answer
        .name
        .map_or_else(String::new, |name| format!(" `{}`", sanitize(name)));
    let at = match (answer.repo_relative_path, answer.span) {
        (Some(path), Some(span)) => format!(" @ {}:{}", sanitize(path), span.start_line),
        (Some(path), None) => format!(" @ {}", sanitize(path)),
        (None, _) => String::new(),
    };
    let commit = answer
        .git_commit
        .map_or_else(String::new, |commit| format!(" [{commit}]"));
    let verdict = match answer.verdict {
        ResolveVerdict::Valid => "valid",
        ResolveVerdict::Drifted => "drifted",
        ResolveVerdict::Dangling => "dangling",
    };
    let mut line = format!(
        "{verdict} {}{} ({}){at}{commit}",
        answer.record_id, name, answer.kind
    );
    if answer.verdict == ResolveVerdict::Drifted {
        match (answer.current_repo_relative_path, answer.current_span) {
            (Some(path), Some(span)) => {
                let _ = write!(line, " — now at {}:{}", sanitize(path), span.start_line);
            }
            _ => {
                line.push_str(" — gone from the current view");
            }
        }
    }
    line
}

/// Implements `eg query resolve` (issue #160).
#[allow(clippy::too_many_lines)]
pub(crate) fn query_resolve_cmd(
    record_id: &str,
    graph: Option<&Path>,
    data_dir: Option<PathBuf>,
    #[cfg(feature = "embedded-aletheiadb")] daemon: bool,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Validate the handle before touching any store: a malformed id is an
    // input error (exit 1), never a dangling verdict (exit 2).
    if let Err(handle_error) = validate_resolve_handle(record_id) {
        let (code, message) = match handle_error {
            ResolveHandleError::Malformed => (
                "malformed_handle",
                format!(
                    "record_id '{record_id}' is not a well-formed codegraph handle \
                     (expected codegraph:v<version>:<suffix>)"
                ),
            ),
            ResolveHandleError::UnsupportedDomain => (
                "unsupported_handle_domain",
                format!(
                    "record_id '{record_id}' is a non-codegraph handle, which query \
                     resolve does not dereference (issue #160 is codegraph-scoped)"
                ),
            ),
        };
        fail_resolve_input(code, Some(record_id), &message);
    }

    #[cfg(feature = "embedded-aletheiadb")]
    if daemon {
        // Clap guarantees `--data-dir` whenever `--daemon` is present.
        let dir = data_dir.expect("clap requires --data-dir with --daemon");
        return query_resolve_via_daemon(record_id, &dir, at, as_of, format);
    }

    // Transport selection mirrors the `query file` temporal lane: the ById
    // fast path applies only to unscoped --graph reads, where a whole-file
    // scan is pure waste; history reads and --data-dir reads use the
    // whole-corpus loaders so commit topology and head-anchoring see every
    // record.
    let selector_active = at.is_some() || as_of.is_some();
    let data_dir = resolve_query_data_dir(graph, data_dir);
    let records = match (graph, data_dir.as_deref()) {
        (Some(graph_path), None) => {
            let selector = if selector_active {
                crate::graph_index::Selector::Whole
            } else {
                crate::graph_index::Selector::ById(record_id.to_owned())
            };
            load_records_from_jsonl_selected(graph_path, &selector)?
        }
        (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    };

    let deleted = current_deleted_ids(&records);
    // Issue #472: targets of active repository-eviction tombstones are
    // suppressed from this lane, including their temporal snapshots. Ordinary
    // `forget` tombstones keep the issue #231 temporal exemption.
    let evicted = crate::repo_evict::active_eviction_tombstoned_ids(&records);

    // The "current view" comparison is ALWAYS the head-anchored corpus,
    // even on the temporal path: `resolve_current_state_corpus` with a
    // temporal pin reports `commit_pinned` and returns no filtered corpus,
    // so the pin must not leak into the current-view lookup (that would
    // compare a historical snapshot against itself).
    let index = query::RepositoryIndex::build(&records);
    let (_, _, filtered) = resolve_current_state_corpus(&records, &index, false, false, false)?;
    let current_view: &[GraphRecord] = filtered.as_deref().unwrap_or(&records);

    if !selector_active {
        // Current view: head-anchor over history stores (issue #456), then
        // dereference. A record that survives here is live by construction,
        // so the verdict is `valid`.
        let found = find_current_record(current_view, record_id, &deleted, &evicted);
        let Some(record) = found else {
            fail_resolve_dangling(record_id);
        };
        let (mode, source, disclaimer) = disclose_head_anchored_corpus(&records, false);
        print_resolve_answer(
            resolved_record_fields(record),
            record_id,
            ResolveVerdict::Valid,
            None,
            None,
            Some((mode, source, disclaimer)),
            format,
        )?;
        return Ok(());
    }

    // Temporal view: find the pinned snapshot, then compare against the
    // current view for the drift verdict.
    let pinned: Option<&GraphRecord> = match (at, as_of) {
        (Some(commit_prefix), None) => {
            match find_record_at_commit(&records, record_id, commit_prefix, &evicted) {
                Ok(found) => found,
                Err(message) => fail_resolve_input("ambiguous_commit", None, &message),
            }
        }
        (None, Some(as_of)) => match find_record_as_of(&records, record_id, as_of, &evicted) {
            Ok(found) => found,
            Err(message) => fail_resolve_input("invalid_as_of", None, &message),
        },
        // Clap's conflicts_with makes every other combination unreachable.
        _ => fail_resolve_input(
            "unsupported_combination",
            None,
            "--at and --as-of are mutually exclusive",
        ),
    };
    let Some(pinned) = pinned else {
        fail_resolve_dangling(record_id);
    };
    let current = find_current_record(current_view, record_id, &deleted, &evicted);
    let (verdict, detail) = drift_verdict_for(pinned, current);
    let current_coords = current.map(|record| {
        let fields = resolved_record_fields(record);
        (fields.repo_relative_path, fields.span)
    });
    let (mode, source, disclaimer) = disclose_head_anchored_corpus(&records, true);
    print_resolve_answer(
        resolved_record_fields(pinned),
        record_id,
        verdict,
        current_coords,
        detail,
        Some((mode, source, disclaimer)),
        format,
    )?;
    Ok(())
}

/// Resolves a handle through the running daemon (issue #160): the daemon's
/// `get_records` verb reads the current view by id, suppressing actively
/// tombstoned records (issue #231). Eviction is applied client-side to match
/// The `resolve_record` verb answer, deserialized. Field-for-field the CLI's
/// [`ResolveAnswer`]; `verdict` drives the exit code.
#[cfg(feature = "embedded-aletheiadb")]
#[derive(serde::Deserialize)]
struct DaemonResolveAnswer {
    kind: String,
    name: Option<String>,
    repo_relative_path: Option<String>,
    span: Option<SourceSpan>,
    git_commit: Option<String>,
    valid_time: Option<String>,
    verdict: String,
    detail: Option<String>,
    current_repo_relative_path: Option<String>,
    current_span: Option<SourceSpan>,
}

/// Resolves through the daemon's `resolve_record` verb (issue #160): the
/// verdict is computed server-side, so `--at`/`--as-of` work here exactly as
/// they do on the local transports. Tombstone handling (forget exemption,
/// eviction suppression) lives in the verb; no client-side store copy.
#[cfg(feature = "embedded-aletheiadb")]
fn query_resolve_via_daemon(
    record_id: &str,
    data_dir: &Path,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;
    let params = serde_json::json!({
        "record_id": record_id,
        "at": at,
        "as_of": as_of,
    });
    let values = match client.query_verb("resolve_record", &params, None) {
        Ok(values) => values,
        Err(error) => {
            if let Some(rejection) = error.downcast_ref::<crate::daemon::DaemonQueryRejection>() {
                match rejection.code.as_str() {
                    "dangling_handle" => fail_resolve_dangling(record_id),
                    "malformed_handle"
                    | "unsupported_handle_domain"
                    | "ambiguous_commit_prefix" => fail_resolve_input(
                        &rejection.code,
                        Some(record_id),
                        rejection.message.as_str(),
                    ),
                    _ => {}
                }
            }
            return Err(error);
        }
    };
    let raw = values
        .into_iter()
        .next()
        .unwrap_or_else(|| fail_resolve_dangling(record_id));
    let answer: DaemonResolveAnswer = serde_json::from_value(raw)
        .with_context(|| "daemon resolve_record returned a malformed answer")?;
    let verdict = match answer.verdict.as_str() {
        "valid" => ResolveVerdict::Valid,
        "drifted" => ResolveVerdict::Drifted,
        // The verb answers dangling as an error; a `dangling` verdict string
        // here would be a protocol violation — treat it as dangling anyway.
        "dangling" => fail_resolve_dangling(record_id),
        other => anyhow::bail!("daemon resolve_record returned unknown verdict '{other}'"),
    };
    let fields = ResolvedRecordFields {
        kind: &answer.kind,
        name: answer.name.as_deref(),
        repo_relative_path: answer.repo_relative_path.as_deref(),
        span: answer.span,
        git_commit: answer.git_commit.as_deref(),
        valid_time: answer.valid_time.as_deref(),
    };
    // The daemon answers from the live store and discloses no corpus mode;
    // the verb already computed the drift verdict server-side.
    print_resolve_answer(
        fields,
        record_id,
        verdict,
        Some((
            answer.current_repo_relative_path.as_deref(),
            answer.current_span,
        )),
        answer.detail,
        None,
        format,
    )?;
    Ok(())
}
