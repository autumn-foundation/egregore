use super::*;

// ---------------------------------------------------------------------------
// implementors query (issue #133)
// ---------------------------------------------------------------------------

/// One implementor row (newline-delimited JSON, one object per line).
#[derive(Serialize)]
pub(crate) struct ImplementorResult<'a> {
    /// Stable record ID of the `impl` Symbol (the citable handle).
    record_id: &'a str,
    schema_version: u32,
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol_kind: Option<&'a str>,
    /// Impl display name, e.g. `impl Renderable for Circle`.
    name: &'a str,
    /// Qualified name of the implementing type, resolved from the impl.
    implementing_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    implementing_type_record_id: Option<&'a str>,
    /// `resolved` (type maps to a live Symbol record) or `parsed_only`.
    implementing_type_resolution: &'a str,
    /// Stable record ID of the resolved trait this row belongs to.
    trait_record_id: &'a str,
    trait_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    /// Stable record ID of the connecting `IMPLEMENTS` edge.
    edge_record_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    /// Always `local_traits_only` — see the implementors completeness contract.
    completeness: &'static str,
}

impl PrintText for ImplementorResult<'_> {
    fn as_text(&self) -> String {
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        let commit = self.git_commit.map_or(String::new(), |c| format!(" [{c}]"));
        format!(
            "{} ({}) implements {} @ {path}:{line}{commit} (completeness: {})",
            self.implementing_type, self.name, self.trait_name, self.completeness
        )
    }
}

/// Explicit signal emitted when a trait resolves but has zero recorded
/// implementors — distinguishable from both a no-match and a bare empty list.
#[derive(Serialize)]
pub(crate) struct ImplementorsZeroResult<'a> {
    ok: bool,
    code: &'static str,
    trait_record_id: &'a str,
    trait_name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    implementors_recorded: usize,
    completeness: &'static str,
    note: &'static str,
}

impl PrintText for ImplementorsZeroResult<'_> {
    fn as_text(&self) -> String {
        format!(
            "{}: 0 implementors recorded (completeness: {}) — {}",
            self.trait_name, self.completeness, self.note
        )
    }
}

/// Prints the implementors no-match/stale-handle envelope on stdout and exits 2.
pub(crate) fn exit_implementors_unresolved(
    code: &str,
    name: &str,
    at: Option<&str>,
    as_of: Option<&str>,
) -> ! {
    let mut error = serde_json::json!({
        "code": code,
        "name": name,
        "message": format!(
            "no live Symbol record matches `{name}`; implementors are \
             edge-backed only for locally-defined traits, so an external/std \
             trait resolves to no record and its impls carry no IMPLEMENTS edge"
        ),
    });
    if code == "stale_handle" {
        error["message"] = serde_json::json!(format!(
            "`{name}` resolves only to tombstoned record(s); the trait is no \
             longer live in this store"
        ));
    }
    if let Some(prefix) = at {
        error["at"] = serde_json::json!(prefix);
    }
    if let Some(instant) = as_of {
        error["as_of"] = serde_json::json!(instant);
    }
    let envelope = serde_json::json!({ "ok": false, "error": error });
    println!("{envelope}");
    std::process::exit(2);
}

/// Prints the non-target-kind diagnostic on stdout and exits 2: the name
/// resolved only to symbols whose kind can never be an `IMPLEMENTS` target.
pub(crate) fn exit_implementors_non_target(name: &str, kinds: &[String]) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": {
            "code": "no_match",
            "name": name,
            "non_target_symbol_kinds": kinds,
            "message": format!(
                "`{name}` resolves only to symbol kind(s) [{}] that can never \
                 be an IMPLEMENTS target; implementors accepts \
                 trait/class/interface/type-defining handles — pass a \
                 canonical record ID to inspect a specific record",
                kinds.join(", ")
            ),
        },
    });
    println!("{envelope}");
    std::process::exit(2);
}

/// Drops name-resolved candidates whose symbol kind can never be an
/// `IMPLEMENTS` target. Exits with the non-target diagnostic when the filter
/// empties a previously non-empty match set.
pub(crate) fn retain_implements_target_candidates(matches: &mut Vec<&GraphRecord>, name: &str) {
    if matches.is_empty() {
        return;
    }
    let mut kinds: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    matches.retain(|r| {
        if let GraphRecord::Node { symbol_kind, .. } = r {
            if query::is_implements_target_kind(symbol_kind.as_deref()) {
                true
            } else {
                if let Some(kind) = symbol_kind.as_deref() {
                    kinds.insert(kind.to_owned());
                }
                false
            }
        } else {
            false
        }
    });
    if matches.is_empty() {
        let kinds: Vec<String> = kinds.into_iter().collect();
        exit_implementors_non_target(name, &kinds);
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) fn query_implementors_cmd(
    records: &[GraphRecord],
    name: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // A canonical record-ID handle stays valid under the temporal selectors
    // (PR #296 review): the pinning helpers match by symbol name, so resolve
    // the ID to its name first and afterwards keep only records carrying that
    // exact ID. All records sharing one stable ID carry the same name.
    let id_symbol_name: Option<&str> = records.iter().find_map(|r| {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: Some(symbol_name),
            ..
        } = r
            && id == name
        {
            Some(symbol_name.as_str())
        } else {
            None
        }
    });
    let lookup_name = id_symbol_name.unwrap_or(name);

    // Resolve the trait candidates for the requested time view. Each
    // candidate carries its own optional commit pin so the implementor set is
    // read at the same point the trait was resolved at.
    let candidates: Vec<&GraphRecord>;
    if let Some(prefix) = at {
        // Repository-scoped commit-prefix ambiguity check (same contract as
        // `eg query symbol --at`).
        let matching_commits: std::collections::BTreeSet<&str> = records
            .iter()
            .filter(|r| {
                repo_scope.is_none_or(|repo| record_belongs_to_repo_for_commit_scan(r, index, repo))
            })
            .filter_map(|r| temporal_commit_if_prefix(r, prefix))
            .collect();
        if matching_commits.len() > 1 {
            eprintln!(
                "error: ambiguous commit prefix `{prefix}` matches {} commits",
                matching_commits.len()
            );
            std::process::exit(1);
        }
        let mut matches = query::symbols_at_commit(records, lookup_name, prefix);
        if id_symbol_name.is_some() {
            // Record-ID handle: keep only the pinned record(s) with that ID.
            matches.retain(|r| r.id() == name);
        } else {
            retain_implements_target_candidates(&mut matches, name);
        }
        if let Some(repo) = repo_scope {
            matches.retain(|r| index.owner_of(r.id()) == Some(repo));
        } else {
            let groups: std::collections::BTreeSet<Option<&str>> =
                matches.iter().map(|r| index.owner_of(r.id())).collect();
            if groups.len() > 1 {
                exit_ambiguous_repository(&groups);
            }
        }
        if matches.is_empty() {
            exit_implementors_unresolved("no_match", name, at, as_of);
        }
        candidates = matches;
    } else if let Some(instant) = as_of {
        // A record-ID handle selects among the records carrying that exact ID
        // (one symbol identity), bypassing the same-name best-per-repository
        // pick which could land on a different record.
        let resolved = if id_symbol_name.is_some() {
            query::symbol_as_of_valid_time_by_id(records, name, instant)
        } else {
            // Every same-named candidate at the instant, so the ambiguity
            // rule (all candidates, labeled by trait_record_id) holds under
            // --as-of too — never a best-per-repository pick.
            query::symbols_as_of_valid_time_per_symbol(records, name, instant)
        };
        let resolved = match (resolved, repo_scope) {
            (Ok(mut results), Some(repo)) => {
                results.retain(|r| index.owner_of(r.id()) == Some(repo));
                Ok(results)
            }
            (other, _) => other,
        };
        let resolved = match resolved {
            Ok(mut results) => {
                if id_symbol_name.is_none() {
                    retain_implements_target_candidates(&mut results, name);
                }
                Ok(results)
            }
            other => other,
        };
        match resolved {
            Err(msg) => {
                eprintln!("error: {msg}");
                std::process::exit(1);
            }
            Ok(results) => {
                if results.is_empty() {
                    exit_implementors_unresolved("no_match", name, at, as_of);
                }
                if repo_scope.is_none() {
                    let groups: std::collections::BTreeSet<Option<&str>> =
                        results.iter().map(|r| index.owner_of(r.id())).collect();
                    if groups.len() > 1 {
                        exit_ambiguous_repository(&groups);
                    }
                }
                candidates = results;
            }
        }
    } else {
        match query::implementors_resolve_trait(records, name, index, repo_scope) {
            query::ImplementorsResolution::Resolved(resolved) => candidates = resolved,
            query::ImplementorsResolution::Stale => {
                exit_implementors_unresolved("stale_handle", name, at, as_of);
            }
            query::ImplementorsResolution::NonTargetKinds { kinds } => {
                exit_implementors_non_target(name, &kinds);
            }
            query::ImplementorsResolution::NoMatch => {
                exit_implementors_unresolved("no_match", name, at, as_of);
            }
        }
    }

    let pinned = at.is_some() || as_of.is_some();
    // Validate every candidate's pin BEFORE emitting any output: an
    // anchorless later candidate must never leave earlier candidates' rows
    // on stdout next to the error envelope — a machine-readable failure is
    // never accompanied by partial output.
    let mut pins: Vec<Option<&str>> = Vec::with_capacity(candidates.len());
    for trait_record in &candidates {
        let pin_commit = if pinned {
            if let GraphRecord::Node {
                temporal: Some(t), ..
            } = trait_record
            {
                Some(t.git_commit.as_str())
            } else {
                // The resolved snapshot carries only a node-level valid_time
                // (current-tree / refresh records): the implementor set
                // cannot be pinned to a commit, and silently answering with
                // the unpinned current view would leak later-added
                // implementors into the past. Refuse machine-readably —
                // matching the sibling convention (transitive-callers
                // requires commit history for temporal selectors).
                let mut error = serde_json::json!({
                    "code": "no_commit_anchor",
                    "name": name,
                    "trait_record_id": trait_record.id(),
                    "message": format!(
                        "the snapshot of `{name}` resolved at this instant \
                         carries no commit anchor (current-tree/refresh \
                         records); a pinned implementor set requires \
                         scan-history commit snapshots — re-run against a \
                         scan-history store"
                    ),
                });
                if let Some(prefix) = at {
                    error["at"] = serde_json::json!(prefix);
                }
                if let Some(instant) = as_of {
                    error["as_of"] = serde_json::json!(instant);
                }
                let envelope = serde_json::json!({ "ok": false, "error": error });
                println!("{envelope}");
                std::process::exit(2);
            }
        } else {
            None
        };
        pins.push(pin_commit);
    }

    for (trait_record, pin_commit) in candidates.into_iter().zip(pins) {
        let rows = query::implementors_rows_for(records, trait_record, pin_commit, index);
        let (trait_name, trait_path, trait_span) = if let GraphRecord::Node {
            name: trait_name,
            repo_relative_path,
            span,
            ..
        } = trait_record
        {
            (
                trait_name.as_deref().unwrap_or(""),
                repo_relative_path.as_deref(),
                *span,
            )
        } else {
            ("", None, None)
        };
        let trait_repository_id = index.owner_of(trait_record.id());

        if rows.is_empty() {
            // A resolved trait with zero recorded implementors is an explicit
            // machine-readable signal, never a silent empty answer (AC4).
            let zero = ImplementorsZeroResult {
                ok: true,
                code: "zero_implementors_recorded",
                trait_record_id: trait_record.id(),
                trait_name,
                repo_relative_path: trait_path,
                span: trait_span,
                repository_id: trait_repository_id,
                repository: trait_repository_id.and_then(|repo| index.display_of(repo)),
                implementors_recorded: 0,
                completeness: query::IMPLEMENTORS_COMPLETENESS,
                note: query::IMPLEMENTORS_COMPLETENESS_NOTE,
            };
            print_result(&zero, format)?;
            continue;
        }

        for lead in &rows {
            let GraphRecord::Node {
                id,
                schema_version,
                name: impl_name,
                repo_relative_path,
                span,
                symbol_kind,
                temporal,
                ..
            } = lead.impl_record
            else {
                continue;
            };
            let edge_git_commit = if let GraphRecord::Edge {
                temporal: Some(t), ..
            } = lead.edge
            {
                Some(t.git_commit.as_str())
            } else {
                None
            };
            let repository_id = index.owner_of(id);
            let row = ImplementorResult {
                record_id: id,
                schema_version: *schema_version,
                kind: "Symbol",
                symbol_kind: symbol_kind.as_deref(),
                name: impl_name.as_deref().unwrap_or(""),
                implementing_type: &lead.implementing_type,
                implementing_type_record_id: lead.implementing_type_record_id.as_deref(),
                implementing_type_resolution: lead.implementing_type_resolution,
                trait_record_id: trait_record.id(),
                trait_name,
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                edge_record_id: lead.edge.id(),
                // Pinned rows carry the queried commit: the impl-block
                // fallback can keep an edge whose stored temporal commit is
                // the store's LATEST edge version, and labeling the row with
                // it would misstate the historical provenance.
                git_commit: pin_commit
                    .or(edge_git_commit)
                    .or_else(|| temporal.as_ref().map(|t| t.git_commit.as_str())),
                repository_id,
                repository: repository_id.and_then(|repo| index.display_of(repo)),
                completeness: query::IMPLEMENTORS_COMPLETENESS,
            };
            print_result(&row, format)?;
        }
    }
    Ok(())
}
