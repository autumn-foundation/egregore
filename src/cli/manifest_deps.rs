use super::*;

// ---------------------------------------------------------------------------
// manifest-declared dependency query (issue #180)
// ---------------------------------------------------------------------------

/// Standing disclaimer on every `query manifest-deps` response: rows are declaration
/// facts parsed from manifests, never usage, build, or resolvability proof.
pub(crate) const MANIFEST_DEPS_DISCLAIMER: &str = "Declared-dependency facts parsed from Cargo manifests and the nearest Cargo.lock; never proof the dependency is used in code, builds, or resolves.";

/// One declared-dependency row in the `query manifest-deps` response.
#[derive(serde::Serialize)]
pub(crate) struct ManifestDepsDeclarationJson<'a> {
    record_id: &'a str,
    /// Owning repository display label; absent when the record cannot be
    /// attributed (e.g. a legacy graph without a `Repository` node).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    name: &'a str,
    /// Manifest key when the entry was declared under a `package = "…"`
    /// rename; absent for plain declarations.
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_as: Option<&'a str>,
    dependency_kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_requirement: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_version: Option<&'a str>,
    resolution: &'a str,
    declaring_package: &'a str,
    manifest_path: &'a str,
    schema_version: u32,
}

/// One stable machine-readable diagnostic in the `query manifest-deps` response.
#[derive(serde::Serialize)]
pub(crate) struct ManifestDepsDiagnosticJson<'a> {
    code: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<&'a str>,
    /// Citing record ID for record-backed diagnostics (`skipped_manifest`).
    #[serde(skip_serializing_if = "Option::is_none")]
    record_id: Option<&'a str>,
    /// Owning repository display label for record-backed diagnostics; absent
    /// when the record cannot be attributed (legacy graphs).
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
}

/// Top-level `query manifest-deps` response envelope.
#[derive(serde::Serialize)]
pub(crate) struct ManifestDepsResponse<'a> {
    ok: bool,
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    count: usize,
    disclaimer: &'a str,
    declarations: Vec<ManifestDepsDeclarationJson<'a>>,
    diagnostics: Vec<ManifestDepsDiagnosticJson<'a>>,
}

/// Sort rank keeping the documented dependency-kind order stable:
/// `normal` < `dev` < `build` < anything unknown.
pub(crate) const fn dependency_kind_rank(kind: &str) -> u8 {
    match kind.as_bytes() {
        b"normal" => 0,
        b"dev" => 1,
        b"build" => 2,
        _ => 3,
    }
}

/// Collects, attributes, scopes, and canonically orders the declaration rows
/// for `eg query manifest-deps`.
pub(crate) fn collect_manifest_deps_rows<'a>(
    records: &'a [GraphRecord],
    index: &'a query::RepositoryIndex,
    repo_scope: Option<&str>,
    deleted: &std::collections::BTreeSet<&str>,
) -> Vec<ManifestDepsDeclarationJson<'a>> {
    let mut rows: Vec<ManifestDepsDeclarationJson<'_>> = records
        .iter()
        .filter_map(|record| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::DependencyDeclaration,
                schema_version,
                repo_relative_path: Some(manifest_path),
                name: Some(name),
                dependency: Some(payload),
                ..
            } = record
            else {
                return None;
            };
            // Current-state query: a tombstoned declaration is not live
            // (PR #314 review), mirroring the other query paths.
            if deleted.contains(id.as_str()) {
                return None;
            }
            // Repository attribution via the CONTAINS topology (PR #314
            // review); scoping drops rows owned by other repositories.
            let owner = index.owner_of(id);
            if let Some(scope) = repo_scope
                && owner != Some(scope)
            {
                return None;
            }
            Some(ManifestDepsDeclarationJson {
                record_id: id,
                repository: owner.and_then(|repo_id| index.display_of(repo_id)),
                name,
                declared_as: payload.declared_as.as_deref(),
                dependency_kind: &payload.dependency_kind,
                declared_requirement: payload.declared_requirement.as_deref(),
                resolved_version: payload.resolved_version.as_deref(),
                resolution: &payload.resolution,
                declaring_package: &payload.declaring_package,
                manifest_path,
                schema_version: *schema_version,
            })
        })
        .collect();
    rows.sort_by(|left, right| {
        left.repository
            .unwrap_or("")
            .cmp(right.repository.unwrap_or(""))
            .then_with(|| left.manifest_path.cmp(right.manifest_path))
            .then_with(|| left.declaring_package.cmp(right.declaring_package))
            .then_with(|| {
                dependency_kind_rank(left.dependency_kind)
                    .cmp(&dependency_kind_rank(right.dependency_kind))
            })
            .then_with(|| left.name.cmp(right.name))
            .then_with(|| {
                left.declared_as
                    .unwrap_or("")
                    .cmp(right.declared_as.unwrap_or(""))
            })
            .then_with(|| left.record_id.cmp(right.record_id))
    });
    rows
}

/// Skipped-manifest honesty (PR #314 review): an unreadable/unparseable
/// manifest means dependency coverage has holes, so every answer — hit,
/// miss, and empty surface — is qualified with one `skipped_manifest`
/// diagnostic per skipped manifest (repo-relative handle + Diagnostic record
/// ID + owning repository label, deterministic order). Diagnostics are
/// attributed via the Repository —CONTAINS→ Diagnostic topology; under
/// `--repo`, holes owned by OTHER repositories are dropped, while
/// unattributable legacy diagnostics are always included (their absent
/// `repository` field is the marker) — hiding a possible coverage hole would
/// be worse than over-reporting one.
pub(crate) fn collect_skipped_manifest_diagnostics<'a>(
    records: &'a [GraphRecord],
    index: &'a query::RepositoryIndex,
    repo_scope: Option<&str>,
    deleted: &std::collections::BTreeSet<&str>,
) -> Vec<ManifestDepsDiagnosticJson<'a>> {
    let mut skipped: Vec<(&str, &str, Option<&str>)> = records
        .iter()
        .filter_map(|record| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Diagnostic,
                repo_relative_path: Some(path),
                symbol_kind: Some(symbol_kind),
                ..
            } = record
            else {
                return None;
            };
            // A tombstoned diagnostic no longer qualifies current-state
            // answers (PR #314 review).
            if deleted.contains(id.as_str()) {
                return None;
            }
            // Every skipped-manifest class qualifies answers: unparseable
            // manifests, parseable ones whose dependency tables carry no
            // usable [package].name, manifests whose `workspace = true`
            // entries have no resolvable workspace root, dependency
            // entries Cargo would reject — neither version string nor
            // table — and workspace roots whose member resolution Cargo
            // rejects outright (PR #314 review).
            if symbol_kind != crate::manifest_deps::SKIPPED_MANIFEST_DIAGNOSTIC_KIND
                && symbol_kind != crate::manifest_deps::UNATTRIBUTABLE_MANIFEST_DIAGNOSTIC_KIND
                && symbol_kind != crate::manifest_deps::UNINHERITABLE_MANIFEST_DIAGNOSTIC_KIND
                && symbol_kind != crate::manifest_deps::UNINTERPRETABLE_DEPENDENCY_DIAGNOSTIC_KIND
                && symbol_kind != crate::manifest_deps::UNLOADABLE_WORKSPACE_DIAGNOSTIC_KIND
            {
                return None;
            }
            let owner = index.owner_of(id);
            if let Some(scope) = repo_scope
                && owner.is_some_and(|owner| owner != scope)
            {
                return None;
            }
            Some((
                path.as_str(),
                id.as_str(),
                owner.and_then(|repo_id| index.display_of(repo_id)),
            ))
        })
        .collect();
    skipped.sort_unstable();
    skipped.dedup();
    skipped
        .into_iter()
        .map(|(path, record_id, repository)| ManifestDepsDiagnosticJson {
            code: "skipped_manifest",
            detail: Some(path),
            record_id: Some(record_id),
            repository,
        })
        .collect()
}

/// `eg query manifest-deps` (issue #180): list declared Cargo dependencies
/// with their lockfile resolution and repository attribution, or answer a
/// direct `--name` lookup. Deterministic, byte-identical output; an empty
/// surface or a miss is a machine-readable success, never an error.
pub(crate) fn query_manifest_deps_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    name_filter: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Current-state view: tombstoned records (rows and diagnostics alike)
    // are excluded, mirroring the other query paths (PR #314 review).
    let deleted = current_deleted_ids(records);
    let mut rows = collect_manifest_deps_rows(records, index, repo_scope, &deleted);
    let surface_is_empty = rows.is_empty();
    if let Some(filter) = name_filter {
        rows.retain(|row| row.name == filter);
    }

    let mut diagnostics = Vec::new();
    if surface_is_empty {
        diagnostics.push(ManifestDepsDiagnosticJson {
            code: "empty_dependency_surface",
            detail: None,
            record_id: None,
            repository: None,
        });
    } else if rows.is_empty() {
        diagnostics.push(ManifestDepsDiagnosticJson {
            code: "no_match_for_name",
            detail: name_filter,
            record_id: None,
            repository: None,
        });
    }
    diagnostics.extend(collect_skipped_manifest_diagnostics(
        records, index, repo_scope, &deleted,
    ));

    match format {
        OutputFormat::Json => {
            let response = ManifestDepsResponse {
                ok: true,
                query: "manifest-deps",
                name: name_filter,
                repo_scope: repo_scope.and_then(|repo_id| index.display_of(repo_id)),
                count: rows.len(),
                disclaimer: MANIFEST_DEPS_DISCLAIMER,
                declarations: rows,
                diagnostics,
            };
            let output = serde_json::to_string_pretty(&response)
                .context("failed to serialize dependency declarations")?;
            println!("{output}");
        }
        OutputFormat::Text => {
            println!("{} dependency declaration(s)", rows.len());
            for row in &rows {
                let requirement = row.declared_requirement.unwrap_or("(none)");
                let resolved = row.resolved_version.unwrap_or(row.resolution);
                let repository = row
                    .repository
                    .map(|label| format!(" repo={label}"))
                    .unwrap_or_default();
                let declared_as = row
                    .declared_as
                    .map(|key| format!(" declared-as={key}"))
                    .unwrap_or_default();
                println!(
                    "{package} {kind} {name}{declared_as} requirement={requirement} resolved={resolved} manifest={manifest}{repository} ({record_id})",
                    package = row.declaring_package,
                    kind = row.dependency_kind,
                    name = row.name,
                    manifest = row.manifest_path,
                    record_id = row.record_id,
                );
            }
            for diagnostic in &diagnostics {
                let record_id = diagnostic
                    .record_id
                    .map(|id| format!(" [{id}]"))
                    .unwrap_or_default();
                match diagnostic.detail {
                    Some(detail) => {
                        println!("diagnostic: {} ({detail}){record_id}", diagnostic.code);
                    }
                    None => println!("diagnostic: {}{record_id}", diagnostic.code),
                }
            }
        }
    }
    Ok(())
}
