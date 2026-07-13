use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::RepositoryIndex;
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, TemporalMetadata};

// ---------------------------------------------------------------------------
// Trait implementors query (issue #133)
// ---------------------------------------------------------------------------

/// Completeness marker attached to every implementors answer.
///
/// `IMPLEMENTS` edges exist for traits whose definition is resolvable anywhere
/// in the scanned repository — same-file (issues #133/#343) and, as of issue
/// #344, cross-file out-of-line impls (`impl crate::T for Foo` in a separate
/// `mod m;` file). Impls of external/std (cross-crate) traits are still not
/// edge-backed. Absence of a row is therefore never proof that no
/// implementation exists.
pub const IMPLEMENTORS_COMPLETENESS: &str = "local_traits_only";

/// Human-readable expansion of [`IMPLEMENTORS_COMPLETENESS`], carried on the
/// zero-implementors signal so an empty answer is never mistaken for an
/// authoritative one.
pub const IMPLEMENTORS_COMPLETENESS_NOTE: &str = "implementors are recorded only for traits defined in the scanned \
     repository (same-file or cross-file out-of-line impls); impls of \
     external/std traits are not edge-backed in this graph, so absence of a \
     row is not proof that no implementation exists";

/// One implementor row: the `impl` symbol reached through an inbound
/// `IMPLEMENTS` edge on the resolved trait, plus the implementing type
/// resolved from the impl.
pub struct ImplementorLead<'a> {
    /// The resolved trait Symbol record this row belongs to.
    pub trait_record: &'a GraphRecord,
    /// The connecting `IMPLEMENTS` edge record.
    pub edge: &'a GraphRecord,
    /// The `impl` Symbol record (edge source).
    pub impl_record: &'a GraphRecord,
    /// Qualified name of the implementing type, resolved against the graph
    /// when possible, otherwise parsed from the impl display name.
    pub implementing_type: String,
    /// Stable record ID of the implementing type's Symbol record, when the
    /// type resolved to a live record in the graph.
    pub implementing_type_record_id: Option<String>,
    /// `resolved` when [`Self::implementing_type_record_id`] is present,
    /// `parsed_only` otherwise.
    pub implementing_type_resolution: &'static str,
}

/// Resolution outcome for the queried trait handle in the current view.
pub enum ImplementorsResolution<'a> {
    /// At least one live Symbol record matches. An ambiguous name returns
    /// every candidate (sorted by record ID); a candidate is never picked
    /// implicitly.
    Resolved(Vec<&'a GraphRecord>),
    /// Every matching record is tombstoned.
    Stale,
    /// Every live name match has a symbol kind that can never be an
    /// `IMPLEMENTS` target (e.g. a function). Record-ID handles bypass this
    /// filter and always answer for their precise record.
    NonTargetKinds {
        /// Distinct non-target symbol kinds encountered, sorted.
        kinds: Vec<String>,
    },
    /// Nothing matches the handle.
    NoMatch,
}

/// Orders two records that share a stable ID: the current-tree (untemporal)
/// record wins over history records; among history records the
/// chronologically newest `valid_time` wins (ties break on the commit SHA).
///
/// `valid_time` is compared as a parsed instant, never lexically:
/// scan-history stores the committer's LOCAL offset (`%cI`; only `+00:00`
/// is rewritten to `Z`), and RFC 3339 strings with mixed offsets do not
/// sort chronologically as bytes. `DateTime` ordering compares instants, so
/// two spellings of one instant tie and fall through to the SHA tie-break;
/// an unparseable timestamp sorts lowest. Deterministic for identical
/// inputs.
fn implementors_view_key(
    record: &GraphRecord,
) -> (u8, Option<DateTime<chrono::FixedOffset>>, &str) {
    match record {
        GraphRecord::Node {
            temporal: Some(t), ..
        }
        | GraphRecord::Edge {
            temporal: Some(t), ..
        } => (
            0,
            DateTime::parse_from_rfc3339(&t.valid_time).ok(),
            t.git_commit.as_str(),
        ),
        _ => (1, None, ""),
    }
}

fn implementors_tombstoned_ids(records: &[GraphRecord]) -> BTreeSet<&str> {
    records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect()
}

/// Resolves the trait handle (exact symbol name or canonical record ID) to
/// live Symbol candidates in the current view: one record per stable ID,
/// tombstoned IDs excluded, sorted by record ID.
#[must_use]
pub fn implementors_resolve_trait<'r>(
    records: &'r [GraphRecord],
    handle: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> ImplementorsResolution<'r> {
    let deleted = implementors_tombstoned_ids(records);
    let mut best: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    // A current embedded read emits active tombstones but FILTERS OUT the
    // deleted node itself, so a saved record-ID handle used after deletion
    // may see only `Tombstone { deleted_id }` in the stream. The Node-only
    // loop below can never observe that shape — seed the stale signal from
    // the tombstone set directly so the answer is the documented
    // stale_handle, never no_match.
    let mut saw_deleted = deleted.contains(handle);
    let mut non_target_kinds: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name,
            symbol_kind,
            ..
        } = record
        else {
            continue;
        };
        let id_match = id == handle;
        if name.as_deref() != Some(handle) && !id_match {
            continue;
        }
        if let Some(repo_id) = repo
            && index.owner_of(id) != Some(repo_id)
        {
            continue;
        }
        if deleted.contains(id.as_str()) {
            saw_deleted = true;
            continue;
        }
        // Name resolution only considers symbols that can be IMPLEMENTS
        // targets: a `fn Foo` sharing a trait's name lives in a different
        // namespace and would otherwise produce a spurious zero signal. An
        // explicit record-ID handle bypasses the filter — the caller asked
        // for that precise record.
        if !id_match && !is_implements_target_kind(symbol_kind.as_deref()) {
            if let Some(kind) = symbol_kind.as_deref() {
                non_target_kinds.insert(kind);
            }
            continue;
        }
        let replace = best
            .get(id.as_str())
            .is_none_or(|prev| implementors_view_key(record) > implementors_view_key(prev));
        if replace {
            best.insert(id.as_str(), record);
        }
    }
    let candidates: Vec<&GraphRecord> = best.into_values().collect();
    if candidates.is_empty() {
        if saw_deleted {
            ImplementorsResolution::Stale
        } else if non_target_kinds.is_empty() {
            ImplementorsResolution::NoMatch
        } else {
            ImplementorsResolution::NonTargetKinds {
                kinds: non_target_kinds.into_iter().map(str::to_owned).collect(),
            }
        }
    } else {
        ImplementorsResolution::Resolved(candidates)
    }
}

/// As-of variant that returns EVERY same-named symbol candidate at the
/// instant: one record per distinct stable ID — the newest version of that
/// ID at or before `as_of` — sorted by record ID.
///
/// This keeps the implementors ambiguity rule uniform across time views
/// (PR #296 review): two same-named traits in one repository are BOTH
/// reported, never silently reduced to a best-per-repository pick.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbols_as_of_valid_time_per_symbol<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
) -> Result<Vec<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut per_id: BTreeMap<&str, Vec<(&GraphRecord, DateTime<chrono::FixedOffset>)>> =
        BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        per_id.entry(id.as_str()).or_default().push((record, vt));
    }
    Ok(per_id
        .into_values()
        .filter_map(|candidates| pick_as_of_snapshot(&candidates))
        .collect())
}

/// Picks the newest snapshot among one stable ID's candidates at or before
/// the instant.
///
/// Ties at one instant (two commits sharing a committer timestamp) resolve
/// from stored facts, independent of store read order — embedded reads emit
/// snapshots in commit-SHA order, not history order, so an input-order rule
/// would disagree between transports. The unique tied commit that no other
/// tied candidate records as a parent wins (`git_parent_commits` recovers
/// chains); otherwise the largest commit SHA wins — a documented
/// deterministic fallback, because true topological order between unrelated
/// equal-time commits is not recoverable from stored facts.
fn pick_as_of_snapshot<'r>(
    candidates: &[(&'r GraphRecord, DateTime<chrono::FixedOffset>)],
) -> Option<&'r GraphRecord> {
    const fn temporal_of(r: &GraphRecord) -> Option<&TemporalMetadata> {
        match r {
            GraphRecord::Node {
                temporal: Some(t), ..
            } => Some(t),
            _ => None,
        }
    }
    let max_vt = candidates.iter().map(|(_, vt)| *vt).max()?;
    let tied: Vec<&'r GraphRecord> = candidates
        .iter()
        .filter(|(_, vt)| *vt == max_vt)
        .map(|(r, _)| *r)
        .collect();
    if let [only] = tied.as_slice() {
        return Some(only);
    }
    let tied_shas: BTreeSet<&str> = tied
        .iter()
        .filter_map(|r| temporal_of(r).map(|t| t.git_commit.as_str()))
        .collect();
    let parents_of_tied: BTreeSet<&str> = tied
        .iter()
        .filter_map(|r| temporal_of(r))
        .flat_map(|t| t.git_parent_commits.iter().map(String::as_str))
        .filter(|parent| tied_shas.contains(parent))
        .collect();
    let sinks: Vec<&'r GraphRecord> = tied
        .iter()
        .copied()
        .filter(|r| {
            temporal_of(r).is_some_and(|t| !parents_of_tied.contains(t.git_commit.as_str()))
        })
        .collect();
    let sink_shas: BTreeSet<&str> = sinks
        .iter()
        .filter_map(|r| temporal_of(r).map(|t| t.git_commit.as_str()))
        .collect();
    // The SHA fallback stays among the sinks: a tied candidate recorded as
    // another tied candidate's parent is KNOWN not to be newest and must
    // never win, even when its SHA sorts largest. The full tied set is used
    // only when no sink exists (degenerate/cyclic data).
    let pool = if sink_shas.is_empty() { tied } else { sinks };
    pool.into_iter()
        .max_by_key(|r| temporal_of(r).map(|t| t.git_commit.clone()))
}

/// Record-ID variant of [`symbol_as_of_valid_time`].
///
/// Selects the newest Symbol record at or before `as_of` among the records
/// carrying one exact stable ID, so a canonical record-ID handle stays valid
/// under `--as-of` (issue #133 review follow-up). Returns at most one record.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time_by_id<'records>(
    records: &'records [GraphRecord],
    record_id: &str,
    as_of: &str,
) -> Result<Vec<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut candidates: Vec<(&GraphRecord, DateTime<chrono::FixedOffset>)> = Vec::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            temporal,
            valid_time,
            ..
        } = record
        else {
            continue;
        };
        if id != record_id {
            continue;
        }
        let vt_str = temporal
            .as_ref()
            .map(|t| t.valid_time.as_str())
            .or(valid_time.as_deref());
        let Some(vt_str) = vt_str else {
            continue;
        };
        let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
            continue;
        };
        if vt > as_of_dt {
            continue;
        }
        candidates.push((record, vt));
    }
    Ok(pick_as_of_snapshot(&candidates).into_iter().collect())
}

/// Parses the implementing type out of an impl display name such as
/// `impl Renderable for Circle`, `impl<T> GenT for Wrapper<T>`,
/// `mymod::impl Other for Foo<T>`, or the inherent form `impl Circle`.
/// Returns the base type text with generic arguments and reference sigils
/// stripped, or `None` when the name is not impl-shaped.
fn parse_implementing_type(impl_name: &str) -> Option<String> {
    let rest = impl_header_remainder(impl_name)?;
    let type_text = rest.split_once(" for ").map_or(rest, |(_, rhs)| rhs).trim();
    // The extractor's impl display keeps the full header before `{`, so a
    // trailing where clause (`... for Circle where Circle: Clone`) is part
    // of the name and must be stripped before resolution.
    let type_text = type_text
        .split_once(" where ")
        .map_or(type_text, |(lhs, _)| lhs)
        .trim();
    let type_text = strip_reference_prefix(type_text);
    let base = type_text.split('<').next().unwrap_or(type_text).trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_owned())
    }
}

/// Strips a full reference prefix from an impl target: one or more `&`
/// sigils, each optionally followed by a lifetime (`'a`) and/or `mut`
/// (`&mut Foo`, `&'a Foo`, `&'a mut Foo`, `&&Foo`).
fn strip_reference_prefix(text: &str) -> &str {
    let mut t = text.trim();
    while let Some(rest) = t.strip_prefix('&') {
        t = rest.trim_start();
        if let Some(lifetime) = t.strip_prefix('\'') {
            let end = lifetime
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(lifetime.len());
            t = lifetime[end..].trim_start();
        }
        if let Some(rest) = t.strip_prefix("mut ") {
            t = rest.trim_start();
        }
    }
    t
}

/// Returns the text after the `impl` keyword and its optional balanced
/// `<...>` generic-parameter list (`impl<T: Into<String>> ...`), or `None`
/// when the name carries no impl header. `impl` followed by anything other
/// than a space or `<` (e.g. an identifier like `implement_service`) is not
/// a header.
fn impl_header_remainder(impl_name: &str) -> Option<&str> {
    let abs = impl_header_start(impl_name)?;
    let tail = &impl_name[abs + "impl".len()..];
    match tail.chars().next() {
        Some(' ') => Some(tail[1..].trim_start()),
        Some('<') => {
            let mut depth = 0usize;
            for (i, c) in tail.char_indices() {
                match c {
                    '<' => depth += 1,
                    '>' => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some(tail[i + c.len_utf8()..].trim_start());
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        _ => None,
    }
}

/// Byte offset of the `impl` keyword that opens the header (`impl ` or
/// `impl<`), or `None` when the name carries no impl header.
fn impl_header_start(impl_name: &str) -> Option<usize> {
    let mut from = 0;
    loop {
        let rel = impl_name.get(from..)?.find("impl")?;
        let abs = from + rel;
        match impl_name[abs + "impl".len()..].chars().next() {
            Some(' ' | '<') => return Some(abs),
            _ => from = abs + "impl".len(),
        }
    }
}

/// The module path qualifying an impl display name — `outer::inner` from
/// `outer::inner::impl T for self::Foo` — or `None` for a top-level impl.
///
/// A trailing `unsafe` keyword is part of the impl header, never a module
/// segment (`unsafe` is reserved): `outer::unsafe impl T for Foo` qualifies
/// under `outer`, and a root `unsafe impl T for Foo` has no module prefix.
fn impl_module_prefix(impl_name: &str) -> Option<&str> {
    let start = impl_header_start(impl_name)?;
    let mut prefix = impl_name[..start].trim_end();
    if prefix == "unsafe" {
        return None;
    }
    if let Some(stripped) = prefix.strip_suffix("::unsafe") {
        prefix = stripped;
    }
    let prefix = prefix.trim_end_matches("::");
    if prefix.is_empty() {
        None
    } else {
        Some(prefix)
    }
}

/// Returns `true` when a symbol of this kind can be the TARGET of an
/// `IMPLEMENTS` edge.
///
/// Rust traits, type-defining items (inherent impls target structs/enums),
/// and the class/interface kinds of the other extractors qualify. Records
/// without a recorded kind (legacy graphs) are accepted — their kind cannot
/// be judged.
#[must_use]
pub fn is_implements_target_kind(symbol_kind: Option<&str>) -> bool {
    symbol_kind.is_none_or(|kind| kind == "trait" || is_type_defining_kind(Some(kind)))
}

/// Returns `true` when `symbol_kind` defines a type that can implement a
/// trait/base: Rust type items plus the class/interface kinds the Python,
/// TypeScript, and Go extractors attach to `IMPLEMENTS` edge sources.
fn is_type_defining_kind(symbol_kind: Option<&str>) -> bool {
    matches!(
        symbol_kind,
        Some("struct" | "enum" | "union" | "type" | "type_alias" | "class" | "interface")
    )
}

/// Collects inbound `IMPLEMENTS` rows for one resolved trait candidate.
///
/// With `pin_commit == Some(sha)` an edge is in the view when its version at
/// that exact commit is present, or — because an embedded store keeps only
/// the latest physical version of each edge — when its source is an
/// `impl`-block symbol with a snapshot at the pinned commit (the impl block's
/// identity encodes the implemented trait, so its presence at the commit
/// implies the relationship; type-source edges get no such fallback). The
/// impl node is taken at that commit when available (temporal pinning). With `None` the current view is used: edges are
/// deduplicated by stable ID (current-tree record preferred, else the newest
/// history record), and both tombstoned impl symbols and tombstoned edge
/// records (incremental invalidation tombstones a disappeared relationship
/// by its edge record ID) are excluded.
///
/// Implementing-type resolution never crosses the repository boundary:
/// a candidate type record is considered only when `index` attributes it to
/// the same repository as the impl record (unattributed legacy records form
/// their own group). A same-named type in another repository is never cited;
/// the row falls back to `parsed_only`.
///
/// Rows are sorted by `(impl record ID, edge record ID)` — a stable
/// tie-break that keeps output byte-identical across runs.
#[must_use]
pub fn implementors_rows_for<'r>(
    records: &'r [GraphRecord],
    trait_record: &'r GraphRecord,
    pin_commit: Option<&str>,
    index: &RepositoryIndex,
) -> Vec<ImplementorLead<'r>> {
    let trait_id = trait_record.id();
    let deleted = implementors_tombstoned_ids(records);

    // One Symbol record per stable ID for impl lookup and type resolution.
    // When pinned, a record at exactly the pinned commit wins. `pinned_nodes`
    // tracks which symbol IDs have a snapshot at the pinned commit.
    let mut node_view: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut pinned_nodes: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if let Some(pin) = pin_commit {
            let at_pin = temporal.as_ref().is_some_and(|t| t.git_commit == pin);
            if at_pin {
                pinned_nodes.insert(id.as_str());
            }
            let prev_at_pin = node_view.get(id.as_str()).is_some_and(|prev| {
                matches!(
                    prev,
                    GraphRecord::Node { temporal: Some(t), .. } if t.git_commit == pin
                )
            });
            if at_pin && !prev_at_pin {
                node_view.insert(id.as_str(), record);
                continue;
            }
            if prev_at_pin {
                continue;
            }
        }
        let replace = node_view
            .get(id.as_str())
            .is_none_or(|prev| implementors_view_key(record) > implementors_view_key(prev));
        if replace {
            node_view.insert(id.as_str(), record);
        }
    }

    // Select the inbound IMPLEMENTS edges for this trait.
    let mut edge_view: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Edge {
            id,
            label: EdgeLabel::Implements,
            source,
            target,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if target != trait_id {
            continue;
        }
        if let Some(pin) = pin_commit {
            if temporal.as_ref().is_some_and(|t| t.git_commit == pin) {
                // The exact commit version always wins over a fallback pick.
                edge_view.insert(id.as_str(), record);
                continue;
            }
            // Collapsed-store fallback: an embedded store keeps only the
            // LATEST physical version of each edge (per stable edge ID), so
            // the pinned commit's edge version may be unavailable even though
            // the relationship held there. The fallback is applied only when
            // it is sound: an `impl`-block source symbol whose snapshot
            // exists at the pinned commit — the impl block's identity encodes
            // the trait it implements, so its presence at the commit implies
            // this exact relationship. Type-source edges (Python/TS classes,
            // Go embedding) are excluded: heritage can change without the
            // class identity changing, and fabricating a pinned relationship
            // from them would be a guess.
            if pinned_nodes.contains(source.as_str())
                && node_view.get(source.as_str()).is_some_and(|n| {
                    matches!(
                        n,
                        GraphRecord::Node { symbol_kind: Some(k), .. } if k == "impl"
                    )
                })
            {
                edge_view.entry(id.as_str()).or_insert(record);
            }
        } else {
            // Incremental invalidation tombstones edge record IDs themselves
            // when a relationship disappears from a rebuilt file, so the
            // current view must drop a tombstoned edge even when its source
            // symbol is still live — never resurface a stale implementor.
            if deleted.contains(id.as_str()) || deleted.contains(source.as_str()) {
                continue;
            }
            let replace = edge_view
                .get(id.as_str())
                .is_none_or(|prev| implementors_view_key(record) > implementors_view_key(prev));
            if replace {
                edge_view.insert(id.as_str(), record);
            }
        }
    }

    let mut rows: Vec<ImplementorLead<'r>> = Vec::new();
    for edge in edge_view.values() {
        let GraphRecord::Edge { source, .. } = edge else {
            continue;
        };
        let Some(impl_record) = node_view.get(source.as_str()).copied() else {
            // An IMPLEMENTS edge whose source node is absent from the store
            // cannot produce a citable row; it is skipped rather than
            // fabricated.
            continue;
        };
        let impl_name = if let GraphRecord::Node { name, .. } = impl_record {
            name.as_deref().unwrap_or("")
        } else {
            ""
        };
        let parsed = parse_implementing_type(impl_name);
        // Python class->base, TS heritage, and Go embedding edges carry the
        // implementing TYPE as the edge source (there is no `impl X for Y`
        // display name): the source record itself is the citable
        // implementing type. The same holds when the source name is not
        // impl-shaped at all.
        let source_symbol_kind = if let GraphRecord::Node { symbol_kind, .. } = impl_record {
            symbol_kind.as_deref()
        } else {
            None
        };
        let source_is_type = is_type_defining_kind(source_symbol_kind);
        // An impl-kind source is never itself the implementing type: even
        // when its display name fails to parse, degrading to parsed_only is
        // honest while citing the impl record would be a wrong handle.
        if source_is_type
            || (parsed.is_none() && !impl_name.is_empty() && source_symbol_kind != Some("impl"))
        {
            rows.push(ImplementorLead {
                trait_record,
                edge,
                impl_record,
                implementing_type: impl_name.to_owned(),
                implementing_type_record_id: Some(impl_record.id().to_owned()),
                implementing_type_resolution: "resolved",
            });
            continue;
        }
        let resolved = parsed.as_deref().and_then(|base| {
            resolve_implementing_type(
                &node_view,
                &deleted,
                impl_record,
                base,
                index,
                pin_commit.is_some().then_some(&pinned_nodes),
            )
        });
        let (implementing_type, implementing_type_record_id, implementing_type_resolution) =
            if let (Some(base), Some(resolved)) = (parsed.as_deref(), resolved) {
                let resolved_name = if let GraphRecord::Node {
                    name: Some(name), ..
                } = resolved
                {
                    name.clone()
                } else {
                    base.to_owned()
                };
                (resolved_name, Some(resolved.id().to_owned()), "resolved")
            } else {
                (
                    parsed.unwrap_or_else(|| impl_name.to_owned()),
                    None,
                    "parsed_only",
                )
            };
        rows.push(ImplementorLead {
            trait_record,
            edge,
            impl_record,
            implementing_type,
            implementing_type_record_id,
            implementing_type_resolution,
        });
    }

    rows.sort_by(|a, b| {
        a.impl_record
            .id()
            .cmp(b.impl_record.id())
            .then_with(|| a.edge.id().cmp(b.edge.id()))
    });
    rows
}

/// Resolves the parsed implementing-type text against the graph's Symbol
/// records. Only records `index` attributes to the impl's own repository are
/// considered (unattributed legacy records form their own group), so a
/// same-named type in another repository is never cited. Under a pinned
/// view (`pinned_nodes` set) only symbols with a snapshot at the pinned
/// commit are candidates, so a type introduced at a later commit is never
/// cited into the past. Name matching is path-aware: `self::`/`super::`
/// resolve in the impl's module tree and `crate::` at the crate root, all
/// matched exactly; any other qualified path matches only its module-scoped
/// or repo-root exact form (never an arbitrary-parent suffix); only an
/// unqualified base matches bare names. Preference is deterministic: for unqualified targets a type in
/// the impl's OWN module wins first (Rust's innermost scope), then a
/// type-defining symbol in the impl's own file, then any type-defining
/// symbol elsewhere, then non-type fallbacks (same file first, then
/// exact-name matches), then the lowest record ID.
/// Computes the matching needle for a parsed implementing-type path, plus
/// whether it must match a stored name exactly.
///
/// `self::` names the impl's OWN module and `super::` (possibly repeated)
/// walks UP its module prefix — both recoverable from the impl symbol's
/// qualified display name and matched exactly. `crate::` is an absolute
/// crate-root path, matched exactly. Any other qualified path
/// (`external::Foo`) is a relative path: it matches only the module-scoped
/// exact form (`<impl module prefix>::external::Foo`) or the repo-root
/// exact form — never an arbitrary-parent suffix. Only an unqualified base
/// matches bare names. Returns `None` when the path is not derivable from
/// stored facts (more `super::` levels than the impl's module prefix has
/// segments) — the row then degrades honestly to `parsed_only`.
fn implementing_type_needle(base: &str, impl_name: &str) -> Option<(Vec<String>, bool)> {
    if let Some(path) = base.strip_prefix("self::") {
        let scoped = impl_module_prefix(impl_name)
            .map_or_else(|| path.to_owned(), |prefix| format!("{prefix}::{path}"));
        return Some((vec![scoped], true));
    }
    if let Some(path) = base.strip_prefix("crate::") {
        return Some((vec![path.to_owned()], true));
    }
    if base.starts_with("super::") {
        let mut rest = base;
        let mut supers = 0usize;
        while let Some(stripped) = rest.strip_prefix("super::") {
            rest = stripped;
            supers += 1;
        }
        let segments: Vec<&str> = impl_module_prefix(impl_name)
            .map(|prefix| prefix.split("::").collect())
            .unwrap_or_default();
        if supers > segments.len() {
            return None;
        }
        let kept = &segments[..segments.len() - supers];
        let scoped = if kept.is_empty() {
            rest.to_owned()
        } else {
            format!("{}::{rest}", kept.join("::"))
        };
        return Some((vec![scoped], true));
    }
    if base.contains("::") {
        // A RELATIVE qualified path resolves in the impl's own module
        // (`<module prefix>::<path>`, the innermost scope) or as a repo-root
        // module path — both EXACT. Suffix-matching arbitrary parent modules
        // could cite an unrelated `other::external::Foo` for
        // `external::Foo`; when neither exact form exists the path is
        // crate-external and the row stays parsed_only.
        let mut needles = Vec::new();
        if let Some(prefix) = impl_module_prefix(impl_name) {
            needles.push(format!("{prefix}::{base}"));
        }
        needles.push(base.to_owned());
        return Some((needles, true));
    }
    Some((vec![base.to_owned()], false))
}

/// Candidate preference for implementing-type resolution:
/// `(module_scoped, type_kind, same_file, exact)`.
type ImplementorCandidateRank = (bool, bool, bool, bool);

fn resolve_implementing_type<'r>(
    node_view: &BTreeMap<&str, &'r GraphRecord>,
    deleted: &BTreeSet<&str>,
    impl_record: &'r GraphRecord,
    base: &str,
    index: &RepositoryIndex,
    pinned_nodes: Option<&BTreeSet<&str>>,
) -> Option<&'r GraphRecord> {
    let impl_path = if let GraphRecord::Node {
        repo_relative_path, ..
    } = impl_record
    {
        repo_relative_path.as_deref()
    } else {
        None
    };
    let impl_owner = index.owner_of(impl_record.id());
    let impl_name = if let GraphRecord::Node {
        name: Some(name), ..
    } = impl_record
    {
        name.as_str()
    } else {
        ""
    };
    let (needles, exact_only) = implementing_type_needle(base, impl_name)?;
    // Rust name resolution: the impl's own module is the innermost scope, so
    // its module-scoped form outranks every other match — for an UNQUALIFIED
    // target `<module prefix>::<base>` beats bare/suffix matches (a same-file
    // top-level namesake must not win), and for a relative qualified target
    // the scoped needle beats the repo-root form. When the module has no
    // such type the remaining rules below still apply — use-imported types
    // keep resolving through them, a documented residual imprecision.
    let module_scoped_needle: Option<String> = if exact_only {
        (needles.len() > 1).then(|| needles[0].clone())
    } else {
        impl_module_prefix(impl_name).map(|prefix| format!("{prefix}::{base}"))
    };

    let mut best: Option<(ImplementorCandidateRank, &'r GraphRecord)> = None;
    for candidate in node_view.values() {
        let GraphRecord::Node {
            id,
            name: Some(name),
            symbol_kind,
            repo_relative_path,
            temporal,
            ..
        } = candidate
        else {
            continue;
        };
        if symbol_kind.as_deref() == Some("impl") {
            continue;
        }
        if temporal.is_none() && deleted.contains(id.as_str()) {
            continue;
        }
        if index.owner_of(id) != impl_owner {
            continue;
        }
        // Pinned views resolve against the pinned commit only: a type whose
        // Symbol record was introduced at a LATER commit did not exist at the
        // pin, and citing its record would fabricate history. The row falls
        // back to parsed_only instead.
        if let Some(pinned) = pinned_nodes
            && !pinned.contains(id.as_str())
        {
            continue;
        }
        let exact = name == base;
        let path_match = if exact_only {
            needles.iter().any(|needle| name == needle)
        } else {
            let needle = &needles[0];
            name == needle || name.ends_with(&format!("::{needle}"))
        };
        if !exact && !path_match {
            continue;
        }
        let same_file = repo_relative_path.as_deref() == impl_path;
        let type_kind = is_type_defining_kind(symbol_kind.as_deref());
        let module_scoped = module_scoped_needle.as_deref() == Some(name.as_str());
        let key = (module_scoped, type_kind, same_file, exact);
        let better = match &best {
            None => true,
            Some((prev_key, prev)) => {
                key > *prev_key || (key == *prev_key && candidate.id() < prev.id())
            }
        };
        if better {
            best = Some((key, candidate));
        }
    }
    best.map(|(_, record)| record)
}
