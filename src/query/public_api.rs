use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// Symbol kinds that can appear on the externally-reachable public API
/// surface. Methods, tests, and `impl` blocks are declaration details of
/// their owning items and are never enumerated as surface items.
pub(super) const PUBLIC_API_SYMBOL_KINDS: &[&str] = &[
    "function",
    "struct",
    "enum",
    "trait",
    "type_alias",
    "const",
    "static",
];

/// One externally-reachable public API item.
///
/// For declared items (`kind` = symbol kind or `module`) the citation fields
/// point at the declaration. For re-exports (`via_reexport` = `true`) they
/// point at the **re-export site** (the `pub use` line), per issue #213 AC3.
#[derive(Debug, Clone)]
pub struct PublicApiItem<'a> {
    /// Stable record ID of the declaring `Symbol`/`Module` node, or of the
    /// `Import` node at the re-export site.
    pub record_id: &'a str,
    /// Item kind: a symbol kind from [`PUBLIC_API_SYMBOL_KINDS`], `module`,
    /// or `reexport` when a `pub use` target does not resolve in-graph.
    pub kind: String,
    /// Externally visible crate-relative fully-qualified path.
    pub path: String,
    /// Repo-relative file of the declaration or re-export site.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration or re-export site.
    pub span: Option<SourceSpan>,
    /// Persisted declaration signature (issue #124), joined when present.
    pub signature: Option<&'a str>,
    /// `true` when the item reaches the surface through a `pub use`.
    pub via_reexport: bool,
    /// Crate-relative use-path the re-export points at (re-exports only).
    pub target: Option<String>,
    /// Record ID of the resolved in-graph re-export target, when the target
    /// path names a symbol or module in this graph.
    pub target_record_id: Option<&'a str>,
}

/// Deterministic tier tallies for items that were considered but excluded
/// from the externally-reachable set.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct PublicApiCounts {
    /// Items on the surface (including re-export rows).
    pub externally_reachable: usize,
    /// Surface rows contributed by `pub use` re-exports.
    pub reexports: usize,
    /// `pub(crate)` / `pub(super)` / `pub(in path)` items: crate-internal.
    pub crate_internal: usize,
    /// Items with no visibility modifier (or `pub(self)`).
    pub private: usize,
    /// `pub` items whose module chain is not provably all-`pub` — declared
    /// public but **not** externally reachable.
    pub trapped_public: usize,
}

/// A stable machine-readable condition attached to the surface result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PublicApiDiagnostic {
    /// Stable diagnostic code (`empty_surface`, `glob_reexport_unresolved`,
    /// `module_visibility_unknown`, `symbol_visibility_missing`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    pub record_id: Option<String>,
    /// Bounded human-readable detail (paths and counts only — never payload).
    pub detail: String,
}

/// The enumerated public API surface plus exclusion tallies and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct PublicApiSurface<'a> {
    /// Externally-reachable items, sorted by (path, kind, record ID).
    pub items: Vec<PublicApiItem<'a>>,
    /// Exclusion-tier tallies.
    pub counts: PublicApiCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<PublicApiDiagnostic>,
}

/// Returns `true` when `path` belongs to the library crate rooted at `src/`.
///
/// `src/bin/**` holds separate binary crates and non-`src/` paths (tests,
/// examples, benches) are separate test/target crates; `pub` items there are
/// never part of the library's external contract.
pub(super) fn is_library_crate_path(path: &str) -> bool {
    let mut segments = path.split(['/', '\\']).filter(|s| !s.is_empty());
    segments.next() == Some("src") && {
        let second = segments.next();
        second.is_some() && second != Some("bin")
    }
}

/// How a module chain resolved during reachability checking.
pub(super) enum ChainReachability {
    /// Every ancestor module is recorded `public`.
    Public,
    /// Some ancestor module is recorded with a non-`public` visibility.
    NotPublic,
    /// Some ancestor module has no visibility record in the graph (dead file,
    /// pre-#213 scan, or unscanned crate root). Reported, never guessed.
    Unknown(String),
}

/// Checks that every prefix of `chain` names a module recorded `public`.
pub(super) fn chain_reachability(
    chain: &[&str],
    module_visibility: &BTreeMap<String, &str>,
) -> ChainReachability {
    for depth in 1..=chain.len() {
        let prefix = chain[..depth].join("::");
        match module_visibility.get(&prefix).copied() {
            Some("public") => {}
            Some(_) => return ChainReachability::NotPublic,
            None => return ChainReachability::Unknown(prefix),
        }
    }
    ChainReachability::Public
}

/// One leaf of a parsed `pub use` tree.
struct UseLeaf {
    /// Use-path as written (before `crate::`/`self::`/`super::` resolution).
    target: String,
    /// Name the leaf is visible under at the re-export site.
    visible: String,
}

/// A parsed `pub use` declaration: named leaves plus unresolvable glob stems.
struct ParsedPubUse {
    leaves: Vec<UseLeaf>,
    globs: Vec<String>,
}

/// Parses an import record's stored text (e.g. `pub use a::{B as C, d}`)
/// into re-export leaves. Returns `None` for plain `use` and for
/// `pub(crate)`/`pub(super)`/`pub(in path)` restricted re-exports, which do
/// not widen visibility to the outside world.
fn parse_pub_use(text: &str) -> Option<ParsedPubUse> {
    let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let rest = normalized.strip_prefix("pub ")?;
    if rest.starts_with('(') {
        return None;
    }
    let tree = rest.strip_prefix("use ")?.trim();
    let mut parsed = ParsedPubUse {
        leaves: Vec::new(),
        globs: Vec::new(),
    };
    parse_use_tree("", tree, &mut parsed);
    Some(parsed)
}

/// Joins two `::`-separated path fragments, tolerating empty sides.
fn join_use_path(prefix: &str, rest: &str) -> String {
    match (prefix.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_owned(),
        (_, true) => prefix.to_owned(),
        (false, false) => format!("{prefix}::{rest}"),
    }
}

/// Splits a `{...}` group body on top-level commas.
fn split_top_level_commas(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&text[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
}

/// Recursively flattens a use tree (`a::{b, c as d, e::*}`) into leaves.
fn parse_use_tree(prefix: &str, tree: &str, out: &mut ParsedPubUse) {
    let tree = tree.trim().trim_end_matches(';').trim();
    if tree.is_empty() {
        return;
    }
    if let Some(brace_start) = tree.find('{') {
        let head = tree[..brace_start].trim().trim_end_matches("::").trim();
        let new_prefix = join_use_path(prefix, head);
        let inner_end = tree.rfind('}').unwrap_or(tree.len());
        for part in split_top_level_commas(&tree[brace_start + 1..inner_end]) {
            parse_use_tree(&new_prefix, part, out);
        }
        return;
    }
    if let Some((path, alias)) = tree.split_once(" as ") {
        let target = join_use_path(prefix, path.trim());
        out.leaves.push(UseLeaf {
            target,
            visible: alias.trim().to_owned(),
        });
        return;
    }
    if tree == "*" || tree.ends_with("::*") {
        let stem = tree.trim_end_matches('*').trim_end_matches("::");
        out.globs.push(join_use_path(prefix, stem));
        return;
    }
    if tree == "self" {
        if let Some(visible) = prefix.rsplit("::").next().filter(|s| !s.is_empty()) {
            out.leaves.push(UseLeaf {
                target: prefix.to_owned(),
                visible: visible.to_owned(),
            });
        }
        return;
    }
    let target = join_use_path(prefix, tree);
    let visible = target
        .rsplit("::")
        .next()
        .unwrap_or(target.as_str())
        .to_owned();
    out.leaves.push(UseLeaf { target, visible });
}

/// Resolves a use-path against the module chain of the re-export site:
/// `crate::` anchors at the crate root, `self::` at the owning module, and
/// each leading `super::` pops one module. Bare paths stay as written (a
/// crate-root module or an external crate — resolved only if in-graph).
fn resolve_use_target(target: &str, owner_chain: &[String]) -> String {
    if let Some(rest) = target.strip_prefix("crate::") {
        return rest.to_owned();
    }
    if target == "crate" {
        return String::new();
    }
    if let Some(rest) = target.strip_prefix("self::") {
        return join_use_path(&owner_chain.join("::"), rest);
    }
    let mut chain: &[String] = owner_chain;
    let mut rest = target;
    while let Some(popped) = rest.strip_prefix("super::") {
        chain = chain.split_last().map_or(&[], |(_, head)| head);
        rest = popped;
    }
    if rest == target {
        // No `crate`/`self`/`super` anchor: crate-root-relative (2018 edition).
        return target.to_owned();
    }
    join_use_path(&chain.join("::"), rest)
}

/// Enumerates the crate's externally-reachable public API surface from the
/// recorded code graph (issue #213).
///
/// An item is externally reachable when its own recorded visibility is
/// `public` **and** every module on its containment chain is recorded
/// `public`; a `pub use` re-export at a reachable site adds its leaves,
/// attributed to the re-export line. `pub` items trapped inside non-`pub`
/// modules are excluded and tallied as `trapped_public`; `pub(crate)` /
/// `pub(super)` / `pub(in path)` items are tallied as `crate_internal`.
///
/// Scope: the Rust library crate rooted at `src/` (excluding `src/bin/**`,
/// tests, examples, and benches) in the current graph state — tombstoned
/// records are excluded. History replay re-emits the full graph at every
/// commit with `temporal` provenance and mints no tombstone for a
/// between-commit deletion, so the surface is anchored to each repository's
/// stamped HEAD (`source_snapshot`, issue #82): a temporal record is part of
/// the current state only when its commit is its repository's HEAD — the same
/// rule `resolve_head_symbols` uses — so a symbol deleted at HEAD never
/// resurfaces (issue #428). Snapshot-less stores (plain current-tree `scan`)
/// keep the conservative fallback: nodes resolve by keep-last dedupe by stable
/// ID. Deterministic: output ordering depends only on record content, never on
/// map iteration or wall-clock time. Purely parse-derived — never a
/// build-verified or semver claim.
#[must_use]
pub fn public_api_surface<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> PublicApiSurface<'a> {
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // Default current-state view: the set of record IDs NOT current at their
    // repository's stamped HEAD (`source_snapshot`, issue #82/#428), computed by
    // the shared head-anchor gate (`non_head_current_record_ids`) rather than an
    // inlined `repo_heads` / `owned_record_is_current` copy. History replay
    // re-emits the full graph at every commit with `temporal` provenance and no
    // tombstone for a between-commit deletion, so a symbol deleted at HEAD would
    // otherwise resurface. The drop-set groups by stable ID (an ID with any
    // HEAD-current version is retained whole) and the recency keep-last below
    // selects its HEAD version. Snapshot-less stores (plain current-tree `scan`)
    // yield an empty drop-set: nodes resolve by keep-last dedupe by stable ID.
    let non_head_current = super::non_head_current_record_ids(records, index);

    // Current-state view: keep-last dedupe by stable ID so history graphs
    // resolve to their newest (HEAD) version deterministically.
    let mut nodes: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    let mut saw_rust_code = false;
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            language,
            repo_relative_path,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(kind, NodeKind::Module | NodeKind::Symbol | NodeKind::Import) {
            continue;
        }
        if language.as_deref() != Some("rust") {
            continue;
        }
        if tombstoned.contains(id.as_str())
            || !is_owned(id)
            || non_head_current.contains(id.as_str())
        {
            continue;
        }
        saw_rust_code = true;
        if !repo_relative_path
            .as_deref()
            .is_some_and(is_library_crate_path)
        {
            continue;
        }
        // Keep-last dedupe by recency so a retained ID's HEAD (newest-valid-time)
        // version wins — the shared drop-set keeps every version of a
        // HEAD-current ID, and this selects the same version the removed
        // per-record HEAD gate did.
        let replace = nodes.get(id.as_str()).is_none_or(|existing| {
            super::file_at_point::version_recency_key(record)
                >= super::file_at_point::version_recency_key(existing)
        });
        if replace {
            nodes.insert(id.as_str(), record);
        }
    }

    let mut surface = PublicApiSurface::default();

    // Module visibility map. Colliding declarations (e.g. cfg-gated) resolve
    // deterministically: `public` wins over any other recorded class.
    let mut module_visibility: BTreeMap<String, &str> = BTreeMap::new();
    for record in nodes.values() {
        if let GraphRecord::Node {
            kind: NodeKind::Module,
            name: Some(name),
            visibility,
            ..
        } = record
        {
            let vis = visibility.as_deref().unwrap_or("unknown");
            let entry = module_visibility.entry(name.clone()).or_insert(vis);
            if vis == "public" {
                *entry = vis;
            }
        }
    }

    // In-graph name map for re-export target resolution: qualified name →
    // (record ID, kind), first record ID winning deterministically.
    let mut names: BTreeMap<&str, (&'a str, String)> = BTreeMap::new();
    for (id, record) in &nodes {
        let GraphRecord::Node {
            kind,
            name: Some(name),
            symbol_kind,
            ..
        } = record
        else {
            continue;
        };
        let item_kind = match kind {
            NodeKind::Module => "module".to_owned(),
            NodeKind::Symbol => match symbol_kind.as_deref() {
                Some(k) if PUBLIC_API_SYMBOL_KINDS.contains(&k) => k.to_owned(),
                _ => continue,
            },
            _ => continue,
        };
        names.entry(name.as_str()).or_insert((id, item_kind));
    }

    // Imports edge map: import record ID → owning module/file record ID.
    let mut import_owner: BTreeMap<&str, &str> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Edge {
            id,
            label: EdgeLabel::Imports,
            source,
            target,
            ..
        } = record
        {
            if !tombstoned.contains(id.as_str()) {
                import_owner.insert(target.as_str(), source.as_str());
            }
        }
    }

    let mut unknown_modules: BTreeSet<String> = BTreeSet::new();
    let mut missing_visibility = 0usize;

    // Declared items: symbols of the enumerated kinds, plus modules.
    for record in nodes.values() {
        let GraphRecord::Node {
            id,
            kind,
            name: Some(name),
            repo_relative_path,
            span,
            symbol_kind,
            visibility,
            signature,
            ..
        } = record
        else {
            continue;
        };
        let item_kind = match kind {
            NodeKind::Module => "module".to_owned(),
            NodeKind::Symbol => match symbol_kind.as_deref() {
                Some(k) if PUBLIC_API_SYMBOL_KINDS.contains(&k) => k.to_owned(),
                _ => continue,
            },
            _ => continue,
        };
        let Some(vis) = visibility.as_deref() else {
            missing_visibility += 1;
            continue;
        };
        match vis {
            "public" => {}
            "crate" | "restricted" => {
                surface.counts.crate_internal += 1;
                continue;
            }
            _ => {
                surface.counts.private += 1;
                continue;
            }
        }
        let segments: Vec<&str> = name.split("::").collect();
        let chain = &segments[..segments.len().saturating_sub(1)];
        match chain_reachability(chain, &module_visibility) {
            ChainReachability::Public => {}
            ChainReachability::NotPublic => {
                surface.counts.trapped_public += 1;
                continue;
            }
            ChainReachability::Unknown(prefix) => {
                surface.counts.trapped_public += 1;
                unknown_modules.insert(prefix);
                continue;
            }
        }
        surface.items.push(PublicApiItem {
            record_id: id,
            kind: item_kind,
            path: name.clone(),
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            signature: signature.as_deref(),
            via_reexport: false,
            target: None,
            target_record_id: None,
        });
    }

    // Re-exports: `pub use` import records at externally reachable sites.
    for record in nodes.values() {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Import,
            name: Some(name),
            repo_relative_path,
            span,
            ..
        } = record
        else {
            continue;
        };
        let Some(parsed) = parse_pub_use(name) else {
            continue;
        };
        // Module chain of the re-export site: the owning inline module when
        // one is recorded, else the file's crate-relative module path.
        let owner_chain: Vec<String> = import_owner
            .get(id.as_str())
            .and_then(|owner_id| nodes.get(owner_id))
            .and_then(|owner| {
                if let GraphRecord::Node {
                    kind: NodeKind::Module,
                    name: Some(module_name),
                    ..
                } = owner
                {
                    Some(module_name.split("::").map(str::to_owned).collect())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                repo_relative_path
                    .as_deref()
                    .map(crate::languages::rust::file_module_path)
                    .unwrap_or_default()
            });
        let chain_refs: Vec<&str> = owner_chain.iter().map(String::as_str).collect();
        let site_reachable = match chain_reachability(&chain_refs, &module_visibility) {
            ChainReachability::Public => true,
            ChainReachability::NotPublic => false,
            ChainReachability::Unknown(prefix) => {
                unknown_modules.insert(prefix);
                false
            }
        };
        if !site_reachable {
            surface.counts.trapped_public += parsed.leaves.len();
            continue;
        }
        for glob in &parsed.globs {
            let stem = resolve_use_target(glob, &owner_chain);
            surface.diagnostics.push(PublicApiDiagnostic {
                code: "glob_reexport_unresolved",
                record_id: Some((*id).clone()),
                detail: format!(
                    "pub use {stem}::* cannot be enumerated without name resolution; \
                     inspect the target module directly"
                ),
            });
        }
        for leaf in &parsed.leaves {
            let target = resolve_use_target(&leaf.target, &owner_chain);
            let resolved = names.get(target.as_str());
            surface.items.push(PublicApiItem {
                record_id: id,
                kind: resolved.map_or_else(|| "reexport".to_owned(), |(_, k)| k.clone()),
                path: join_use_path(&owner_chain.join("::"), &leaf.visible),
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                signature: None,
                via_reexport: true,
                target: Some(target),
                target_record_id: resolved.map(|(target_id, _)| *target_id),
            });
        }
    }

    surface.counts.externally_reachable = surface.items.len();
    surface.counts.reexports = surface.items.iter().filter(|i| i.via_reexport).count();

    for module in unknown_modules {
        surface.diagnostics.push(PublicApiDiagnostic {
            code: "module_visibility_unknown",
            record_id: None,
            detail: format!(
                "module `{module}` has no recorded visibility; items beneath it \
                 are excluded, not guessed"
            ),
        });
    }
    if missing_visibility > 0 {
        surface.diagnostics.push(PublicApiDiagnostic {
            code: "symbol_visibility_missing",
            record_id: None,
            detail: format!(
                "{missing_visibility} symbol record(s) carry no visibility field \
                 (pre-#124 scan?); re-scan to include them"
            ),
        });
    }
    if surface.items.is_empty() {
        surface.diagnostics.push(PublicApiDiagnostic {
            code: "empty_surface",
            record_id: None,
            detail: if saw_rust_code {
                "no externally-reachable public items found".to_owned()
            } else {
                "graph contains no Rust code-graph records".to_owned()
            },
        });
    }

    surface.items.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    surface.diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    surface.diagnostics.dedup();
    surface
}

// ---------------------------------------------------------------------------
// public-API surface deltas across a commit range (issue #157)
// ---------------------------------------------------------------------------
