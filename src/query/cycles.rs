use std::collections::{BTreeMap, BTreeSet};

use super::{RepositoryIndex, ResolvedFailureTarget, imported_symbol_names, last_path_segment};
use crate::ir::{CallResolution, EdgeLabel, GraphRecord, NodeKind};

// ============================================================================
// Dependency-cycle query (issue #138)
// ============================================================================

/// Maximum elementary cycles enumerated before truncation (with a
/// `cycles_truncated` diagnostic, never a silent cutoff).
const MAX_DEPENDENCY_CYCLES: usize = 100;

/// Maximum citation records serialized per cycle edge; the full count is
/// always reported in `evidence_total`.
const MAX_CYCLE_EVIDENCE_PER_EDGE: usize = 8;

/// A `(repository_id, repo_relative_path)` pair identifying one file node of
/// the dependency graph. The repository component keeps same-path files from
/// different repositories distinct in a multi-repo store.
type FileKey<'a> = (&'a str, &'a str);

/// One citable graph record contributing to a file-level dependency edge.
#[derive(Debug, Clone)]
pub struct CycleEvidence<'a> {
    /// Wire relation label the dependency was derived from
    /// (`"CALLS"` or `"IMPORTS"`).
    pub relation: &'static str,
    /// Stable record ID of the contributing `CALLS` edge or `Import` node.
    pub record_id: &'a str,
    /// Call resolution status carried by a contributing `CALLS` edge
    /// (issues #152/#134); `None` for import-derived evidence and for edges
    /// outside the resolution contract.
    pub resolution: Option<CallResolution>,
}

/// One directed file-level dependency edge closing part of a cycle.
#[derive(Debug, Clone)]
pub struct CycleEdge<'a> {
    /// Repo-relative path of the depending file.
    pub from: &'a str,
    /// Repo-relative path of the depended-on file.
    pub to: &'a str,
    /// Sorted unique relation labels contributing to this edge.
    pub relations: Vec<&'static str>,
    /// Contributing records, sorted by (relation, record ID) and capped at
    /// [`MAX_CYCLE_EVIDENCE_PER_EDGE`].
    pub evidence: Vec<CycleEvidence<'a>>,
    /// Total contributing records before the evidence cap.
    pub evidence_total: usize,
}

/// One file participating in a dependency cycle.
#[derive(Debug, Clone)]
pub struct CycleMember<'a> {
    /// Stable record ID of the `File` node.
    pub record_id: &'a str,
    /// Repo-relative path of the file.
    pub repo_relative_path: &'a str,
}

/// One dependency cycle in canonical form.
///
/// Members are rotated so the cycle starts at its lexicographically smallest
/// file, and `edges[i]` connects `members[i]` to `members[(i + 1) % len]`,
/// closing the loop.
#[derive(Debug, Clone)]
pub struct DependencyCycle<'a> {
    /// Ordered cycle members (each file appears exactly once).
    pub members: Vec<CycleMember<'a>>,
    /// The directed edges closing the loop, one per member.
    pub edges: Vec<CycleEdge<'a>>,
}

/// Deterministic tallies for the dependency graph and every edge class that
/// was considered, including the excluded ones — exclusions are counted,
/// never silently dropped.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct DependencyCycleCounts {
    /// File nodes in the dependency graph.
    pub files: usize,
    /// Distinct directed file-to-file dependency edges.
    pub dependency_edges: usize,
    /// Cross-file `CALLS` edges that participated (labeled `resolved`).
    pub calls_resolved: usize,
    /// `CALLS` edges labeled `ambiguous`, excluded from cycle detection.
    pub calls_ambiguous_excluded: usize,
    /// `CALLS` edges labeled `unresolved`, excluded from cycle detection.
    pub calls_unresolved_excluded: usize,
    /// Cross-file `CALLS` edges carrying no resolution label (older or
    /// third-party stores predating issues #152/#134), excluded from cycle
    /// detection: absence means "outside the resolution contract", never
    /// "resolved". Same-file unlabeled edges are not tallied — they can
    /// never contribute a file-level dependency edge.
    pub calls_unlabeled_excluded: usize,
    /// Import items that resolved to exactly one in-repo defining file.
    pub imports_resolved: usize,
    /// Import items matching two or more in-repo defining files, excluded.
    pub imports_ambiguous_excluded: usize,
    /// Import items with no in-repo defining file (external crates/packages).
    pub imports_external: usize,
    /// Non-Rust `Import` declarations, excluded from cycle detection: import
    /// name resolution is Rust-only in this slice, and a raw Python /
    /// TypeScript / Go import statement must be tallied — never silently
    /// treated as external or reported as a bare "acyclic".
    pub imports_non_rust_excluded: usize,
}

/// A stable machine-readable condition attached to the cycle result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DependencyCycleDiagnostic {
    /// Stable diagnostic code (`acyclic`, `ambiguous_dependencies_excluded`,
    /// `unresolved_calls_excluded`, `unlabeled_calls_excluded`,
    /// `non_rust_imports_excluded`, `cycles_truncated`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    pub record_id: Option<String>,
    /// Bounded human-readable detail (paths and counts only — never payload).
    pub detail: String,
}

/// The enumerated dependency cycles plus tallies and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct DependencyCycles<'a> {
    /// Canonically ordered cycles (see [`DependencyCycle`]); scope-filtered
    /// when a scope target was supplied.
    pub cycles: Vec<DependencyCycle<'a>>,
    /// Cycles found in the whole (repo-scoped) graph before scope filtering.
    pub cycles_total: usize,
    /// Dependency-graph and edge-class tallies.
    pub counts: DependencyCycleCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<DependencyCycleDiagnostic>,
    /// `true` when enumeration stopped at [`MAX_DEPENDENCY_CYCLES`].
    pub truncated: bool,
}

/// Johnson-style blocked DFS state for elementary-cycle enumeration rooted at
/// `root`. Only vertices `>= root` are visited, so every elementary cycle is
/// discovered exactly once, already rotated to start at its smallest vertex —
/// rotations and self-restarts can never produce duplicates.
struct CycleSearch<'g> {
    adj: &'g [Vec<usize>],
    root: usize,
    blocked: Vec<bool>,
    block_map: Vec<BTreeSet<usize>>,
    stack: Vec<usize>,
    found_cycles: Vec<Vec<usize>>,
    remaining: usize,
    truncated: bool,
}

impl CycleSearch<'_> {
    fn unblock(&mut self, v: usize) {
        if !self.blocked[v] {
            return;
        }
        self.blocked[v] = false;
        let waiting = std::mem::take(&mut self.block_map[v]);
        for w in waiting {
            self.unblock(w);
        }
    }

    fn circuit(&mut self, v: usize) -> bool {
        let mut found = false;
        self.stack.push(v);
        self.blocked[v] = true;
        for i in 0..self.adj[v].len() {
            let w = self.adj[v][i];
            if w < self.root || self.truncated {
                continue;
            }
            if w == self.root {
                if self.remaining > 0 {
                    self.found_cycles.push(self.stack.clone());
                    self.remaining -= 1;
                } else {
                    self.truncated = true;
                }
                found = true;
            } else if !self.blocked[w] && self.circuit(w) {
                found = true;
            }
        }
        if found {
            self.unblock(v);
        } else {
            for i in 0..self.adj[v].len() {
                let w = self.adj[v][i];
                if w >= self.root {
                    self.block_map[w].insert(v);
                }
            }
        }
        self.stack.pop();
        found
    }
}

/// Enumerates every elementary cycle of the digraph `adj` (adjacency lists of
/// sorted vertex indices), up to `cap` cycles. Returns the cycles — each
/// starting at its smallest vertex — plus a truncation flag.
fn enumerate_elementary_cycles(adj: &[Vec<usize>], cap: usize) -> (Vec<Vec<usize>>, bool) {
    let n = adj.len();
    let mut all: Vec<Vec<usize>> = Vec::new();
    let mut truncated = false;
    for root in 0..n {
        if truncated {
            break;
        }
        let mut search = CycleSearch {
            adj,
            root,
            blocked: vec![false; n],
            block_map: vec![BTreeSet::new(); n],
            stack: Vec::new(),
            found_cycles: Vec::new(),
            remaining: cap - all.len(),
            truncated: false,
        };
        search.circuit(root);
        truncated |= search.truncated;
        all.extend(search.found_cycles);
    }
    (all, truncated)
}

/// Compute the file-level dependency cycles of a scanned graph (issue #138).
///
/// The dependency graph is built from already-extracted records only — no
/// re-parsing, no network:
///
/// - **`CALLS` edges** (symbol → symbol): a cross-file edge contributes a
///   `caller-file → callee-file` dependency **only when labeled `resolved`**
///   by the resolution passes (issues #152/#134). Edges labeled `ambiguous`
///   or `unresolved` are excluded from cycle detection and tallied — an
///   ambiguous edge must never fabricate a cycle. A cross-file edge carrying
///   **no** resolution label (an older or third-party store predating the
///   resolution field) is likewise excluded and tallied: absence means
///   "outside the resolution contract", never "resolved". Excluded-edge
///   tallies are scoped to the calling symbol's repository, so a
///   `--repo`-scoped response never reports another repository's edges.
/// - **`IMPORTS` declarations** (`File/Module → Import` nodes): each imported
///   item (grouped and aliased imports expanded) is name-matched against
///   symbol definitions in the same repository. Exactly one defining file →
///   an `importing-file → defining-file` dependency; two or more candidate
///   files → ambiguous, excluded and tallied; none → external, tallied.
///   Import name resolution is **Rust-only** in this slice: non-Rust
///   `Import` nodes (Python / TypeScript / Go raw statement text) are
///   excluded and tallied with a diagnostic — an unparseable import is never
///   silently treated as external, and their absence from the cycle set is
///   never a bare acyclicity claim.
///
/// Same-file dependencies never form an edge, so self-loops are excluded by
/// construction. When `repo_scope` is set, only that repository's files and
/// records participate. When `scope_target` is set (the pre-refactor check),
/// the returned cycles are filtered to those containing any of the target's
/// resolved files, while `cycles_total` still reports the unfiltered count.
///
/// Enumeration is deterministic and canonical: vertices are ordered by
/// (path, repository), each elementary cycle starts at its lexicographically
/// smallest member, the cycle set is sorted by its member sequence, and
/// repeated runs over the same records are byte-identical.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn dependency_cycles<'a>(
    records: &'a [GraphRecord],
    repo_index: &'a RepositoryIndex,
    repo_scope: Option<&str>,
    scope_target: Option<&ResolvedFailureTarget>,
) -> DependencyCycles<'a> {
    let mut result = DependencyCycles::default();

    // ── tombstone / temporal filtering (mirrors change_impact_context) ────────
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let has_temporal: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let deleted = |id: &str| tombstoned.contains(id) && !has_temporal.contains(id);
    let owner = |id: &str| repo_index.owner_of(id).unwrap_or("");
    let in_scope = |id: &str| repo_scope.is_none_or(|scope| owner(id) == scope);

    // ── file nodes of the dependency graph ────────────────────────────────────
    // One node per (repository, path); when a stable path appears through more
    // than one record (history graphs) the smallest record ID represents it
    // deterministically.
    let mut files: BTreeMap<FileKey<'a>, &'a GraphRecord> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if deleted(id.as_str()) || !in_scope(id.as_str()) {
                continue;
            }
            files
                .entry((owner(id.as_str()), path.as_str()))
                .and_modify(|existing| {
                    if id.as_str() < existing.id() {
                        *existing = r;
                    }
                })
                .or_insert(r);
        }
    }

    // ── symbol → owning file, and name → defining files (import matching) ─────
    let mut symbol_file: BTreeMap<&'a str, FileKey<'a>> = BTreeMap::new();
    let mut defs_by_name: BTreeMap<&'a str, BTreeSet<FileKey<'a>>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            repo_relative_path: Some(path),
            name,
            ..
        } = r
        {
            if deleted(id.as_str()) || !in_scope(id.as_str()) {
                continue;
            }
            let key = (owner(id.as_str()), path.as_str());
            symbol_file.insert(id.as_str(), key);
            if let Some(name) = name {
                defs_by_name
                    .entry(last_path_segment(name))
                    .or_default()
                    .insert(key);
            }
        }
    }

    // ── dependency edges ──────────────────────────────────────────────────────
    let mut adjacency: BTreeMap<FileKey<'a>, BTreeSet<FileKey<'a>>> = BTreeMap::new();
    #[allow(clippy::type_complexity)]
    let mut edge_evidence: BTreeMap<
        (FileKey<'a>, FileKey<'a>),
        BTreeMap<(&'static str, &'a str), Option<CallResolution>>,
    > = BTreeMap::new();
    let mut add_dependency = |from: FileKey<'a>,
                              to: FileKey<'a>,
                              relation: &'static str,
                              record_id: &'a str,
                              resolution: Option<CallResolution>| {
        adjacency.entry(from).or_default().insert(to);
        edge_evidence
            .entry((from, to))
            .or_default()
            .insert((relation, record_id), resolution);
    };

    // CALLS edges: resolved edges drive cycle detection; ambiguous,
    // unresolved, and unlabeled cross-file edges are excluded and tallied,
    // never silently dropped.
    for r in records {
        let GraphRecord::Edge {
            id,
            label: EdgeLabel::Calls,
            source,
            target,
            resolution,
            ..
        } = r
        else {
            continue;
        };
        if deleted(id.as_str()) {
            continue;
        }
        // An edge belongs to its calling symbol's repository: resolve the
        // caller first so a `--repo`-scoped response never tallies (or
        // traverses) another repository's edges. `symbol_file` holds only
        // in-scope symbols.
        let Some(&from) = symbol_file.get(source.as_str()) else {
            continue;
        };
        match resolution {
            Some(CallResolution::Ambiguous) => {
                result.counts.calls_ambiguous_excluded += 1;
                continue;
            }
            Some(CallResolution::Unresolved) => {
                result.counts.calls_unresolved_excluded += 1;
                continue;
            }
            Some(CallResolution::Resolved) | None => {}
        }
        let Some(&to) = symbol_file.get(target.as_str()) else {
            continue;
        };
        if from == to || !files.contains_key(&from) || !files.contains_key(&to) {
            continue;
        }
        // A cross-file edge with no resolution label is outside the
        // resolution contract (older or third-party store predating issues
        // #152/#134): absence never means "resolved", so it must not close
        // a cycle. Excluded and tallied.
        if resolution.is_none() {
            result.counts.calls_unlabeled_excluded += 1;
            continue;
        }
        result.counts.calls_resolved += 1;
        add_dependency(from, to, "CALLS", id.as_str(), *resolution);
    }

    // IMPORTS declarations: each imported item resolves by name against
    // same-repository symbol definitions. Exactly one defining file is a
    // dependency; several candidate files are ambiguous and excluded; no
    // candidate is an external import. Import name resolution is Rust-only
    // in this slice: non-Rust `Import` nodes carry raw statement text
    // (`from a import X`, `import { X } from "./b"`, `"pkg/path"`) that Rust
    // path parsing cannot resolve, so they are excluded and tallied — never
    // silently folded into the external tally.
    for r in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Import,
            name: Some(name),
            repo_relative_path: Some(path),
            language,
            ..
        } = r
        else {
            continue;
        };
        if deleted(id.as_str()) || !in_scope(id.as_str()) {
            continue;
        }
        if language.as_deref() != Some("rust") {
            result.counts.imports_non_rust_excluded += 1;
            continue;
        }
        let repo = owner(id.as_str());
        let from = (repo, path.as_str());
        if !files.contains_key(&from) {
            continue;
        }
        for segment in imported_symbol_names(name) {
            let candidates: Vec<FileKey<'a>> = defs_by_name
                .get(segment)
                .map(|set| {
                    set.iter()
                        .filter(|(candidate_repo, _)| *candidate_repo == repo)
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            match candidates.as_slice() {
                [] => result.counts.imports_external += 1,
                [to] => {
                    result.counts.imports_resolved += 1;
                    if *to != from {
                        add_dependency(from, *to, "IMPORTS", id.as_str(), None);
                    }
                }
                _ => result.counts.imports_ambiguous_excluded += 1,
            }
        }
    }

    result.counts.files = files.len();
    result.counts.dependency_edges = edge_evidence.len();

    // ── canonical vertex order and cycle enumeration ──────────────────────────
    let mut keys: Vec<FileKey<'a>> = files.keys().copied().collect();
    keys.sort_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)));
    let index_of: BTreeMap<FileKey<'a>, usize> =
        keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); keys.len()];
    for (from, targets) in &adjacency {
        for to in targets {
            adj[index_of[from]].push(index_of[to]);
        }
    }
    for list in &mut adj {
        list.sort_unstable();
    }

    let (mut cycles, truncated) = enumerate_elementary_cycles(&adj, MAX_DEPENDENCY_CYCLES);
    result.truncated = truncated;
    cycles.sort();
    result.cycles_total = cycles.len();

    // ── scope filtering (the pre-refactor check) ──────────────────────────────
    if let Some(target) = scope_target {
        let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
        let mut scope_indices: BTreeSet<usize> = BTreeSet::new();
        for anchor in &target.anchor_ids {
            if let Some(&record) = by_id.get(anchor.as_str())
                && let GraphRecord::Node {
                    repo_relative_path: Some(path),
                    ..
                } = record
                && let Some(&idx) = index_of.get(&(owner(anchor), path.as_str()))
            {
                scope_indices.insert(idx);
            }
        }
        cycles.retain(|cycle| cycle.iter().any(|v| scope_indices.contains(v)));
    }

    // ── materialize cycles with citations ─────────────────────────────────────
    result.cycles = cycles
        .iter()
        .map(|indices| {
            let members: Vec<CycleMember<'a>> = indices
                .iter()
                .map(|&i| {
                    let key = keys[i];
                    CycleMember {
                        record_id: files[&key].id(),
                        repo_relative_path: key.1,
                    }
                })
                .collect();
            let edges: Vec<CycleEdge<'a>> = indices
                .iter()
                .enumerate()
                .map(|(pos, &i)| {
                    let from = keys[i];
                    let to = keys[indices[(pos + 1) % indices.len()]];
                    let contributions = edge_evidence.get(&(from, to));
                    let evidence_total = contributions.map_or(0, BTreeMap::len);
                    let relations: Vec<&'static str> = contributions
                        .map(|map| {
                            map.keys()
                                .map(|(relation, _)| *relation)
                                .collect::<BTreeSet<_>>()
                                .into_iter()
                                .collect()
                        })
                        .unwrap_or_default();
                    let evidence: Vec<CycleEvidence<'a>> = contributions
                        .into_iter()
                        .flatten()
                        .take(MAX_CYCLE_EVIDENCE_PER_EDGE)
                        .map(|(&(relation, record_id), &resolution)| CycleEvidence {
                            relation,
                            record_id,
                            resolution,
                        })
                        .collect();
                    CycleEdge {
                        from: from.1,
                        to: to.1,
                        relations,
                        evidence,
                        evidence_total,
                    }
                })
                .collect();
            DependencyCycle { members, edges }
        })
        .collect();

    // ── diagnostics ───────────────────────────────────────────────────────────
    if result.cycles.is_empty() {
        result.diagnostics.push(DependencyCycleDiagnostic {
            code: "acyclic",
            record_id: None,
            detail: if files.is_empty() {
                "graph contains no code-graph file records".to_owned()
            } else if scope_target.is_some() {
                "no dependency cycle includes the scoped node".to_owned()
            } else {
                "no dependency cycles detected over resolved CALLS and \
                 unambiguous IMPORTS dependency edges"
                    .to_owned()
            },
        });
    }
    let ambiguous_excluded =
        result.counts.calls_ambiguous_excluded + result.counts.imports_ambiguous_excluded;
    if ambiguous_excluded > 0 {
        result.diagnostics.push(DependencyCycleDiagnostic {
            code: "ambiguous_dependencies_excluded",
            record_id: None,
            detail: format!(
                "{} ambiguous CALLS edge(s) and {} ambiguous import(s) were \
                 excluded from cycle detection; ambiguity never fabricates a cycle",
                result.counts.calls_ambiguous_excluded, result.counts.imports_ambiguous_excluded
            ),
        });
    }
    if result.counts.calls_unresolved_excluded > 0 {
        result.diagnostics.push(DependencyCycleDiagnostic {
            code: "unresolved_calls_excluded",
            record_id: None,
            detail: format!(
                "{} unresolved CALLS edge(s) have no in-repo target and were \
                 excluded from cycle detection",
                result.counts.calls_unresolved_excluded
            ),
        });
    }
    if result.counts.calls_unlabeled_excluded > 0 {
        result.diagnostics.push(DependencyCycleDiagnostic {
            code: "unlabeled_calls_excluded",
            record_id: None,
            detail: format!(
                "{} cross-file CALLS edge(s) carry no resolution label \
                 (store predates issues #152/#134?) and were excluded from \
                 cycle detection; absence never means resolved — re-scan to \
                 label them",
                result.counts.calls_unlabeled_excluded
            ),
        });
    }
    if result.counts.imports_non_rust_excluded > 0 {
        result.diagnostics.push(DependencyCycleDiagnostic {
            code: "non_rust_imports_excluded",
            record_id: None,
            detail: format!(
                "{} non-Rust import declaration(s) are outside this slice's \
                 Rust-only import name resolution and were excluded from \
                 cycle detection; their absence from the cycle set is not \
                 proof of acyclicity",
                result.counts.imports_non_rust_excluded
            ),
        });
    }
    if result.truncated {
        result.diagnostics.push(DependencyCycleDiagnostic {
            code: "cycles_truncated",
            record_id: None,
            detail: format!(
                "cycle enumeration stopped at {MAX_DEPENDENCY_CYCLES} cycles; \
                 the returned set is the canonical prefix"
            ),
        });
    }
    result.diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    result.diagnostics.dedup();

    result
}
