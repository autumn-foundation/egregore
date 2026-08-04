use std::collections::BTreeSet;

use super::*;
use crate::verification_freshness::{
    self as freshness, FRESHNESS_VERDICTS_DIAGNOSTIC, NO_STALE_DIAGNOSTIC,
    NO_VERIFICATION_CODE_CITATIONS_DIAGNOSTIC, NO_VERIFICATION_RECORDS_DIAGNOSTIC,
    STALE_PRESENT_DIAGNOSTIC, VerificationFreshnessEntry,
};

// ---------------------------------------------------------------------------
// verification-freshness lane (issue #111)
// ---------------------------------------------------------------------------

/// One stable machine-readable diagnostic in the response.
#[derive(Serialize)]
pub(crate) struct VerificationFreshnessDiagnosticJson {
    code: &'static str,
    detail: String,
}

/// Top-level verification-freshness response envelope.
#[derive(Serialize)]
pub(crate) struct VerificationFreshnessResponse<'a> {
    ok: bool,
    stale_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
    /// Verdict tally across `current`/`stale`/`unresolved`/`unanchored`.
    counts: std::collections::BTreeMap<&'static str, usize>,
    /// `true` when `--limit` truncated the (already sorted) verdict set.
    truncated: bool,
    /// Stable diagnostics; always carries at least one code (AC7).
    diagnostics: Vec<VerificationFreshnessDiagnosticJson>,
    /// Per-citation freshness verdicts, deterministically ordered.
    verdicts: &'a [VerificationFreshnessEntry],
}

/// How a supplied `scope` handle resolved against the in-store code items.
enum ScopeResolution {
    /// No scope handle was supplied.
    None,
    /// Resolved to this set of code-handle record IDs plus an optional path
    /// prefix (path-prefix scopes filter by path, not just by ID, so a
    /// citation whose target record fell out of the loaded slice can still
    /// match on its recorded `repo_relative_path`).
    Matched {
        ids: BTreeSet<String>,
        path_prefix: Option<String>,
    },
    /// A name matched more than one live code item.
    Ambiguous { candidates: Vec<String> },
    /// A path-shaped handle matched no in-store code item.
    NotFoundPath,
    /// A name/id-shaped handle matched no in-store code item.
    NoMatch,
}

/// Resolves an optional `scope` handle (record ID, exact `Symbol`/`File`
/// name, or a segment-aware repo-relative path prefix) against the code
/// handles present in `records`.
///
/// When `repo_scope` is set, candidates are pre-filtered to that repository
/// (mirroring `verification_coverage`'s strict `code_in_scope` convention: no
/// unattributed-code fallback, since code handles ARE normally attributed) —
/// otherwise a symbol name unique within the selected repository but
/// duplicated in another repository would be wrongly reported `ambiguous`,
/// and a name that exists only outside the selected repository would wrongly
/// resolve at all.
///
/// Name and path-prefix matching are further restricted to the SAME live
/// code-handle set the core module's own citation classification uses
/// (`freshness::live_code_handle_ids`): the history-inclusive store this
/// lane always reads can retain a tombstoned/superseded handle alongside a
/// live one that happens to share a name, and without this a scope that
/// uniquely names the LIVE handle would be wrongly reported `ambiguous_scope`
/// (or match a dead handle no citation can ever target). An explicit record
/// ID is deliberately exempt from this filter: it must keep resolving a
/// historical/tombstoned handle directly, so scoping to it still surfaces
/// that citation's `unresolved` verdict.
fn resolve_scope(
    records: &[GraphRecord],
    repo_scope: Option<&str>,
    index: &query::RepositoryIndex,
    scope: Option<&str>,
) -> ScopeResolution {
    let Some(scope) = scope else {
        return ScopeResolution::None;
    };
    let live_ids = freshness::live_code_handle_ids(records);
    let mut by_id: BTreeSet<String> = BTreeSet::new();
    let mut by_name: BTreeSet<String> = BTreeSet::new();
    let mut by_path_prefix: BTreeSet<String> = BTreeSet::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            name,
            repo_relative_path,
            ..
        } = record
        else {
            continue;
        };
        // Deliberately narrower than the core module's shared
        // `is_code_handle_kind` (which also allows Module/Import for issue
        // #85's broader domain): `docs/schema/verification.md` §6 closes
        // FAILED_ON/MENTIONS_SYMBOL/TOUCHED_FILE targets to Symbol/File only,
        // so a Module/Import scope handle can never match an actual citation
        // in this lane -- reporting it `no_match`/`scope_not_found` here is
        // more honest than a silently-empty `Matched`.
        if !matches!(
            kind,
            crate::ir::NodeKind::Symbol | crate::ir::NodeKind::File
        ) {
            continue;
        }
        if repo_scope.is_some_and(|r| index.owner_of(id) != Some(r)) {
            continue;
        }
        if id == scope {
            by_id.insert(id.clone());
        }
        let is_live = live_ids.contains(id.as_str());
        if is_live && name.as_deref() == Some(scope) {
            by_name.insert(id.clone());
        }
        if is_live
            && let Some(path) = repo_relative_path
            && query::path_is_under_prefix(path, scope)
        {
            by_path_prefix.insert(id.clone());
        }
    }
    if !by_id.is_empty() {
        return ScopeResolution::Matched {
            ids: by_id,
            path_prefix: None,
        };
    }
    if by_name.len() > 1 {
        return ScopeResolution::Ambiguous {
            candidates: by_name.into_iter().collect(),
        };
    }
    if !by_name.is_empty() {
        return ScopeResolution::Matched {
            ids: by_name,
            path_prefix: None,
        };
    }
    if !by_path_prefix.is_empty() {
        return ScopeResolution::Matched {
            ids: by_path_prefix,
            path_prefix: Some(scope.to_owned()),
        };
    }
    if scope.contains('/') {
        ScopeResolution::NotFoundPath
    } else {
        ScopeResolution::NoMatch
    }
}

/// Handles `eg query verification-freshness [scope] --graph <path> | --data-dir <dir>
/// [--repo <selector>] [--repo-path <dir>] [--stale-only] [--limit <n>] [--format text|json]`.
///
/// Strictly read-only: computes verdicts from records already loaded (and, when
/// `--repo-path` is given, from a live read-only file read of the artefact
/// path) and never creates, modifies, or deletes anything. Output carries only
/// record IDs, hashes, handles, spans, statuses, and edge labels — never raw
/// stdout/stderr/summary text (AC10).
#[allow(clippy::too_many_lines)]
pub(crate) fn query_verification_freshness_cmd(
    records: &[GraphRecord],
    repo_scope: Option<&str>,
    scope: Option<&str>,
    repo_path: Option<&std::path::Path>,
    stale_only: bool,
    limit: Option<usize>,
    format: OutputFormat,
) -> Result<()> {
    let index = query::RepositoryIndex::build(records);
    let scope_resolution = resolve_scope(records, repo_scope, &index, scope);
    let scope_error_code = match &scope_resolution {
        ScopeResolution::NotFoundPath => Some("scope_not_found"),
        ScopeResolution::NoMatch => Some("no_match"),
        ScopeResolution::Ambiguous { .. }
        | ScopeResolution::None
        | ScopeResolution::Matched { .. } => None,
    };
    if let Some(code) = scope_error_code {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": code,
                "scope": scope,
                "message": "no code item in the selected store slice matches this scope handle",
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }
    if let ScopeResolution::Ambiguous { candidates } = &scope_resolution {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "ambiguous_scope",
                "scope": scope,
                "candidates": candidates,
                "message": "scope name matches more than one live code item",
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(1);
    }

    let all = freshness::verification_freshness(records, repo_path);
    // Repository scope: keep only records/citations attributable to the
    // selected repository (or unattributable, matching the verification-coverage
    // convention of treating unattributed verification records as in-scope).
    let all = match repo_scope {
        None => all,
        Some(repo_id) => {
            // A row whose OWN cited handle resolves to a definitive owner
            // (the common case: MENTIONS_SYMBOL/TOUCHED_FILE/FAILED_ON to a
            // live, attributed code node) is scoped precisely by that owner.
            // Two row shapes carry NO definitive owner of their own and must
            // borrow attribution from the SAME record's other citations
            // instead of passing through under every `--repo` scope:
            //   - the synthetic `source_artifact` citation (no
            //     `target_record_id` at all; its owning verification record
            //     is typically unattributed too, since no writer links a
            //     TestRun/CIStatus node to a Repository) -- unhandled, this
            //     would let `--repo-path` hash one repository's checkout
            //     against another repository's TestRun;
            //   - an `unresolved`/`unanchored` row whose target is absent or
            //     itself unattributed -- unhandled, this attributes an
            //     unrelated repository's dangling reference to every scope.
            // Borrowed attribution requires a UNIQUE owner: a record citing
            // code in two repositories must not resolve its ownerless rows
            // to EITHER one (that would let each scope hash a different
            // repository's checkout, or falsely narrow an ambiguous
            // reference to one repo). With zero attributable citations
            // anywhere, fall back to the record's own (usually absent)
            // direct attribution -- the original lenient pass-through.
            let mut ver_repo_owners: std::collections::BTreeMap<String, BTreeSet<String>> =
                std::collections::BTreeMap::new();
            for e in &all {
                if let Some(target) = e.cited_handle.target_record_id.as_deref()
                    && let Some(owner) = index.owner_of(target)
                {
                    ver_repo_owners
                        .entry(e.verification_record_id.clone())
                        .or_default()
                        .insert(owner.to_owned());
                }
            }
            all.into_iter()
                .filter(|e| {
                    if let Some(target) = e.cited_handle.target_record_id.as_deref()
                        && let Some(owner) = index.owner_of(target)
                    {
                        return owner == repo_id;
                    }
                    let owners = ver_repo_owners.get(e.verification_record_id.as_str());
                    // A `source_artifact` row is NEVER let through on the
                    // lenient "no attribution anywhere" default below: an
                    // artifact-only record (e.g. `capture-tests` run
                    // without `--graph`, carrying zero code citations) would
                    // otherwise pass under EVERY `--repo` scope, letting
                    // `--repo-path` hash an unrelated repository's checkout
                    // and fabricate a verdict. Without a unique borrowed
                    // owner, exclude it outright.
                    if e.cited_handle.relation == "source_artifact" {
                        return owners.is_some_and(|o| o.len() == 1 && o.contains(repo_id));
                    }
                    if let Some(owners) = owners
                        && !owners.is_empty()
                    {
                        return owners.len() == 1 && owners.contains(repo_id);
                    }
                    index
                        .owner_of(&e.verification_record_id)
                        .is_none_or(|o| o == repo_id)
                })
                .collect()
        }
    };
    // Scope filter: keep rows whose cited handle matches the resolved scope
    // (by record ID, or — for a path-prefix scope — by recorded path, so a
    // citation whose target fell out of the loaded slice can still match).
    let all = match &scope_resolution {
        ScopeResolution::None => all,
        ScopeResolution::Matched { ids, path_prefix } => all
            .into_iter()
            .filter(|e| {
                e.cited_handle
                    .target_record_id
                    .as_deref()
                    .is_some_and(|id| ids.contains(id))
                    || path_prefix.as_deref().is_some_and(|prefix| {
                        e.cited_handle
                            .repo_relative_path
                            .as_deref()
                            .is_some_and(|p| query::path_is_under_prefix(p, prefix))
                    })
            })
            .collect(),
        ScopeResolution::Ambiguous { .. }
        | ScopeResolution::NotFoundPath
        | ScopeResolution::NoMatch => unreachable!("handled above"),
    };

    let counts = freshness::verdict_counts(&all);
    let verdicts = if stale_only {
        freshness::stale_only(all)
    } else {
        all
    };

    let mut verdicts = verdicts;
    let limit = limit.unwrap_or(freshness::VERIFICATION_FRESHNESS_DEFAULT_LIMIT);
    let mut diagnostics: Vec<VerificationFreshnessDiagnosticJson> = Vec::new();
    let mut truncated = false;
    if verdicts.len() > limit {
        let total = verdicts.len();
        verdicts.truncate(limit);
        truncated = true;
        diagnostics.push(VerificationFreshnessDiagnosticJson {
            code: "results_truncated",
            detail: format!("showing {limit} of {total} rows; raise --limit to see the rest"),
        });
    }

    if !freshness::has_verification_records(records) {
        diagnostics.push(VerificationFreshnessDiagnosticJson {
            code: NO_VERIFICATION_RECORDS_DIAGNOSTIC,
            detail: "the store records no TestRun/CIStatus/BenchmarkRun/CoverageReport/\
                 ProofResult nodes; verification freshness cannot be assessed"
                .to_owned(),
        });
    } else if verdicts.is_empty() && !stale_only {
        diagnostics.push(VerificationFreshnessDiagnosticJson {
            code: NO_VERIFICATION_CODE_CITATIONS_DIAGNOSTIC,
            detail: "verification records exist but none cite a code handle via \
                 FAILED_ON/MENTIONS_SYMBOL/TOUCHED_FILE, and no --repo-path artifact \
                 citation applied"
                .to_owned(),
        });
    } else if stale_only {
        let code = if verdicts.is_empty() {
            NO_STALE_DIAGNOSTIC
        } else {
            STALE_PRESENT_DIAGNOSTIC
        };
        diagnostics.push(VerificationFreshnessDiagnosticJson {
            code,
            detail: format!(
                "{} stale/unresolved of {} evaluated citation(s)",
                verdicts.len(),
                {
                    let mut total = 0;
                    for v in counts.values() {
                        total += v;
                    }
                    total
                }
            ),
        });
    } else {
        diagnostics.push(VerificationFreshnessDiagnosticJson {
            code: FRESHNESS_VERDICTS_DIAGNOSTIC,
            detail: format!("{} citation(s) evaluated", verdicts.len()),
        });
    }

    match format {
        OutputFormat::Json => {
            let response = VerificationFreshnessResponse {
                ok: true,
                stale_only,
                repo_scope,
                scope,
                counts,
                truncated,
                diagnostics,
                verdicts: &verdicts,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize verification-freshness report")?;
            println!("{output}");
        }
        OutputFormat::Text => {
            println!(
                "Verification-freshness verdicts (freshness leads only -- never a \
                 re-judgment of pass/fail)."
            );
            for entry in &verdicts {
                let cited = entry
                    .cited_handle
                    .repo_relative_path
                    .as_deref()
                    .unwrap_or("?");
                println!(
                    "- {} [{}] status={} verdict={} <- {}({})",
                    entry.verification_record_id,
                    entry.verification_kind,
                    entry.status.as_deref().unwrap_or("?"),
                    entry.verdict.as_str(),
                    entry.cited_handle.relation,
                    cited
                );
            }
            println!(
                "counts: current={} stale={} unresolved={} unanchored={}",
                counts.get("current").unwrap_or(&0),
                counts.get("stale").unwrap_or(&0),
                counts.get("unresolved").unwrap_or(&0),
                counts.get("unanchored").unwrap_or(&0),
            );
            for d in &diagnostics {
                println!("diagnostic: {}: {}", d.code, d.detail);
            }
        }
    }
    Ok(())
}
