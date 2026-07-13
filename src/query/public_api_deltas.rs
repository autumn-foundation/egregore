use std::collections::{BTreeMap, BTreeSet};

use super::{
    ChainReachability, PUBLIC_API_SYMBOL_KINDS, RangeDeltaClass, RangeDeltasError,
    chain_reachability, is_library_crate_path, range_delta_node_summary, resolve_commit_range,
    resolve_range_scope, temporal_snapshot_index,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// Always-present advisory label for [`public_api_deltas`] responses.
///
/// Rows are observed structural surface changes derived from recorded
/// visibility, signatures, and module containment; they never assert semver
/// breakage, downstream build failure, behavior change, or a required
/// version bump.
pub const PUBLIC_API_DELTAS_DISCLAIMER: &str = "Rows are observed structural changes to the \
     parse-derived public API surface between the resolved commits; they are not proof of \
     semver breakage, downstream build failure, or behavior change, and no version bump is \
     asserted. `potentially_breaking` marks a change class worth review, never a breakage \
     claim.";

/// Stable label attached to the opt-in internal group of a
/// [`public_api_deltas`] response: these rows are crate-internal deltas,
/// never public-API changes.
pub const PUBLIC_API_DELTAS_INTERNAL_LABEL: &str = "internal_not_public_surface";

/// Options for [`public_api_deltas`].
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct PublicApiDeltasOptions {
    /// Also list non-exported (crate-internal) symbol deltas in a separate
    /// clearly-labeled `internal` group. Internal deltas are always tallied
    /// in `counts.internal_changes` regardless of this flag.
    pub include_internal: bool,
    /// Attach base-endpoint internal caller leads (existing `CALLS` edges) to
    /// `removed` and `signature_changed` rows so the agent sees who relied on
    /// the changed item. Never required for the core classification.
    pub with_callers: bool,
}

/// One internal caller lead attached to a `removed` or `signature_changed`
/// row when [`PublicApiDeltasOptions::with_callers`] is set.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct PublicApiDeltaCaller<'a> {
    /// Stable record ID of the calling symbol.
    pub record_id: &'a str,
    /// Caller symbol name, when its base-endpoint snapshot resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    /// Repo-relative path of the caller, when its snapshot resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<&'a str>,
}

/// One classified public-API surface change between the two endpoints of a
/// commit range.
///
/// Serialization is bounded to identity fields plus the recorded declaration
/// surface (visibility class and normalized signature header, issue #124) —
/// never snapshot bodies, blob contents, or patch hunks.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct PublicApiDeltaRow<'a> {
    /// Stable record ID of the citation snapshot (`handle_side` says which
    /// endpoint it belongs to).
    pub record_id: &'a str,
    /// Schema version stamped on the backing record.
    pub schema_version: u32,
    /// Stable change-class label from the closed set documented in
    /// `docs/cli/public-api-deltas.md`: `added` / `removed` /
    /// `signature_changed` / `visibility_narrowed` / `visibility_widened`,
    /// or `internal_added` / `internal_removed` / `internal_modified` inside
    /// the internal group.
    pub change_class: &'static str,
    /// `true` for surface-contract-shrinking classes (`removed`,
    /// `signature_changed`, `visibility_narrowed`). An observed-surface
    /// review flag, never a semver or breakage claim (see the response
    /// disclaimer).
    pub potentially_breaking: bool,
    /// Crate-relative qualified symbol name.
    pub name: &'a str,
    /// Language-specific symbol category, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<&'a str>,
    /// Repository-relative path of the citation snapshot.
    pub repo_relative_path: &'a str,
    /// Source span of the citation snapshot, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented reason a row carries no span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<&'static str>,
    /// Which endpoint snapshot the citation handle points at: `head`, or
    /// `base_tombstone` for removals (the item no longer exists at head, so
    /// the base-side snapshot is the documented tombstone handle).
    pub handle_side: &'static str,
    /// The range commit that introduced the head-visible state of this
    /// change (last such commit in deterministic topological order).
    pub commit: &'a str,
    /// Valid time (committer date) of the introducing commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<&'a str>,
    /// Recorded visibility class at the base endpoint, when present there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_visibility: Option<&'a str>,
    /// Recorded visibility class at the head endpoint, when present there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_visibility: Option<&'a str>,
    /// Recorded signature header at the base endpoint, when present there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_signature: Option<&'a str>,
    /// Recorded signature header at the head endpoint, when present there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_signature: Option<&'a str>,
    /// `true` when the reachability change came from the containing module
    /// chain (the item's own `pub` did not change; a containing module's
    /// visibility did).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub via_module_chain: bool,
    /// Base-endpoint internal caller leads; present only on `removed` /
    /// `signature_changed` rows when the caller join was requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal_callers: Option<Vec<PublicApiDeltaCaller<'a>>>,
}

/// The opt-in internal group of a [`public_api_deltas`] response: deltas to
/// symbols that are not on the external surface at either endpoint.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicApiInternalSection<'a> {
    /// Always [`PUBLIC_API_DELTAS_INTERNAL_LABEL`].
    pub label: &'static str,
    /// Internal delta rows (`internal_added` / `internal_removed` /
    /// `internal_modified`), never potentially-breaking.
    pub rows: Vec<PublicApiDeltaRow<'a>>,
}

/// Deterministic tallies for a [`public_api_deltas`] response.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, Eq, PartialEq)]
pub struct PublicApiDeltaCounts {
    /// Exported items present at head but not present at all at base.
    pub added: usize,
    /// Exported items whose snapshot vanished entirely by head.
    pub removed: usize,
    /// Exported items whose recorded signature header changed.
    pub signature_changed: usize,
    /// Items that left the external surface but still exist at head.
    pub visibility_narrowed: usize,
    /// Items that joined the external surface from an existing declaration.
    pub visibility_widened: usize,
    /// Exported items whose body changed while the recorded surface
    /// (visibility and signature) stayed identical — not a surface change.
    pub exported_body_only_modified: usize,
    /// Non-exported symbol deltas (listed only in the opt-in internal group).
    pub internal_changes: usize,
}

/// A stable machine-readable condition attached to a [`public_api_deltas`]
/// response. These are markers, never partial or guessed rows.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct PublicApiDeltaDiagnostic {
    /// Stable diagnostic code (`symbol_visibility_missing`,
    /// `module_visibility_unknown`, `unresolved_introducing_commit`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Bounded human-readable detail (identity fields only, never payloads).
    pub detail: String,
}

/// Structured public-API surface changes between two commits, grouped by
/// stable change class. Returned by [`public_api_deltas`].
///
/// Every group is always present (empty vecs, never omitted) and canonically
/// ordered by `(repo_relative_path, name, record_id)` so repeated queries
/// are byte-equivalent after serialization. The `internal` group is present
/// only when requested.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PublicApiDeltas<'a> {
    /// Resolved full SHA of the base (older) endpoint.
    pub base: &'a str,
    /// Resolved full SHA of the head (newer) endpoint.
    pub head: &'a str,
    /// Number of commits in the range (reachable from head, not from base).
    pub range_commit_count: usize,
    /// Always-present advisory disclaimer
    /// ([`PUBLIC_API_DELTAS_DISCLAIMER`]).
    pub disclaimer: &'static str,
    /// Items exported at head that did not exist at base.
    pub added: Vec<PublicApiDeltaRow<'a>>,
    /// Items exported at base whose snapshot vanished entirely by head.
    pub removed: Vec<PublicApiDeltaRow<'a>>,
    /// Items exported at both endpoints whose signature header changed.
    pub signature_changed: Vec<PublicApiDeltaRow<'a>>,
    /// Items exported at base that still exist at head but left the surface.
    pub visibility_narrowed: Vec<PublicApiDeltaRow<'a>>,
    /// Items that existed at base off-surface and are exported at head.
    pub visibility_widened: Vec<PublicApiDeltaRow<'a>>,
    /// Opt-in internal group; `None` unless requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub internal: Option<PublicApiInternalSection<'a>>,
    /// Deterministic tallies (internal deltas are counted even when unlisted).
    pub counts: PublicApiDeltaCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<PublicApiDeltaDiagnostic>,
}

/// How one symbol snapshot relates to the external surface at one endpoint.
struct SurfaceSnapView<'a> {
    record: &'a GraphRecord,
    record_id: &'a str,
    schema_version: u32,
    name: &'a str,
    symbol_kind: Option<&'a str>,
    repo_relative_path: Option<&'a str>,
    span: Option<SourceSpan>,
    visibility: Option<&'a str>,
    signature: Option<&'a str>,
    /// `true` when the snapshot is on the external surface at this endpoint.
    exported: bool,
    /// `true` when the item's own recorded visibility is `public` (even if a
    /// non-`pub` containing module keeps it off the surface).
    own_public: bool,
    /// Surface-eligible (Rust, library-crate path, enumerable symbol kind)
    /// but carrying no recorded visibility (pre-#124 scan) — unclassifiable.
    missing_visibility: bool,
    /// First module-chain prefix with no recorded visibility, when the item
    /// is otherwise `pub` and surface-eligible.
    unknown_module: Option<String>,
}

/// The recorded signature header of a node record (empty when absent).
/// Used only for in-process introducing-commit comparison.
fn signature_surface_key(record: &GraphRecord) -> &str {
    match record {
        GraphRecord::Node { signature, .. } => signature.as_deref().unwrap_or(""),
        _ => "",
    }
}

/// The recorded visibility class of a node record (empty when absent).
/// Used only for in-process introducing-commit comparison.
fn visibility_surface_key(record: &GraphRecord) -> &str {
    match record {
        GraphRecord::Node { visibility, .. } => visibility.as_deref().unwrap_or(""),
        _ => "",
    }
}

/// Builds the endpoint surface view of one symbol snapshot against that
/// endpoint's module-visibility map. Returns `None` for non-node records and
/// records without a name (defensive; symbol snapshots always carry one).
fn surface_snapshot_view<'a>(
    record: &'a GraphRecord,
    module_visibility: &BTreeMap<String, &'a str>,
) -> Option<SurfaceSnapView<'a>> {
    let GraphRecord::Node {
        id,
        schema_version,
        name: Some(name),
        language,
        symbol_kind,
        repo_relative_path,
        span,
        visibility,
        signature,
        ..
    } = record
    else {
        return None;
    };
    let eligible = language.as_deref() == Some("rust")
        && symbol_kind
            .as_deref()
            .is_some_and(|k| PUBLIC_API_SYMBOL_KINDS.contains(&k))
        && repo_relative_path
            .as_deref()
            .is_some_and(is_library_crate_path);
    let missing_visibility = eligible && visibility.is_none();
    let own_public = visibility.as_deref() == Some("public");
    let mut unknown_module = None;
    let exported = eligible && own_public && {
        let segments: Vec<&str> = name.split("::").collect();
        let chain = &segments[..segments.len().saturating_sub(1)];
        match chain_reachability(chain, module_visibility) {
            ChainReachability::Public => true,
            ChainReachability::NotPublic => false,
            ChainReachability::Unknown(prefix) => {
                unknown_module = Some(prefix);
                false
            }
        }
    };
    Some(SurfaceSnapView {
        record,
        record_id: id.as_str(),
        schema_version: *schema_version,
        name: name.as_str(),
        symbol_kind: symbol_kind.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        visibility: visibility.as_deref(),
        signature: signature.as_deref(),
        exported,
        own_public,
        missing_visibility,
        unknown_module,
    })
}

/// Classifies changes to the Rust library crate's externally-reachable
/// public API surface between two commit handles (issue #157).
///
/// The slice composes the range-delta mechanics of issue #118 (endpoint
/// resolution, introducing commits, error taxonomy) with the issue #124
/// declaration surface (per-symbol `visibility`/`signature`) and the issue
/// #213 reachability rule (an item is exported when its own visibility is
/// `public` and every containing module is recorded `public` at that
/// endpoint). Renames surface as a `removed` + `added` pair because symbol
/// identity is path- and name-based. Non-exported symbol deltas never enter
/// the public-surface groups; they are tallied, and listed in a separate
/// `internal` group only when requested. `pub use` re-export sites are not
/// classified (see `docs/cli/public-api-deltas.md`).
///
/// Purely read-time: reads only the provided records, never Git state or
/// the working tree. Output is deterministic and byte-equivalent across
/// repeated runs on an unchanged store.
///
/// # Errors
///
/// Returns a [`RangeDeltasError`] when the history is empty, a commit handle
/// is missing or ambiguous, the endpoints are identical, the range is
/// reversed, or no ancestor path connects the endpoints — the same taxonomy
/// as [`range_deltas`].
#[allow(clippy::too_many_lines, clippy::missing_panics_doc)]
pub fn public_api_deltas<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
    options: PublicApiDeltasOptions,
) -> Result<PublicApiDeltas<'a>, RangeDeltasError> {
    let scope = resolve_range_scope(records, base_prefix, head_prefix, repo_scope)?;
    let in_scope = |id: &str| scope.in_scope(id);

    let range = resolve_commit_range(records, base_prefix, head_prefix, &in_scope)?;
    let base_sha = range.base_sha;
    let head_sha = range.head_sha;

    let symbol_snaps = temporal_snapshot_index(records, NodeKind::Symbol, &in_scope);
    let module_snaps = temporal_snapshot_index(records, NodeKind::Module, &in_scope);

    // Per-endpoint module-visibility maps. Colliding declarations (e.g.
    // cfg-gated) resolve deterministically: `public` wins, as in
    // `public_api_surface`.
    let module_visibility_at = |sha: &str| -> BTreeMap<String, &'a str> {
        let mut map: BTreeMap<String, &str> = BTreeMap::new();
        for per_commit in module_snaps.values() {
            let Some(GraphRecord::Node {
                name: Some(name),
                language,
                visibility,
                ..
            }) = per_commit.get(sha).copied()
            else {
                continue;
            };
            if language.as_deref() != Some("rust") {
                continue;
            }
            let vis = visibility.as_deref().unwrap_or("unknown");
            let entry = map.entry(name.clone()).or_insert(vis);
            if vis == "public" {
                *entry = vis;
            }
        }
        map
    };
    let module_vis_base = module_visibility_at(base_sha);
    let module_vis_head = module_visibility_at(head_sha);

    // Base-endpoint caller leads, keyed by callee record ID (opt-in join).
    let mut callers_by_target: BTreeMap<&str, BTreeMap<&str, PublicApiDeltaCaller<'a>>> =
        BTreeMap::new();
    if options.with_callers {
        for r in records {
            let GraphRecord::Edge {
                label: EdgeLabel::Calls,
                source,
                target,
                temporal: Some(t),
                ..
            } = r
            else {
                continue;
            };
            if t.git_commit != base_sha || !in_scope(source.as_str()) {
                continue;
            }
            let (name, repo_relative_path) = symbol_snaps
                .get(source.as_str())
                .and_then(|per_commit| per_commit.get(base_sha))
                .map_or((None, None), |caller| {
                    if let GraphRecord::Node {
                        name,
                        repo_relative_path,
                        ..
                    } = caller
                    {
                        (name.as_deref(), repo_relative_path.as_deref())
                    } else {
                        (None, None)
                    }
                });
            callers_by_target
                .entry(target.as_str())
                .or_default()
                .insert(
                    source.as_str(),
                    PublicApiDeltaCaller {
                        record_id: source.as_str(),
                        name,
                        repo_relative_path,
                    },
                );
        }
    }

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut signature_changed = Vec::new();
    let mut visibility_narrowed = Vec::new();
    let mut visibility_widened = Vec::new();
    let mut internal_rows = Vec::new();
    let mut counts = PublicApiDeltaCounts::default();
    let mut diagnostics: Vec<PublicApiDeltaDiagnostic> = Vec::new();
    let mut unknown_modules: BTreeSet<String> = BTreeSet::new();

    for per_commit in symbol_snaps.values() {
        let base_view = per_commit
            .get(base_sha)
            .and_then(|r| surface_snapshot_view(r, &module_vis_base));
        let head_view = per_commit
            .get(head_sha)
            .and_then(|r| surface_snapshot_view(r, &module_vis_head));
        let Some(any_view) = head_view.as_ref().or(base_view.as_ref()) else {
            // Present at neither endpoint: strictly-inside-the-range churn.
            continue;
        };

        // A surface-eligible snapshot with no recorded visibility cannot be
        // classified on either side of the boundary — reported, never guessed.
        if base_view.as_ref().is_some_and(|v| v.missing_visibility)
            || head_view.as_ref().is_some_and(|v| v.missing_visibility)
        {
            diagnostics.push(PublicApiDeltaDiagnostic {
                code: "symbol_visibility_missing",
                record_id: Some(any_view.record_id.to_owned()),
                detail: format!(
                    "symbol `{}` carries no recorded visibility (pre-#124 scan?); \
                     excluded from classification, re-scan to include it",
                    any_view.name
                ),
            });
            continue;
        }
        for view in base_view.iter().chain(head_view.iter()) {
            if let Some(prefix) = &view.unknown_module {
                unknown_modules.insert(prefix.clone());
            }
        }

        let exported_base = base_view.as_ref().is_some_and(|v| v.exported);
        let exported_head = head_view.as_ref().is_some_and(|v| v.exported);

        let mut make_row = |handle: &SurfaceSnapView<'a>,
                            handle_side: &'static str,
                            change_class: &'static str,
                            potentially_breaking: bool,
                            introducing_class: RangeDeltaClass,
                            modified_key: &dyn Fn(&GraphRecord) -> &str,
                            via_module_chain: bool,
                            attach_callers: bool|
         -> Option<PublicApiDeltaRow<'a>> {
            let repo_relative_path = handle.repo_relative_path?;
            let commit = range
                .introducing(per_commit, introducing_class, modified_key)
                .unwrap_or_else(|| {
                    diagnostics.push(PublicApiDeltaDiagnostic {
                        code: "unresolved_introducing_commit",
                        record_id: Some(handle.record_id.to_owned()),
                        detail: format!(
                            "{change_class} change confirmed between endpoints but no range \
                             commit shows the transition; falling back to the head commit"
                        ),
                    });
                    head_sha
                });
            let internal_callers = (attach_callers && options.with_callers).then(|| {
                callers_by_target
                    .get(handle.record_id)
                    .map(|callers| callers.values().cloned().collect())
                    .unwrap_or_default()
            });
            Some(PublicApiDeltaRow {
                record_id: handle.record_id,
                schema_version: handle.schema_version,
                change_class,
                potentially_breaking,
                name: handle.name,
                symbol_kind: handle.symbol_kind,
                repo_relative_path,
                span: handle.span,
                absent_span_reason: if handle.span.is_none() {
                    Some("no_span_module_level")
                } else {
                    None
                },
                handle_side,
                commit,
                valid_time: range.commit_valid_time.get(commit).copied(),
                before_visibility: base_view.as_ref().and_then(|v| v.visibility),
                after_visibility: head_view.as_ref().and_then(|v| v.visibility),
                before_signature: base_view.as_ref().and_then(|v| v.signature),
                after_signature: head_view.as_ref().and_then(|v| v.signature),
                via_module_chain,
                internal_callers,
            })
        };

        match (exported_base, exported_head) {
            (false, false) => {
                // Crate-internal lane: private / restricted / trapped items,
                // non-surface kinds, and non-library paths. Never a
                // public-API change.
                let class = match (base_view.as_ref(), head_view.as_ref()) {
                    (None, Some(_)) => Some(("internal_added", RangeDeltaClass::Added)),
                    (Some(_), None) => Some(("internal_removed", RangeDeltaClass::Removed)),
                    (Some(b), Some(h))
                        if range_delta_node_summary(b.record)
                            != range_delta_node_summary(h.record) =>
                    {
                        Some(("internal_modified", RangeDeltaClass::Modified))
                    }
                    _ => None,
                };
                if let Some((class_label, introducing_class)) = class {
                    counts.internal_changes += 1;
                    if options.include_internal {
                        let handle_side = if head_view.is_some() {
                            "head"
                        } else {
                            "base_tombstone"
                        };
                        if let Some(handle) = head_view.as_ref().or(base_view.as_ref()) {
                            let row = make_row(
                                handle,
                                handle_side,
                                class_label,
                                false,
                                introducing_class,
                                &range_delta_node_summary,
                                false,
                                false,
                            );
                            internal_rows.extend(row);
                        }
                    }
                }
            }
            (false, true) => {
                let head = head_view
                    .as_ref()
                    .expect("exported head endpoint has a snapshot view");
                if let Some(base) = base_view.as_ref() {
                    let via_chain = base.own_public;
                    let row = make_row(
                        head,
                        "head",
                        "visibility_widened",
                        false,
                        RangeDeltaClass::Modified,
                        &visibility_surface_key,
                        via_chain,
                        false,
                    );
                    visibility_widened.extend(row);
                } else {
                    let row = make_row(
                        head,
                        "head",
                        "added",
                        false,
                        RangeDeltaClass::Added,
                        &range_delta_node_summary,
                        false,
                        false,
                    );
                    added.extend(row);
                }
            }
            (true, false) => {
                let base = base_view
                    .as_ref()
                    .expect("exported base endpoint has a snapshot view");
                if let Some(head) = head_view.as_ref() {
                    let via_chain = head.own_public;
                    let row = make_row(
                        head,
                        "head",
                        "visibility_narrowed",
                        true,
                        RangeDeltaClass::Modified,
                        &visibility_surface_key,
                        via_chain,
                        false,
                    );
                    visibility_narrowed.extend(row);
                } else {
                    let row = make_row(
                        base,
                        "base_tombstone",
                        "removed",
                        true,
                        RangeDeltaClass::Removed,
                        &range_delta_node_summary,
                        false,
                        true,
                    );
                    removed.extend(row);
                }
            }
            (true, true) => {
                let base = base_view
                    .as_ref()
                    .expect("exported base endpoint has a snapshot view");
                let head = head_view
                    .as_ref()
                    .expect("exported head endpoint has a snapshot view");
                if base.signature != head.signature {
                    let row = make_row(
                        head,
                        "head",
                        "signature_changed",
                        true,
                        RangeDeltaClass::Modified,
                        &signature_surface_key,
                        false,
                        true,
                    );
                    signature_changed.extend(row);
                } else if range_delta_node_summary(base.record)
                    != range_delta_node_summary(head.record)
                {
                    // Body-only change: the recorded surface is identical, so
                    // this is not a surface change. Tallied for honesty.
                    counts.exported_body_only_modified += 1;
                }
            }
        }
    }

    for module in unknown_modules {
        diagnostics.push(PublicApiDeltaDiagnostic {
            code: "module_visibility_unknown",
            record_id: None,
            detail: format!(
                "module `{module}` has no recorded visibility at an endpoint; items \
                 beneath it are treated as off-surface, not guessed"
            ),
        });
    }

    let sort_rows = |rows: &mut Vec<PublicApiDeltaRow<'a>>| {
        rows.sort_by(|a, b| {
            a.repo_relative_path
                .cmp(b.repo_relative_path)
                .then_with(|| a.name.cmp(b.name))
                .then_with(|| a.record_id.cmp(b.record_id))
        });
    };
    sort_rows(&mut added);
    sort_rows(&mut removed);
    sort_rows(&mut signature_changed);
    sort_rows(&mut visibility_narrowed);
    sort_rows(&mut visibility_widened);
    sort_rows(&mut internal_rows);
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    diagnostics.dedup();

    counts.added = added.len();
    counts.removed = removed.len();
    counts.signature_changed = signature_changed.len();
    counts.visibility_narrowed = visibility_narrowed.len();
    counts.visibility_widened = visibility_widened.len();

    Ok(PublicApiDeltas {
        base: base_sha,
        head: head_sha,
        range_commit_count: range.range_commit_shas.len(),
        disclaimer: PUBLIC_API_DELTAS_DISCLAIMER,
        added,
        removed,
        signature_changed,
        visibility_narrowed,
        visibility_widened,
        internal: options
            .include_internal
            .then_some(PublicApiInternalSection {
                label: PUBLIC_API_DELTAS_INTERNAL_LABEL,
                rows: internal_rows,
            }),
        counts,
        diagnostics,
    })
}
