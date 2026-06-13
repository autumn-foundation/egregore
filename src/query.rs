//! Agent-facing graph query helpers.
#![allow(
    clippy::uninlined_format_args,
    clippy::manual_let_else,
    clippy::collapsible_if,
    clippy::match_like_matches_macro,
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::cast_precision_loss
)]

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use crate::ir::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SemanticDriftMetadata, UserContextScope,
};
use crate::redaction::redact_value;
/// Finds a symbol record by name at a specific Git commit.
///
/// `commit` may be a full SHA or a unique prefix from the caller's graph.
#[must_use]
pub fn symbol_at_commit<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    commit: &str,
) -> Option<&'records GraphRecord> {
    symbols_at_commit(records, symbol_name, commit)
        .into_iter()
        .next()
}

/// Returns every symbol record matching `symbol_name` at a specific Git
/// commit, sorted by record ID for deterministic output.
///
/// In a multi-repository store the same name/commit pair can match records in
/// more than one repository; callers that must answer with a single record
/// use the full list to keep the repository boundary visible instead of
/// picking one implicitly (issue #67).
#[must_use]
pub fn symbols_at_commit<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    commit: &str,
) -> Vec<&'records GraphRecord> {
    let mut matches = records
        .iter()
        .filter(|record| matches_symbol_at_commit(record, symbol_name, commit))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.id().cmp(right.id()));
    matches
}

/// Returns semantic drift nodes ranked by score descending.
#[must_use]
pub fn largest_semantic_drifts(records: &[GraphRecord], limit: usize) -> Vec<&GraphRecord> {
    let mut drifts = records
        .iter()
        .filter_map(|record| semantic_drift(record).map(|drift| (record, drift_score(drift))))
        .collect::<Vec<_>>();
    drifts.sort_by(|(left_record, left_score), (right_record, right_score)| {
        right_score
            .partial_cmp(left_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left_record.id().cmp(right_record.id()))
    });
    drifts
        .into_iter()
        .take(limit)
        .map(|(record, _)| record)
        .collect()
}

/// Resolves the repo relative path, name, and span of a drift target.
#[must_use]
pub fn resolve_drift_target<'a>(
    records: &'a [GraphRecord],
    drift_id: &str,
    drift: &'a SemanticDriftMetadata,
    drift_path: Option<&'a str>,
    drift_name: Option<&'a str>,
) -> (
    Option<&'a str>,
    Option<&'a str>,
    Option<crate::ir::SourceSpan>,
) {
    let target_id = records
        .iter()
        .find_map(|r| {
            let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = r
            else {
                return None;
            };
            if source == drift_id {
                Some(target.as_str())
            } else {
                None
            }
        })
        .unwrap_or(drift.target_record_id.as_str());

    let has_temporal = records.iter().any(|r| {
        r.id() == target_id
            && matches!(
                r,
                GraphRecord::Node {
                    temporal: Some(_),
                    ..
                }
            )
    });

    if let Some(GraphRecord::Node {
        repo_relative_path,
        name,
        span,
        ..
    }) = records.iter().rfind(|r| {
        if r.id() != target_id {
            return false;
        }
        if let GraphRecord::Node {
            temporal: Some(t), ..
        } = r
        {
            t.git_commit == drift.after_git_commit
        } else {
            !has_temporal
        }
    }) {
        return (repo_relative_path.as_deref(), name.as_deref(), *span);
    }
    (drift_path, drift_name, None)
}

// ── Repository scope (issue #67) ──────────────────────────────────────────────

/// Why a repository selector failed to resolve.
///
/// Both variants carry stable machine-readable codes so callers can emit them
/// verbatim as diagnostics: `unknown_repository_selector` and
/// `ambiguous_repository_selector`. Ambiguity is never resolved implicitly.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RepositorySelectorError {
    /// No repository in the store matches the selector.
    Unknown {
        /// The selector as supplied by the caller.
        selector: String,
    },
    /// More than one repository matches the selector.
    Ambiguous {
        /// The selector as supplied by the caller.
        selector: String,
        /// Stable repository record IDs of every match, sorted ascending.
        candidates: Vec<String>,
    },
}

impl RepositorySelectorError {
    /// Returns the stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unknown { .. } => "unknown_repository_selector",
            Self::Ambiguous { .. } => "ambiguous_repository_selector",
        }
    }
}

/// One repository known to a [`RepositoryIndex`].
#[derive(Debug, Clone, Eq, PartialEq)]
struct RepositoryEntry {
    /// Human-usable display handle (e.g. `owner/name` for remote-derived
    /// identities, the basename otherwise).
    display: String,
    /// Every identity-payload-derived handle this repository answers to.
    selectors: BTreeSet<String>,
}

/// Maps code-graph records to their owning repository and resolves
/// human-usable repository selectors.
///
/// Ownership follows the deterministic containment topology emitted by the
/// scanner: `Repository` —CONTAINS→ `File` —DEFINES/CONTAINS/IMPORTS→ nested
/// modules, imports, and symbols. `SemanticDrift` nodes are attributed to the
/// repository of their `DRIFTS_FROM` target (falling back to the metadata's
/// `target_record_id`/`prior_record_id`).
///
/// Records that cannot be attributed (e.g. legacy fixtures without a
/// `Repository` node) simply have no owner; callers must keep that visible
/// rather than guessing.
#[derive(Debug, Default)]
pub struct RepositoryIndex {
    /// Node record ID → owning repository record ID.
    owner: BTreeMap<String, String>,
    /// Repository record ID → identity handles.
    repos: BTreeMap<String, RepositoryEntry>,
}

impl RepositoryIndex {
    /// Builds the index from a record slice.
    #[must_use]
    pub fn build(records: &[GraphRecord]) -> Self {
        // Tombstoned repositories (e.g. an identity change in an incremental
        // scan) are not part of the current state: they must neither resolve
        // as selectors nor make a live repository's selector ambiguous.
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

        let mut repos: BTreeMap<String, RepositoryEntry> = BTreeMap::new();
        for record in records {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                name,
                repository_identity,
                ..
            } = record
            else {
                continue;
            };
            if tombstoned.contains(id.as_str()) {
                continue;
            }
            let mut selectors: BTreeSet<String> = BTreeSet::new();
            let mut display = name.clone();
            if let Some(payload) = repository_identity.as_deref() {
                selectors.insert(payload.basename.clone());
                for handle in [
                    payload.remote_url.as_deref(),
                    payload.root_commit_sha.as_deref(),
                    payload.canonical_path.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    selectors.insert(handle.to_owned());
                }
                if display.is_none() {
                    display = Some(payload.basename.clone());
                }
            }
            if let Some(display_name) = &display {
                selectors.insert(display_name.clone());
            }
            // Remote-backed identities store the remote path (`owner/name`)
            // as both basename and display name; the human-usable final path
            // segment (`name`) must resolve as a selector too.
            let shorts: Vec<String> = selectors
                .iter()
                .filter_map(|s| s.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            selectors.extend(shorts);
            repos.entry(id.clone()).or_insert_with(|| RepositoryEntry {
                display: display.unwrap_or_else(|| id.clone()),
                selectors,
            });
        }

        // Containment adjacency over the deterministic code-graph topology.
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Edge {
                label: EdgeLabel::Contains | EdgeLabel::Defines | EdgeLabel::Imports,
                source,
                target,
                ..
            } = record
            {
                adjacency.entry(source.as_str()).or_default().push(target);
            }
        }

        let mut owner: BTreeMap<String, String> = BTreeMap::new();
        for repo_id in repos.keys() {
            let mut stack: Vec<&str> = vec![repo_id.as_str()];
            while let Some(node_id) = stack.pop() {
                if owner
                    .insert(node_id.to_owned(), repo_id.clone())
                    .is_some_and(|prev| prev == *repo_id)
                {
                    continue;
                }
                if let Some(next) = adjacency.get(node_id) {
                    stack.extend(next.iter().copied());
                }
            }
        }

        // SemanticDrift nodes hang off their target symbol, not the
        // containment topology: attribute them through DRIFTS_FROM (preferred)
        // or the drift metadata's record handles.
        let mut drift_targets: BTreeMap<&str, &str> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = record
            {
                drift_targets.entry(source.as_str()).or_insert(target);
            }
        }
        for record in records {
            let GraphRecord::Node {
                id,
                kind: NodeKind::SemanticDrift,
                semantic_drift,
                ..
            } = record
            else {
                continue;
            };
            if owner.contains_key(id.as_str()) {
                continue;
            }
            let target = drift_targets.get(id.as_str()).copied().or_else(|| {
                semantic_drift
                    .as_deref()
                    .map(|d| d.target_record_id.as_str())
            });
            let fallback = semantic_drift
                .as_deref()
                .map(|d| d.prior_record_id.as_str());
            let resolved = target
                .and_then(|t| owner.get(t))
                .or_else(|| fallback.and_then(|t| owner.get(t)))
                .cloned();
            if let Some(repo_id) = resolved {
                owner.insert(id.clone(), repo_id);
            }
        }

        Self { owner, repos }
    }

    /// Returns the owning repository record ID for a node record ID.
    #[must_use]
    pub fn owner_of(&self, record_id: &str) -> Option<&str> {
        self.owner.get(record_id).map(String::as_str)
    }

    /// Returns the human-usable display handle for a repository record ID.
    #[must_use]
    pub fn display_of(&self, repository_id: &str) -> Option<&str> {
        self.repos.get(repository_id).map(|e| e.display.as_str())
    }

    /// Returns every repository record ID known to the index, sorted ascending.
    #[must_use]
    pub fn repository_ids(&self) -> Vec<&str> {
        self.repos.keys().map(String::as_str).collect()
    }

    /// Resolves a repository selector to a stable repository record ID.
    ///
    /// Accepts the stable repository record ID directly, or any human-usable
    /// handle derived from the identity payload: the display handle (e.g.
    /// remote `owner/name`), the basename / operator override, the normalized
    /// remote URL, the root commit SHA, or the canonical path.
    ///
    /// # Errors
    ///
    /// Returns [`RepositorySelectorError::Unknown`] when nothing matches and
    /// [`RepositorySelectorError::Ambiguous`] (with every candidate listed)
    /// when more than one repository matches. Ambiguity is never resolved by
    /// picking a repository implicitly.
    pub fn resolve_selector(&self, selector: &str) -> Result<&str, RepositorySelectorError> {
        if let Some((id, _)) = self.repos.get_key_value(selector) {
            return Ok(id.as_str());
        }
        let candidates: Vec<&str> = self
            .repos
            .iter()
            .filter(|(_, entry)| entry.selectors.contains(selector))
            .map(|(id, _)| id.as_str())
            .collect();
        match candidates.as_slice() {
            [] => Err(RepositorySelectorError::Unknown {
                selector: selector.to_owned(),
            }),
            [single] => Ok(single),
            _ => Err(RepositorySelectorError::Ambiguous {
                selector: selector.to_owned(),
                candidates: candidates.into_iter().map(str::to_owned).collect(),
            }),
        }
    }
}

fn matches_symbol_at_commit(record: &GraphRecord, symbol_name: &str, commit: &str) -> bool {
    let GraphRecord::Node {
        kind,
        name,
        temporal,
        ..
    } = record
    else {
        return false;
    };
    *kind == NodeKind::Symbol
        && name.as_deref() == Some(symbol_name)
        && temporal
            .as_ref()
            .is_some_and(|temporal| temporal.git_commit.starts_with(commit))
}

// ── symbol_context ────────────────────────────────────────────────────────────

/// An evidence link target that could not be resolved in the current store.
///
/// Surfaced in [`SymbolContext::unresolved`] instead of being silently dropped.
/// Per AC5 from issue #38.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct UnresolvedRef {
    /// Record ID of the node that carries the unresolved evidence link.
    pub source_record_id: String,
    /// Handle from the evidence link (e.g. `target_record_id` or path triple).
    pub target_handle: String,
    /// Relation type from the evidence link (e.g. `"VALIDATED_BY"`).
    pub relation: String,
    /// Target domain string from the evidence link.
    pub target_domain: String,
}

/// Evidence-backed symbol context returned by [`symbol_context`].
///
/// Sections are kept separate so the caller can present source facts
/// (deterministic) differently from observations (subjective) without
/// mixing trust levels. An [`Observation`] node MUST NOT appear in
/// [`SymbolContext::source_facts`].
///
/// [`Observation`]: crate::ir::NodeKind::Observation
#[derive(Debug, Default, Clone)]
pub struct SymbolContext<'a> {
    /// The queried symbol name.
    pub symbol_name: String,
    /// Code-graph records: `Symbol` and `File` nodes that are the source of truth.
    ///
    /// Every record has a stable record ID and at least one of:
    /// `repo_relative_path`, `temporal.git_commit`, or `valid_time`.
    pub source_facts: Vec<&'a GraphRecord>,
    /// Codegraph topology edges (DEFINES, CALLS, IMPORTS, etc.) whose source
    /// and target are both in `source_facts`. Allows consumers to cite the
    /// file→symbol definition relationship without re-reading the raw graph.
    pub topology_edges: Vec<&'a GraphRecord>,
    /// Agent-authored `Observation` nodes that mention or observe the symbol.
    ///
    /// Every record carries `agent_id`, `observed_at`, and `confidence`.
    /// These are subjective and MUST NOT be treated as source truth.
    pub observations: Vec<&'a GraphRecord>,
    /// `Task` and `AcceptanceCriterion` nodes linked to the symbol.
    pub project_state: Vec<&'a GraphRecord>,
    /// `Artifact` and `PatchArtifact` nodes linked to the symbol.
    pub artifacts: Vec<&'a GraphRecord>,
    /// `Verification`, `TestRun`, `CommandRun`, and `CommandEvidence` nodes
    /// linked to the symbol.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// Evidence link targets referenced by agent-memory nodes that are absent
    /// from this store slice. Surfaced explicitly per AC5.
    pub unresolved: Vec<UnresolvedRef>,
}

impl SymbolContext<'_> {
    /// Returns `true` when no symbol with the queried name exists in the store.
    ///
    /// Callers MUST check this before inspecting sections — all sections are
    /// empty for a no-match result.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.project_state.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.unresolved.is_empty()
    }
}

/// Classifies a [`NodeKind`] into one of the five context sections.
///
/// Returns `None` for kinds that do not belong to any section (e.g. edges,
/// tombstones, infrastructure nodes like `Repository`, `Agent`, `AgentSession`).
const fn classify_node(kind: NodeKind) -> Option<ContextSection> {
    match kind {
        // code-graph source facts
        NodeKind::Symbol | NodeKind::File | NodeKind::Module | NodeKind::Import => {
            Some(ContextSection::SourceFact)
        }
        // agent-authored observations and failure records
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
            Some(ContextSection::Observation)
        }
        // project / task domain
        NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::LocalTask
        | NodeKind::GitHubIssue
        | NodeKind::PR => Some(ContextSection::ProjectState),
        // artifact domain
        NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
            Some(ContextSection::Artifact)
        }
        // verification domain
        NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::TestRun
        | NodeKind::CommandRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult => Some(ContextSection::VerificationEvidence),
        // everything else (infrastructure, semantic, user-context, etc.) is excluded
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum ContextSection {
    SourceFact,
    Observation,
    ProjectState,
    Artifact,
    VerificationEvidence,
}

/// Returns all known context for a named symbol, separated by trust domain.
///
/// # Algorithm
///
/// 1. Collect all `Symbol` node IDs for `symbol_name`.
/// 2. Add those symbol nodes to `source_facts`.
/// 3. Scan every other record:
///    a. Edges: if source or target is a known symbol ID, follow the other
///    end and classify the referenced node.
///    b. Nodes with `evidence_links`: for each link whose `target_record_id`
///    is a known symbol ID, classify the linking node.
/// 4. Collect any evidence link targets that are not present in the store
///    slice into `unresolved`.
///
/// Output ordering within each section is sorted by record ID for determinism
/// (AC7).
///
/// An empty [`SymbolContext`] where [`SymbolContext::is_no_match`] returns
/// `true` is returned when the symbol is not found. The caller MUST use
/// `is_no_match()` — there is no panic or error path.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn symbol_context<'a>(records: &'a [GraphRecord], symbol_name: &str) -> SymbolContext<'a> {
    // Step 0: collect tombstoned IDs so deleted symbols yield no-match, not
    // stale context. This mirrors the current-state filter used by the other
    // query paths (query symbol, query file).
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    // Step 1: collect symbol record IDs, excluding tombstoned CURRENT-STATE records.
    //
    // Temporal records (from scan-history, carrying `temporal` metadata) are
    // historical snapshots — they must NOT be suppressed by a tombstone that
    // reflects deletion only in the current state.
    let symbol_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name,
                temporal,
                ..
            } = r
            else {
                return None;
            };
            let is_historical = temporal.is_some();
            if name.as_deref() == Some(symbol_name)
                && (is_historical || !tombstoned_ids.contains(id.as_str()))
            {
                Some(id.as_str())
            } else {
                None
            }
        })
        .collect();

    if symbol_ids.is_empty() {
        return SymbolContext {
            symbol_name: symbol_name.to_owned(),
            ..Default::default()
        };
    }

    // Build a lookup map: record_id → record for fast classification checks.
    // For records with the same stable ID (temporal versions), last-write-wins
    // is acceptable here because we only use by_id for kind inspection.
    // Actual resolution of output records uses records.iter() to capture all
    // versions (see the `resolve` closure below).
    let by_id: std::collections::BTreeMap<&str, &GraphRecord> =
        records.iter().map(|r| (r.id(), r)).collect();

    // IDs that have at least one temporal version in the slice.  Used to
    // exempt historical records from current-state tombstone suppression.
    let has_any_temporal_version: BTreeSet<&str> = records
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

    // Step 2: the symbol nodes themselves are source_facts.
    let mut source_facts: BTreeSet<&str> = symbol_ids.clone();

    // Add co-located File nodes via DEFINES topology edges (primary mechanism).
    // DEFINES edges are created within a single repository scan, so they
    // unambiguously identify the correct file even in multi-repo stores.
    // Guard: only seed a file if its node is present AND not tombstoned.
    // Absent or tombstoned file sources from stale DEFINES edges would pollute
    // seed_ids and could pull in unrelated cross-domain context.
    // Track which symbol IDs were resolved via DEFINES (per-symbol, not a global flag).
    // A global flag would suppress the path fallback for ALL matched symbols when even
    // one has a DEFINES edge, silently omitting files for same-named symbols that lack
    // a DEFINES edge in a partial slice.
    let mut symbols_resolved_by_defines: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label: EdgeLabel::Defines,
            source,
            target,
            temporal: edge_temporal,
            ..
        } = record
            && symbol_ids.contains(target.as_str())
            // Guard: skip tombstoned DEFINES edges — but only for non-temporal (current-state)
            // records. Temporal (historical) edges carry scan-history provenance and must
            // not be suppressed by a tombstone reflecting only the current state.
            && (edge_temporal.is_some() || !tombstoned_ids.contains(edge_id.as_str()))
            // Same temporal guard for the file source: a historical file node must not be
            // excluded by a current-state tombstone on its stable ID.
            && (has_any_temporal_version.contains(source.as_str())
                || !tombstoned_ids.contains(source.as_str()))
            && by_id.get(source.as_str()).is_some_and(|r| {
                matches!(
                    *r,
                    GraphRecord::Node {
                        kind: NodeKind::File,
                        ..
                    }
                )
            })
        {
            source_facts.insert(source.as_str());
            symbols_resolved_by_defines.insert(target.as_str());
        }
    }

    // Path-based fallback: only used per-symbol when no DEFINES edge resolved that
    // symbol's file. Path-matching may produce false positives in multi-repo stores
    // (different repos sharing identical relative paths), so it is skipped per-symbol
    // whenever a DEFINES edge already identified the correct file.
    // Tombstoned file nodes are excluded: a deleted file record must not seed
    // source_facts or its tombstoned ID would pollute seed_ids and could draw in
    // deleted file context via TOUCHED_FILE/TOUCHES_FILE edges.
    for record in records {
        let GraphRecord::Node {
            id: sym_id,
            kind: NodeKind::Symbol,
            name,
            repo_relative_path: Some(path),
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        if !symbol_ids.contains(sym_id.as_str()) {
            continue;
        }
        if symbols_resolved_by_defines.contains(sym_id.as_str()) {
            continue;
        }
        for candidate in records {
            let GraphRecord::Node {
                id: file_id,
                kind: NodeKind::File,
                repo_relative_path: Some(file_path),
                ..
            } = candidate
            else {
                continue;
            };
            if file_path == path
                && (has_any_temporal_version.contains(file_id.as_str())
                    || !tombstoned_ids.contains(file_id.as_str()))
            {
                source_facts.insert(file_id.as_str());
            }
        }
    }

    // Snapshot seed IDs (symbol IDs + co-located file IDs) before the main
    // loop. Used to detect links that target the symbol's context, including
    // file-scoped relations such as CommandRun --TOUCHED_FILE--> File.
    let seed_ids: BTreeSet<&str> = source_facts.iter().copied().collect();

    // Paths from seed nodes (Symbol + File) for gating triple-form evidence links.
    // Only triple-only citations whose target_repo_relative_path matches a seed path
    // are surfaced in `unresolved`; unrelated citations to other files must not
    // pollute the context for the queried symbol.
    let seed_paths: BTreeSet<&str> = seed_ids
        .iter()
        .filter_map(|id| {
            by_id.get(id).and_then(|r| match r {
                GraphRecord::Node {
                    repo_relative_path: Some(p),
                    ..
                } => Some(p.as_str()),
                _ => None,
            })
        })
        .collect();

    // Step 2b: collect topology edges where BOTH endpoints are in seed_ids.
    // These provide citable provenance for the file→symbol definition
    // relationship (and other structural topology) without BFS traversal.
    let mut topology_edge_ids: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
            && label.is_codegraph_topology_label()
            // Temporal guard: historical topology edges must not be suppressed by a
            // current-state tombstone. Only non-temporal edges are excluded by tombstones.
            && (has_any_temporal_version.contains(id.as_str())
                || !tombstoned_ids.contains(id.as_str()))
            && seed_ids.contains(source.as_str())
            && seed_ids.contains(target.as_str())
        {
            topology_edge_ids.insert(id.as_str());
        }
    }

    // Step 3a + 3b: classify linked records.
    let mut observations: BTreeSet<&str> = BTreeSet::new();
    let mut project_state: BTreeSet<&str> = BTreeSet::new();
    let mut artifacts: BTreeSet<&str> = BTreeSet::new();
    let mut verification_evidence: BTreeSet<&str> = BTreeSet::new();

    // Collect IDs of all records present in the slice for unresolved detection.
    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();

    // Helper: insert a record ID into the appropriate section.
    //
    // Returns `true` when the node was actually inserted into a section.
    // Returns `false` for: tombstoned IDs, missing IDs (by_id miss), non-Node
    // records, non-seed SourceFact candidates (sibling symbols/files), and
    // NodeKind variants with no section mapping.
    //
    // Callers MUST gate `next_frontier.push(id)` on this return value — only
    // classified nodes should expand the BFS; pushing skipped IDs would allow
    // missing ghost endpoints and sibling codegraph nodes to traverse further.
    let classify_and_insert = |record_id: &'a str,
                               source_facts: &mut BTreeSet<&'a str>,
                               observations: &mut BTreeSet<&'a str>,
                               project_state: &mut BTreeSet<&'a str>,
                               artifacts: &mut BTreeSet<&'a str>,
                               verification_evidence: &mut BTreeSet<&'a str>|
     -> bool {
        // Suppress tombstoned records, but allow temporal (historical) records
        // through even when their stable ID is tombstoned: the tombstone reflects
        // current-state deletion and must not erase historical context.
        if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
            return false;
        }
        let Some(rec) = by_id.get(record_id) else {
            return false;
        };
        let GraphRecord::Node { kind, .. } = rec else {
            return false;
        };
        match classify_node(*kind) {
            Some(ContextSection::SourceFact) if seed_ids.contains(record_id) => {
                source_facts.insert(record_id);
                true
            }
            Some(ContextSection::Observation) => {
                observations.insert(record_id);
                true
            }
            Some(ContextSection::ProjectState) => {
                project_state.insert(record_id);
                true
            }
            Some(ContextSection::Artifact) => {
                artifacts.insert(record_id);
                true
            }
            Some(ContextSection::VerificationEvidence) => {
                verification_evidence.insert(record_id);
                true
            }
            Some(ContextSection::SourceFact) | None => false,
        }
    };

    // Step 3: bounded BFS traversal — 3 hops beyond seeds.
    //
    // Hop 1 discovers nodes directly linked to the symbol/file seeds.
    // Hop 2 discovers nodes linked to hop-1 results, e.g. AcceptanceCriterion
    // nodes owned by a Task that was found in hop 1 via OWNED_BY_TASK edges.
    // Hop 3 discovers nodes linked to hop-2 results, e.g. a Verification that
    // closes an AC discovered in hop 2 via CLOSES_ACCEPTANCE_CRITERION.
    //
    // `visited` prevents re-classifying the same node in a later hop.
    // `frontier` is the set of IDs whose outgoing/incoming edges are scanned
    // in the current hop.
    let mut visited: BTreeSet<&str> = seed_ids.clone();
    let mut frontier: BTreeSet<&str> = seed_ids.clone();
    // Per-version scan tracking for temporal nodes: keyed by "id@git_commit" so
    // each temporal version of a node has its evidence_links scanned independently.
    let mut temporal_evidence_scanned: BTreeSet<String> = BTreeSet::new();

    for _hop in 0..3_usize {
        let mut next_frontier: Vec<&'a str> = Vec::new();

        for record in records {
            match record {
                GraphRecord::Edge {
                    id: edge_id,
                    label,
                    source,
                    target,
                    ..
                } => {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    // Skip tombstoned edges — a retracted relationship must not
                    // carry the BFS to its formerly-linked node.
                    // Temporal guard: a historical edge is exempt from current-state
                    // tombstone suppression. The tombstone reflects a current deletion;
                    // a temporal edge carries valid historical provenance.
                    if tombstoned_ids.contains(edge_id.as_str())
                        && !has_any_temporal_version.contains(edge_id.as_str())
                    {
                        continue;
                    }
                    let candidate = if frontier.contains(source.as_str()) {
                        Some(target.as_str())
                    } else if frontier.contains(target.as_str()) && !is_forward_only_label(*label) {
                        Some(source.as_str())
                    } else {
                        None
                    };
                    if let Some(id) = candidate
                        && visited.insert(id)
                    {
                        let was_classified = classify_and_insert(
                            id,
                            &mut source_facts,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        // Expand nodes that were classified OR that are valid relay
                        // kinds (e.g. ToolCall) which bridge classifiable sections but
                        // have no output section of their own. Tombstoned and missing
                        // (by_id miss) nodes still must not enter the frontier.
                        if was_classified
                            || is_bfs_relay_node(
                                id,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(id);
                        }
                    }
                }
                GraphRecord::Node {
                    id: node_id,
                    evidence_links: Some(links),
                    temporal,
                    ..
                } => {
                    // For temporal nodes, track per-version by "id@git_commit" so
                    // each historical version has its evidence_links scanned independently.
                    // For current-state nodes, fall back to the visited set.
                    let already_scanned = temporal.as_ref().map_or_else(
                        || visited.contains(node_id.as_str()),
                        |t| {
                            let key = format!("{}@{}", node_id, t.git_commit);
                            !temporal_evidence_scanned.insert(key)
                        },
                    );
                    if already_scanned {
                        continue;
                    }
                    // Skip tombstoned non-temporal nodes before scanning evidence_links.
                    if tombstoned_ids.contains(node_id.as_str())
                        && !has_any_temporal_version.contains(node_id.as_str())
                    {
                        visited.insert(node_id.as_str());
                        continue;
                    }
                    // Classify if any evidence link directly targets a symbol/file seed.
                    // Using seed_ids (not frontier) prevents shared verification sinks
                    // that entered the frontier via edge traversal from causing sibling
                    // observations to be pulled in via their evidence_links. The edge arm
                    // handles multi-hop traversal; this arm is for direct symbol/file
                    // citations.
                    let links_to_frontier = links.iter().any(|link| {
                        link.target_record_id
                            .as_deref()
                            .is_some_and(|tid| seed_ids.contains(tid))
                    });
                    if links_to_frontier {
                        let was_classified = classify_and_insert(
                            node_id.as_str(),
                            &mut source_facts,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        visited.insert(node_id.as_str());
                        if was_classified
                            || is_bfs_relay_node(
                                node_id.as_str(),
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(node_id.as_str());
                        }

                        // Scan backing evidence_links: present unvisited targets are
                        // backing evidence; missing targets go to unresolved (AC5).
                        // Triple-based links (no target_record_id) are also surfaced
                        // as unresolved so consumers can diagnose absent targets.
                        for link in links {
                            if let Some(target_id) = &link.target_record_id {
                                if present_ids.contains(target_id.as_str())
                                    && !visited.contains(target_id.as_str())
                                {
                                    let target_classified = classify_and_insert(
                                        target_id.as_str(),
                                        &mut source_facts,
                                        &mut observations,
                                        &mut project_state,
                                        &mut artifacts,
                                        &mut verification_evidence,
                                    );
                                    visited.insert(target_id.as_str());
                                    if target_classified {
                                        next_frontier.push(target_id.as_str());
                                    }
                                } else if !present_ids.contains(target_id.as_str()) {
                                    unresolved.push(UnresolvedRef {
                                        source_record_id: node_id.clone(),
                                        target_handle: target_id.clone(),
                                        relation: link.relation.clone(),
                                        target_domain: link.target_domain.clone(),
                                    });
                                }
                            } else if let Some(handle) = evidence_link_triple_handle(link) {
                                unresolved.push(UnresolvedRef {
                                    source_record_id: node_id.clone(),
                                    target_handle: handle,
                                    relation: link.relation.clone(),
                                    target_domain: link.target_domain.clone(),
                                });
                            }
                        }
                    } else {
                        // Node has no resolved evidence link to seed_ids but may have
                        // triple-form links (target_repo_relative_path/span/commit with
                        // no target_record_id). Pre-resolution graph slices produced
                        // before daemon resolution can contain only these triples.
                        // Surface them in `unresolved` so consumers can diagnose the
                        // citation without requiring a raw graph reload.
                        let has_triple = links.iter().any(|link| {
                            link.target_record_id.is_none()
                                && evidence_link_triple_handle(link).is_some()
                        });
                        if has_triple {
                            // Do NOT mark visited here — a node that only has
                            // triple-form links may also be reachable via a graph
                            // edge, and marking it visited would prevent the edge arm
                            // from classifying it on a subsequent hop.
                            for link in links {
                                if link.target_record_id.is_none() {
                                    let Some(handle) = evidence_link_triple_handle(link) else {
                                        continue;
                                    };
                                    // Only surface triples that target one of the seed file
                                    // paths; unrelated citations to other files must not
                                    // pollute the context for the queried symbol.
                                    if !link
                                        .target_repo_relative_path
                                        .as_deref()
                                        .is_some_and(|p| seed_paths.contains(p))
                                    {
                                        continue;
                                    }
                                    unresolved.push(UnresolvedRef {
                                        source_record_id: node_id.clone(),
                                        target_handle: handle,
                                        relation: link.relation.clone(),
                                        target_domain: link.target_domain.clone(),
                                    });
                                }
                            }
                        }
                    }
                }
                GraphRecord::Node { .. } | GraphRecord::Tombstone { .. } => {}
            }
        }

        if next_frontier.is_empty() {
            break; // convergence: no new nodes in this hop
        }
        frontier = next_frontier.into_iter().collect();
    }

    // Post-processing: scan evidence_links of nodes classified via the edge arm.
    // Those nodes are in the response but their evidence_links were never scanned
    // (the evidence_links arm only runs when a node's OWN links target the symbol).
    // This covers cases like Obs --edge--> Symbol where Obs also has a VALIDATED_BY
    // link to a Verification that needs to appear in verification_evidence.
    //
    // Iterative until convergence: a newly classified node (e.g. ObsB found via
    // ObsA's evidence_links) may itself have evidence_links (e.g. VALIDATED_BY →
    // CommandRun) that need scanning in a subsequent pass. Loop until no new nodes
    // are classified.
    //
    // `backfill_scanned` tracks which IDs have already been scanned so that the
    // convergence loop does not re-process nodes from earlier passes.
    let mut backfill_scanned: BTreeSet<String> =
        symbol_ids.iter().map(ToString::to_string).collect();

    loop {
        // Collect IDs that have been classified but not yet scanned in backfill.
        // Owned Strings release the borrows on the section BTreeSets so that
        // classify_and_insert can take mutable references below.
        let to_scan: Vec<String> = source_facts
            .iter()
            .chain(observations.iter())
            .chain(project_state.iter())
            .chain(artifacts.iter())
            .chain(verification_evidence.iter())
            .filter(|id| !backfill_scanned.contains(**id))
            .map(|id| (*id).to_owned())
            .collect();

        if to_scan.is_empty() {
            break;
        }

        for node_id in &to_scan {
            backfill_scanned.insert(node_id.clone());
        }

        for node_id in &to_scan {
            // .copied() converts Option<&&'a GraphRecord> → Option<&'a GraphRecord>
            // so that sub-borrows (nid, links) carry lifetime 'a and satisfy
            // the classify_and_insert closure's &'a str constraint.
            let Some(GraphRecord::Node {
                id: nid,
                evidence_links: Some(links),
                ..
            }) = by_id.get(node_id.as_str()).copied()
            else {
                continue;
            };
            for link in links {
                if let Some(target_id) = &link.target_record_id {
                    if present_ids.contains(target_id.as_str())
                        && !symbol_ids.contains(target_id.as_str())
                    {
                        classify_and_insert(
                            target_id.as_str(),
                            &mut source_facts,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                    } else if !present_ids.contains(target_id.as_str()) {
                        unresolved.push(UnresolvedRef {
                            source_record_id: nid.clone(),
                            target_handle: target_id.clone(),
                            relation: link.relation.clone(),
                            target_domain: link.target_domain.clone(),
                        });
                    }
                } else if let Some(handle) = evidence_link_triple_handle(link) {
                    unresolved.push(UnresolvedRef {
                        source_record_id: nid.clone(),
                        target_handle: handle,
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                }
            }
        }
    }

    // Frontier expansion for backfill discoveries: nodes newly classified by the
    // backfill (e.g. a Task found via an Observation's evidence_links) were never
    // in the BFS frontier, so their outgoing cross-domain edges were never scanned.
    // Loop until convergence so that multi-hop chains discovered through backfill
    // are fully traversed — e.g. backfill → Task → AC → CommandRun all appear.
    let mut extra_frontier: BTreeSet<&str> = source_facts
        .iter()
        .chain(observations.iter())
        .chain(project_state.iter())
        .chain(artifacts.iter())
        .chain(verification_evidence.iter())
        .copied()
        .filter(|id| !visited.contains(*id))
        .collect();

    while !extra_frontier.is_empty() {
        let mut next_extra: Vec<&'a str> = Vec::new();
        for record in records {
            if let GraphRecord::Edge {
                id: edge_id,
                label,
                source,
                target,
                ..
            } = record
            {
                if !is_cross_domain_label(*label) {
                    continue;
                }
                // Same temporal guard as the main BFS edge arm.
                if tombstoned_ids.contains(edge_id.as_str())
                    && !has_any_temporal_version.contains(edge_id.as_str())
                {
                    continue;
                }
                let candidate = if extra_frontier.contains(source.as_str()) {
                    Some(target.as_str())
                } else if extra_frontier.contains(target.as_str()) && !is_forward_only_label(*label)
                {
                    Some(source.as_str())
                } else {
                    None
                };
                if let Some(id) = candidate
                    && visited.insert(id)
                {
                    let was_classified = classify_and_insert(
                        id,
                        &mut source_facts,
                        &mut observations,
                        &mut project_state,
                        &mut artifacts,
                        &mut verification_evidence,
                    );
                    if was_classified
                        || is_bfs_relay_node(id, &by_id, &tombstoned_ids, &has_any_temporal_version)
                    {
                        next_extra.push(id);
                    }
                }
            }
        }
        extra_frontier = next_extra.into_iter().collect();
    }

    // Remove symbol records from non-source-fact sections to avoid overlap
    // (a Symbol node classified via edge could end up in the wrong section).
    for sid in &symbol_ids {
        observations.remove(sid);
        project_state.remove(sid);
        artifacts.remove(sid);
        verification_evidence.remove(sid);
    }

    // Resolve ID sets → sorted record slices.
    //
    // Using records.iter() (not by_id) captures ALL records matching each ID,
    // including multiple temporal versions of the same symbol that share a
    // stable ID. by_id last-write-wins would silently drop all but one version.
    let resolve = |ids: &BTreeSet<&str>| -> Vec<&'a GraphRecord> {
        let mut out: Vec<&'a GraphRecord> = records
            .iter()
            .filter(|r| {
                ids.contains(r.id())
                    // Exclude non-temporal records whose IDs are tombstoned.
                    // Temporal (historical) versions of the same ID must be kept.
                    && match r {
                        GraphRecord::Node {
                            temporal: Some(_), ..
                        }
                        | GraphRecord::Edge {
                            temporal: Some(_), ..
                        } => true,
                        _ => !tombstoned_ids.contains(r.id()),
                    }
            })
            .collect();
        out.sort_by(|a, b| {
            a.id().cmp(b.id()).then_with(|| {
                let a_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = a
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                let b_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = b
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                a_commit.cmp(b_commit)
            })
        });
        out
    };

    SymbolContext {
        symbol_name: symbol_name.to_owned(),
        source_facts: resolve(&source_facts),
        topology_edges: {
            let mut out: Vec<&'a GraphRecord> = records
                .iter()
                .filter(|r| {
                    topology_edge_ids.contains(r.id())
                        // Exclude non-temporal records whose ID is tombstoned.
                        // Temporal (historical) versions of the same stable ID must be kept.
                        && match r {
                            GraphRecord::Edge {
                                temporal: Some(_), ..
                            } => true,
                            _ => !tombstoned_ids.contains(r.id()),
                        }
                })
                .collect();
            out.sort_by_key(|r| r.id());
            out
        },
        observations: resolve(&observations),
        project_state: resolve(&project_state),
        artifacts: resolve(&artifacts),
        verification_evidence: resolve(&verification_evidence),
        unresolved: {
            let mut u = unresolved;
            u.sort_by(|a, b| {
                a.source_record_id
                    .cmp(&b.source_record_id)
                    .then_with(|| a.target_handle.cmp(&b.target_handle))
                    .then_with(|| a.relation.cmp(&b.relation))
                    .then_with(|| a.target_domain.cmp(&b.target_domain))
            });
            u.dedup_by(|a, b| {
                a.source_record_id == b.source_record_id
                    && a.target_handle == b.target_handle
                    && a.relation == b.relation
                    && a.target_domain == b.target_domain
            });
            u
        },
    }
}

/// Returns `true` for edge labels that cross domain boundaries and therefore
/// signal a meaningful link to a symbol for the context query.
///
/// This is a superset of the previous list: `RelatesTo`, `Contradicts`, and
/// `Supersedes` are now included because they are permitted cross-domain
/// evidence-link labels and can legally appear on graph edges between an
/// agent-memory/project node and a code-graph symbol.
const fn is_cross_domain_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::Observes
            | EdgeLabel::MentionsSymbol
            | EdgeLabel::ValidatedBy
            | EdgeLabel::HasEvidence
            | EdgeLabel::ReferencesTask
            | EdgeLabel::FailedOn
            | EdgeLabel::ExplainsChange
            | EdgeLabel::TouchedFile
            | EdgeLabel::TouchesFile
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ClosesAcceptanceCriterion
            | EdgeLabel::RelatesTo
            | EdgeLabel::Contradicts
            | EdgeLabel::Supersedes
            | EdgeLabel::OwnedByTask
    )
}

/// Returns `true` for edge labels that should only be traversed in the
/// forward direction (source → target) during BFS.
///
/// These labels all point FROM agent-memory/project nodes TOWARD shared sinks
/// (verification runs, artifacts, tasks). Traversing backward from the sink
/// would pull in unrelated sibling nodes that happen to reference the same
/// sink but have no connection to the queried symbol. For example, if two
/// observations are both validated by the same `CommandRun`, following
/// `VALIDATED_BY` backward from the run would classify the unrelated
/// observation as context.
///
/// `ClosesAcceptanceCriterion` has schema direction AC → Verification. Making
/// it forward-only prevents backward traversal from a Verification sink to
/// unrelated `AcceptanceCriteria` that happen to share the same run.
///
/// `ExplainsChange` is intentionally NOT forward-only: its schema direction
/// is Observation → Symbol/File (the same as `MentionsSymbol`). Backward
/// traversal from the Symbol/File seed is required to discover the explaining
/// Observation.
const fn is_forward_only_label(label: EdgeLabel) -> bool {
    matches!(
        label,
        EdgeLabel::ValidatedBy
            | EdgeLabel::HasEvidence
            | EdgeLabel::ProducedEvidence
            | EdgeLabel::ProducedPatch
            | EdgeLabel::ReferencesTask
            | EdgeLabel::ClosesAcceptanceCriterion
    )
}

/// Returns `true` when `record_id` identifies a node that should expand the BFS
/// frontier even though it has no output context section.
///
/// "Relay" nodes are infrastructure connectors that bridge classifiable sections:
/// - [`NodeKind::ToolCall`]: `TOUCHED_FILE → File` backward traversal discovers the
///   `ToolCall`; its `PRODUCED_EVIDENCE` forward edges then reach `CommandRun`/`TestRun`.
///
/// Relay expansion is only allowed for nodes that are present in `by_id` and
/// not tombstoned (current-state). Temporal relay nodes with the same stable ID
/// as a current-state tombstone are exempt — the tombstone reflects only the
/// current state; the historical relay must still bridge its edges.
fn is_bfs_relay_node(
    record_id: &str,
    by_id: &std::collections::BTreeMap<&str, &GraphRecord>,
    tombstoned_ids: &BTreeSet<&str>,
    has_any_temporal_version: &BTreeSet<&str>,
) -> bool {
    if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
        return false;
    }
    let Some(rec) = by_id.get(record_id) else {
        return false;
    };
    matches!(
        rec,
        GraphRecord::Node {
            kind: NodeKind::ToolCall | NodeKind::AgentTurn | NodeKind::AgentRun,
            ..
        }
    )
}

/// Constructs an unresolved-ref handle string from the triple fields of an
/// `EvidenceLink` that has no `target_record_id`.
///
/// Returns `None` when none of the triple fields are present (link is unusable).
/// Format: `{path}:{start}..{end}@{commit}` when all fields present; subsets
/// when only some are available.
fn evidence_link_triple_handle(link: &EvidenceLink) -> Option<String> {
    let path = link.target_repo_relative_path.as_deref()?;
    Some(match (&link.target_span, &link.target_git_commit) {
        (Some(span), Some(commit)) => {
            format!("{path}:{}..{}@{commit}", span.start_line, span.end_line)
        }
        (Some(span), None) => format!("{path}:{}..{}", span.start_line, span.end_line),
        (None, Some(commit)) => format!("{path}@{commit}"),
        (None, None) => path.to_owned(),
    })
}

fn semantic_drift(record: &GraphRecord) -> Option<&SemanticDriftMetadata> {
    let GraphRecord::Node {
        kind,
        semantic_drift,
        ..
    } = record
    else {
        return None;
    };
    if *kind == NodeKind::SemanticDrift {
        semantic_drift.as_deref()
    } else {
        None
    }
}

const fn drift_score(drift: &SemanticDriftMetadata) -> f64 {
    drift.score
}

/// Finds a symbol record by name at the most recent commit at or before `as_of`.
///
/// `as_of` must be an RFC 3339 timestamp string. Returns an error string if the
/// timestamp cannot be parsed. Returns `None` when no record exists at or before
/// the given instant.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
) -> Result<Option<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut best: Option<(&GraphRecord, DateTime<chrono::FixedOffset>)> = None;

    for record in records {
        let GraphRecord::Node {
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
        // Resolve valid_time from history temporal block (history records) or
        // node-level field (current-tree records stamped by with_valid_time_inferred).
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
        let is_better = best.as_ref().is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best = Some((record, vt));
        }
    }

    Ok(best.map(|(r, _)| r))
}

/// Repository-aware variant of [`symbol_as_of_valid_time`] (issue #67).
///
/// Returns the best record (most recent `valid_time` at or before `as_of`,
/// ties broken by ascending record ID) **per owning repository**, sorted by
/// record ID. When `repo` is supplied only records owned by that repository
/// are considered.
///
/// A multi-repository collision therefore yields one row per repository so
/// the caller can either surface all of them or fail with an
/// ambiguous-repository diagnostic — never picking a repository implicitly.
/// Records the index cannot attribute to any repository share one unattributed
/// group, preserving single-repository and legacy-fixture behavior.
///
/// # Errors
///
/// Returns an error string when `as_of` is not a valid RFC 3339 timestamp.
pub fn symbol_as_of_valid_time_by_repo<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<Vec<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    let mut best: BTreeMap<Option<&str>, (&GraphRecord, DateTime<chrono::FixedOffset>)> =
        BTreeMap::new();

    for record in records {
        let GraphRecord::Node {
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
        let owner = index.owner_of(record.id());
        if let Some(repo_id) = repo
            && owner != Some(repo_id)
        {
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
        let is_better = best.get(&owner).is_none_or(|(prev_r, prev_vt)| {
            vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
        });
        if is_better {
            best.insert(owner, (record, vt));
        }
    }

    let mut results: Vec<&GraphRecord> = best.into_values().map(|(r, _)| r).collect();
    results.sort_by(|left, right| left.id().cmp(right.id()));
    Ok(results)
}

// ── Transaction-time queries (Issue #66) ───────────────────────────────────────

/// A machine-readable diagnostic emitted by a transaction-time query.
///
/// Diagnostics never silently change the result set; they explain edge cases
/// (empty views, excluded rows, out-of-range instants) so callers can tell a
/// real "prior view" apart from a missing-metadata or out-of-range condition.
/// See issue #66 AC6.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TxDiagnostic {
    /// Stable machine-readable code (e.g. `before_first_transaction`).
    pub code: String,
    /// Human-readable explanation. Never contains raw record bodies.
    pub message: String,
}

/// Error that aborts a transaction-time query before any rows are produced.
///
/// Distinct from [`TxDiagnostic`]: an error means the query itself was
/// malformed (e.g. an unparseable timestamp), so no result set exists.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TxQueryError {
    /// Stable machine-readable code (e.g. `invalid_timestamp`).
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Outcome of a transaction-time symbol query.
#[derive(Debug, Default)]
pub struct TxSymbolQuery<'r> {
    /// Selected records: one per stable record ID, the latest version the store
    /// knew at the requested transaction time (further constrained by valid time
    /// when a valid-time instant is also supplied). Sorted by record ID for
    /// deterministic output (AC7).
    pub records: Vec<&'r GraphRecord>,
    /// Machine-readable diagnostics for this query (AC6). Sorted and deduplicated.
    pub diagnostics: Vec<TxDiagnostic>,
}

/// Best-candidate tuple tracked per stable record ID while resolving a
/// transaction-time query: the record, its parsed transaction time, and its
/// parsed valid time (present only when a valid-time axis is requested).
type TxCandidate<'r> = (
    &'r GraphRecord,
    DateTime<chrono::FixedOffset>,
    Option<DateTime<chrono::FixedOffset>>,
    // Deterministic tie-break rank for equal transaction (and valid) times:
    // commit topological rank for history records, input order otherwise.
    usize,
);

/// Resolves the transaction-time handle of a record from its body fields.
///
/// Priority order:
/// 1. explicit `transaction_time` (project-domain mutations, seeded fixtures),
/// 2. `ingested_at` (agent-memory / verification commit time),
/// 3. `valid_time` when `valid_time_source == "inferred_from_transaction_time"`
///    (current-tree scans set `valid_time` to the scan's wall-clock instant,
///    which *is* the transaction time),
/// 4. `temporal.observed_at` for history-replay records (`scan-history`), whose
///    only available store-observation timeline is the commit timeline. For
///    replayed history the transaction axis collapses onto that timeline, which
///    is the honest "known by Egregore then" handle for deterministic replay.
///
/// Returns `None` when no transaction-time stamp can be resolved. Callers MUST
/// treat `None` as *missing metadata*, never as a current-state record — a
/// transaction-time query must not silently fall back to current state (AC6).
#[must_use]
pub fn record_transaction_time(record: &GraphRecord) -> Option<&str> {
    let GraphRecord::Node {
        transaction_time,
        ingested_at,
        valid_time,
        valid_time_source,
        temporal,
        ..
    } = record
    else {
        return None;
    };
    if let Some(tt) = transaction_time.as_deref() {
        return Some(tt);
    }
    if let Some(ia) = ingested_at.as_deref() {
        return Some(ia);
    }
    if valid_time_source.as_deref() == Some("inferred_from_transaction_time") {
        return valid_time.as_deref();
    }
    if let Some(t) = temporal {
        return Some(t.observed_at.as_str());
    }
    None
}

/// Resolves the valid-time of a node from the history temporal block or the
/// current-tree node-level field.
#[must_use]
fn node_valid_time(record: &GraphRecord) -> Option<&str> {
    let GraphRecord::Node {
        temporal,
        valid_time,
        ..
    } = record
    else {
        return None;
    };
    temporal
        .as_ref()
        .map(|t| t.valid_time.as_str())
        .or(valid_time.as_deref())
}

/// Returns the Git commit SHA of a history-replay (temporal) record.
#[must_use]
const fn node_git_commit(record: &GraphRecord) -> Option<&str> {
    if let GraphRecord::Node {
        temporal: Some(t), ..
    } = record
    {
        Some(t.git_commit.as_str())
    } else {
        None
    }
}

/// Deterministic ordering of history commits derived from the Git commit DAG
/// carried on temporal records (`git_commit` + `git_parent_commits`).
///
/// Git commit timestamps are only second-resolution and batch-created commits
/// frequently collide, so a timestamp comparison alone cannot order two commits
/// made in the same second. This reconstructs the parent topology and assigns
/// each commit a `rank` (its longest ancestor-chain length), so a descendant
/// always outranks its ancestors regardless of identical timestamps.
///
/// It also inverts the parent links into a child adjacency map so removal
/// detection can ask whether a commit has a *strict descendant* in the requested
/// view (see [`Self::strict_descendants`]). Child-ward reachability — rather than
/// connected-component membership — is what distinguishes two forks that share
/// Git ancestry: a later commit on one fork is not a descendant of a live symbol
/// on the other, so it never makes that symbol look removed.
#[derive(Default)]
struct CommitOrder {
    /// Commit SHA → longest ancestor-chain length (topological rank).
    rank: BTreeMap<String, usize>,
    /// Commit SHA → its direct child commits (parent links inverted).
    children: BTreeMap<String, Vec<String>>,
}

impl CommitOrder {
    fn build(records: &[GraphRecord]) -> Self {
        // Longest chain over commits that are themselves present in the store
        // (absent shallow-boundary parents anchor at 0).
        fn rank_of(
            sha: &str,
            parents: &BTreeMap<String, Vec<String>>,
            memo: &mut BTreeMap<String, usize>,
            stack: &mut BTreeSet<String>,
        ) -> usize {
            if let Some(&r) = memo.get(sha) {
                return r;
            }
            if !stack.insert(sha.to_owned()) {
                return 0; // cycle guard (not expected in a Git history)
            }
            let mut best = 0;
            if let Some(ps) = parents.get(sha) {
                for p in ps {
                    if parents.contains_key(p) {
                        best = best.max(rank_of(p, parents, memo, stack) + 1);
                    }
                }
            }
            stack.remove(sha);
            memo.insert(sha.to_owned(), best);
            best
        }

        // commit → deduplicated parent SHAs, for every commit observed as a
        // temporal record's `git_commit`.
        let mut parents: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                temporal: Some(t), ..
            } = record
            {
                let entry = parents.entry(t.git_commit.clone()).or_default();
                for parent in &t.git_parent_commits {
                    if !entry.contains(parent) {
                        entry.push(parent.clone());
                    }
                }
            }
        }

        // Topological rank per commit.
        let mut rank: BTreeMap<String, usize> = BTreeMap::new();
        let mut stack: BTreeSet<String> = BTreeSet::new();
        for commit in parents.keys() {
            rank_of(commit, &parents, &mut rank, &mut stack);
        }

        // Invert parent links into a child adjacency map for descendant walks.
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (commit, ps) in &parents {
            for p in ps {
                let entry = children.entry(p.clone()).or_default();
                if !entry.contains(commit) {
                    entry.push(commit.clone());
                }
            }
        }

        Self { rank, children }
    }

    fn rank(&self, sha: &str) -> usize {
        self.rank.get(sha).copied().unwrap_or(0)
    }

    /// All transitive descendant commits of `sha` (children, grandchildren, …),
    /// excluding `sha` itself. Reachability follows the commit DAG child-ward, so
    /// commits on a sibling branch (or a fork that merely shares an ancestor) are
    /// *not* descendants even when they sit in the same connected component.
    fn strict_descendants(&self, sha: &str) -> BTreeSet<&str> {
        let mut out: BTreeSet<&str> = BTreeSet::new();
        let mut stack: Vec<&str> = self
            .children
            .get(sha)
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect();
        while let Some(c) = stack.pop() {
            if out.insert(c)
                && let Some(kids) = self.children.get(c)
            {
                stack.extend(kids.iter().map(String::as_str));
            }
        }
        out
    }
}

/// Computes the store-wide transaction-time range across every record carrying
/// a parseable transaction handle, as `(earliest, latest)`.
///
/// Daemon callers derive this from the *unfiltered* store and pass it into
/// [`symbol_as_of_transaction_time`] so the `before_first_transaction`
/// diagnostic reflects the whole store rather than a domain-filtered slice
/// (otherwise a mixed store with an earlier non-codegraph transaction and a
/// later codegraph symbol would spuriously report a between-the-two instant as
/// out of range). The CLI `--graph` path already holds the full graph, so it
/// passes `None` and lets the resolver derive the bounds from `records`.
#[must_use]
pub fn store_transaction_bounds(
    records: &[GraphRecord],
) -> Option<(DateTime<chrono::FixedOffset>, DateTime<chrono::FixedOffset>)> {
    let mut min_tx: Option<DateTime<chrono::FixedOffset>> = None;
    let mut max_tx: Option<DateTime<chrono::FixedOffset>> = None;
    for r in records {
        if let Some(tt) =
            record_transaction_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            min_tx = Some(min_tx.map_or(tt, |m| m.min(tt)));
            max_tx = Some(max_tx.map_or(tt, |m| m.max(tt)));
        }
    }
    match (min_tx, max_tx) {
        (Some(min), Some(max)) => Some((min, max)),
        _ => None,
    }
}

/// Finds symbol records as the store knew them at a transaction-time instant.
///
/// Returns, per stable record ID, the version whose transaction time is the
/// latest at or before `tx_as_of`. Records committed after `tx_as_of` (later
/// corrections, supersessions, re-imports) are excluded, so the result is the
/// *prior graph view* — what Egregore knew then, not the current state.
///
/// When `as_of_valid_time` is also supplied, both axes are applied
/// independently (issue #66 AC4): first restrict to versions known by
/// `tx_as_of` (transaction axis), then, within those, return the version whose
/// `valid_time` is the most recent at or before `as_of_valid_time` (valid
/// axis). This answers "what was true at valid time V, as known by transaction
/// time T."
///
/// Tombstones are intentionally ignored on the transaction-time path: a
/// current-state tombstone marks a *later* deletion whose transaction time is
/// not recorded on the tombstone itself, so it must not erase a historical
/// view that predates the deletion (AC2).
///
/// # Errors
///
/// Returns [`TxQueryError`] with code `invalid_timestamp` when `tx_as_of` or
/// `as_of_valid_time` is not a valid RFC 3339 instant.
#[allow(clippy::too_many_lines)]
///
/// `store_tx_bounds` supplies the store-wide transaction range explicitly (see
/// [`store_transaction_bounds`]); pass `None` to derive it from `records`. Use
/// it when `records` is a domain-filtered subset of a larger store so the
/// out-of-range diagnostics stay store-wide.
pub fn symbol_as_of_transaction_time<'r>(
    records: &'r [GraphRecord],
    symbol_name: &str,
    tx_as_of: &str,
    as_of_valid_time: Option<&str>,
    store_tx_bounds: Option<(DateTime<chrono::FixedOffset>, DateTime<chrono::FixedOffset>)>,
) -> Result<TxSymbolQuery<'r>, TxQueryError> {
    let tx_instant = DateTime::parse_from_rfc3339(tx_as_of).map_err(|e| TxQueryError {
        code: "invalid_timestamp".to_owned(),
        message: format!("invalid --tx-as-of timestamp '{tx_as_of}': {e}"),
    })?;
    let vt_requested = match as_of_valid_time {
        Some(v) => Some(DateTime::parse_from_rfc3339(v).map_err(|e| TxQueryError {
            code: "invalid_timestamp".to_owned(),
            message: format!("invalid --as-of timestamp '{v}': {e}"),
        })?),
        None => None,
    };

    let mut diagnostics: Vec<TxDiagnostic> = Vec::new();

    // Deterministic commit ordering (topological rank + per-repository
    // component) used to break same-second/equal-transaction ties and to scope
    // history-removal detection to the queried symbol's own repository.
    let commit_order = CommitOrder::build(records);

    // All Symbol nodes carrying the queried name.
    let named: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| {
            matches!(
                r,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name,
                    ..
                } if name.as_deref() == Some(symbol_name)
            )
        })
        .collect();

    // Store-wide transaction lower bound, used to tell a truly out-of-range
    // instant apart from an in-range instant where the queried symbol simply did
    // not exist yet. Caller-supplied bounds win (domain-filtered daemon reads);
    // otherwise derive from `records` (the CLI holds the full graph).
    let store_min_tx = match store_tx_bounds {
        Some((min, _max)) => Some(min),
        None => store_transaction_bounds(records).map(|(min, _max)| min),
    };

    if named.is_empty() {
        diagnostics.push(TxDiagnostic {
            code: "no_named_symbol".to_owned(),
            message: format!("no Symbol named '{symbol_name}' exists in the store"),
        });
        // Even with no matching symbol, report whether the instant predates the
        // whole store so clients can distinguish an out-of-range temporal query
        // from a genuine in-range absence (a misspelled or removed name).
        if let Some(min) = store_min_tx
            && tx_instant < min
        {
            diagnostics.push(TxDiagnostic {
                code: "before_first_transaction".to_owned(),
                message: format!(
                    "tx-as-of '{tx_as_of}' precedes the earliest known store transaction ('{}'); empty view",
                    min.to_rfc3339()
                ),
            });
        }
        diagnostics.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| a.message.cmp(&b.message)));
        diagnostics.dedup();
        return Ok(TxSymbolQuery {
            records: Vec::new(),
            diagnostics,
        });
    }

    // Track the earliest/latest known transaction time across all named
    // versions so we can report symbol-scoped not-yet-known / after-latest
    // conditions, distinct from the store-wide range computed below.
    let mut name_min_tx: Option<DateTime<chrono::FixedOffset>> = None;
    let mut name_max_tx: Option<DateTime<chrono::FixedOffset>> = None;

    // Best candidate per stable record ID.
    // Comparison key: (valid_time, transaction_time) when a valid-time axis is
    // requested; (transaction_time,) otherwise.
    let mut best: BTreeMap<&str, TxCandidate<'r>> = BTreeMap::new();

    for (named_idx, record) in named.iter().enumerate() {
        // Deterministic tie-break for equal transaction (and valid) times:
        // history records order by commit topological rank (a descendant
        // outranks its ancestor even within the same second); non-history
        // records fall back to input order (later-written wins on a tie).
        let tie = node_git_commit(record).map_or(named_idx, |c| commit_order.rank(c));
        let Some(tt_str) = record_transaction_time(record) else {
            diagnostics.push(TxDiagnostic {
                code: "missing_transaction_metadata".to_owned(),
                message: format!(
                    "record '{}' has no transaction-time metadata; excluded (no current-state fallback)",
                    record.id()
                ),
            });
            continue;
        };
        let Ok(tt) = DateTime::parse_from_rfc3339(tt_str) else {
            diagnostics.push(TxDiagnostic {
                code: "invalid_record_transaction_time".to_owned(),
                message: format!(
                    "record '{}' has an unparseable transaction_time '{tt_str}'; excluded",
                    record.id()
                ),
            });
            continue;
        };

        name_min_tx = Some(name_min_tx.map_or(tt, |m| m.min(tt)));
        name_max_tx = Some(name_max_tx.map_or(tt, |m| m.max(tt)));

        // Transaction axis: exclude anything committed after the instant.
        if tt > tx_instant {
            continue;
        }

        // Valid axis (when requested): exclude versions not yet true at V.
        let vt = if let Some(vt_req) = vt_requested {
            let Some(vt_str) = node_valid_time(record) else {
                diagnostics.push(TxDiagnostic {
                    code: "missing_valid_time".to_owned(),
                    message: format!(
                        "record '{}' has no valid_time but --as-of was supplied; excluded",
                        record.id()
                    ),
                });
                continue;
            };
            let Ok(vt) = DateTime::parse_from_rfc3339(vt_str) else {
                diagnostics.push(TxDiagnostic {
                    code: "invalid_record_valid_time".to_owned(),
                    message: format!(
                        "record '{}' has an unparseable valid_time '{vt_str}'; excluded",
                        record.id()
                    ),
                });
                continue;
            };
            if vt > vt_req {
                continue;
            }
            Some(vt)
        } else {
            None
        };

        let key = record.id();
        let replace = match best.get(key) {
            None => true,
            Some((_, prev_transaction, prev_valid, prev_tie)) => match (vt, prev_valid) {
                // Valid-time axis requested: prefer most-recent valid_time, then
                // most-recent transaction_time, then the commit/input tie-break.
                (Some(cur_vt), Some(prev)) => {
                    (cur_vt, tt, tie) > (*prev, *prev_transaction, *prev_tie)
                }
                // No valid-time axis: prefer most-recent transaction_time, then
                // the commit/input tie-break so equal stamps resolve to the
                // later version rather than the first one seen.
                _ => (tt, tie) > (*prev_transaction, *prev_tie),
            },
        };
        if replace {
            best.insert(key, (record, tt, vt, tie));
        }
    }

    // Out-of-range / not-yet-known diagnostics (annotate, never change the set).
    // `store_min_tx` was resolved above (caller-supplied or derived).
    if let Some(min) = store_min_tx
        && tx_instant < min
    {
        // Truly before any store activity.
        diagnostics.push(TxDiagnostic {
            code: "before_first_transaction".to_owned(),
            message: format!(
                "tx-as-of '{tx_as_of}' precedes the earliest known store transaction ('{}'); empty view",
                min.to_rfc3339()
            ),
        });
    } else if let Some(name_min) = name_min_tx
        && tx_instant < name_min
    {
        // In store range, but the queried symbol was introduced later: a real
        // in-range absence, not an out-of-range query.
        diagnostics.push(TxDiagnostic {
            code: "symbol_not_yet_known".to_owned(),
            message: format!(
                "symbol '{symbol_name}' has no transaction at or before '{tx_as_of}' (first known at '{}'); empty view",
                name_min.to_rfc3339()
            ),
        });
    }
    if let Some(max) = name_max_tx
        && tx_instant >= max
    {
        diagnostics.push(TxDiagnostic {
            code: "after_latest_transaction".to_owned(),
            message: format!(
                "tx-as-of '{tx_as_of}' is at or after the latest known transaction for symbol '{symbol_name}' ('{}'); view reflects all known history",
                max.to_rfc3339()
            ),
        });
    }

    // Sort by (span.start_line, record_id) to match the daemon `symbol_by_name`
    // contract and the non-tx handler, so a `max_results` truncation on the
    // daemon side keeps the documented prefix. Deterministic (AC7).
    let span_start = |r: &GraphRecord| -> Option<usize> {
        if let GraphRecord::Node { span, .. } = r {
            span.map(|s| s.start_line)
        } else {
            None
        }
    };
    // Cross-id supersession (AC2): a Symbol carrying `superseded_by` is dropped
    // once its replacement is *effective in the requested view* — the target
    // record (under any name, so renames count) has a version satisfying every
    // requested axis: transaction time ≤ `tx_as_of`, and valid time ≤ the
    // requested `--as-of` when supplied. Two-axis correctness: if the
    // replacement's `valid_time` is after the requested `--as-of`, it is not yet
    // effective and the older row that was true at that valid time is retained.
    let replacement_effective = |target: &str| -> bool {
        records.iter().any(|r| {
            if r.id() != target {
                return false;
            }
            let Some(tt) =
                record_transaction_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            else {
                return false;
            };
            if tt > tx_instant {
                return false;
            }
            if let Some(vt_req) = vt_requested {
                let Some(vt) =
                    node_valid_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                else {
                    return false;
                };
                if vt > vt_req {
                    return false;
                }
            }
            true
        })
    };
    let mut selected: Vec<&GraphRecord> = best.values().map(|(r, _, _, _)| *r).collect();
    selected.retain(|r| {
        let GraphRecord::Node {
            superseded_by: Some(target),
            ..
        } = r
        else {
            return true;
        };
        if replacement_effective(target.as_str()) {
            diagnostics.push(TxDiagnostic {
                code: "superseded".to_owned(),
                message: format!(
                    "record '{}' is superseded by '{target}', which is effective in the view; excluded",
                    r.id()
                ),
            });
            false
        } else {
            true
        }
    });

    // History-replay removal detection (AC2): `scan-history` emits a full symbol
    // snapshot per commit, so a symbol present at a commit always has a version
    // stamped there. A symbol is absent in the requested view when its latest
    // snapshot commit has a descendant commit, visible in the view, that no longer
    // carries that symbol.
    //
    // Axis handling: the "visible in the view" test bounds commits by the
    // requested axes — known by `tx_as_of` (transaction axis) and, with `--as-of
    // V`, valid at V (valid axis). So a removal that happens *after* V leaves the
    // row intact (the symbol was genuinely true at V), while a removal at or before
    // V correctly drops it — removal is evaluated against the requested valid-time
    // view rather than disabled for two-axis queries.
    //
    // Same-second commits, forks, and multi-repository stores: removal is keyed on
    // commit-DAG reachability, not a component-wide maximum. A symbol present at
    // its latest snapshot commit `L` was removed iff `L` has a *strict descendant*
    // commit, visible in the requested view, that carries no snapshot of that
    // stable ID. Sibling-branch commits — and forks that merely share an ancestor,
    // which land in one connected component — are not descendants of `L`, so a
    // later commit on another branch never makes a still-live symbol look removed.
    // Pruning is per stable record ID, so a surviving `foo` never masks a
    // different stable `foo` that was actually deleted.
    let is_temporal = |r: &GraphRecord| -> bool {
        matches!(
            r,
            GraphRecord::Node {
                temporal: Some(_),
                ..
            } | GraphRecord::Edge {
                temporal: Some(_),
                ..
            }
        )
    };
    // A record is visible in the requested view when it carries a commit known by
    // `tx_as_of` (transaction axis) and, with `--as-of V`, valid at V (valid axis).
    let axes_visible = |r: &GraphRecord| -> bool {
        if node_git_commit(r).is_none() {
            return false;
        }
        let Some(observed) =
            record_transaction_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        else {
            return false;
        };
        if observed > tx_instant {
            return false;
        }
        if let Some(vt_req) = vt_requested {
            let Some(vt) = node_valid_time(r).and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            else {
                return false;
            };
            if vt > vt_req {
                return false;
            }
        }
        true
    };
    // Commits visible in the view, and — per stable ID — the commits at which that
    // symbol has a snapshot in the view.
    let in_view_commits: BTreeSet<&str> = records
        .iter()
        .filter(|r| axes_visible(r))
        .filter_map(node_git_commit)
        .collect();
    let mut id_commits: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for r in &named {
        if axes_visible(r)
            && let Some(commit) = node_git_commit(r)
        {
            id_commits.entry(r.id()).or_default().insert(commit);
        }
    }
    selected.retain(|r| {
        if !is_temporal(r) {
            return true;
        }
        let Some(last_commit) = node_git_commit(r) else {
            return true;
        };
        let snapshots = id_commits.get(r.id());
        // Removed iff a strict descendant of this symbol's latest snapshot is
        // visible in the view but carries no snapshot of this stable ID.
        let removed = commit_order
            .strict_descendants(last_commit)
            .into_iter()
            .any(|c| in_view_commits.contains(c) && snapshots.is_none_or(|s| !s.contains(c)));
        if removed {
            diagnostics.push(TxDiagnostic {
                code: "absent_at_transaction".to_owned(),
                message: format!(
                    "symbol '{symbol_name}' (record '{}') was absent at a commit descending from its last snapshot '{last_commit}' in the requested view; excluded",
                    r.id()
                ),
            });
        }
        !removed
    });

    selected.sort_by(|a, b| {
        span_start(a)
            .cmp(&span_start(b))
            .then_with(|| a.id().cmp(b.id()))
    });

    diagnostics.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| a.message.cmp(&b.message)));
    diagnostics.dedup();

    Ok(TxSymbolQuery {
        records: selected,
        diagnostics,
    })
}

// ── Task Evidence Queries (Issue #48) ──────────────────────────────────────────

/// Error returned when resolving a task ID or handle.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TaskResolveError {
    /// The handle matches more than one task node.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The list of matched task IDs.
        candidates: Vec<String>,
    },
    /// The handle format is malformed or unsupported.
    Unsupported {
        /// The query handle.
        handle: String,
        /// Description of why the handle is unsupported.
        message: String,
    },
}

/// Structured task evidence context returned by [`task_evidence_context`].
#[derive(Debug, Default, Clone)]
pub struct TaskEvidenceContext<'a> {
    /// Stable Task record ID.
    pub task_id: String,
    /// The queried Task node(s), including history versions.
    pub tasks: Vec<&'a GraphRecord>,
    /// `AcceptanceCriterion` nodes owned by the Task.
    pub acceptance_criteria: Vec<&'a GraphRecord>,
    /// Code-graph files or symbols linked to the task.
    pub source_facts: Vec<&'a GraphRecord>,
    /// Agent-authored observations and failures referencing the task.
    pub observations: Vec<&'a GraphRecord>,
    /// Artifact handles (`Artifact`, `PatchArtifact`, etc.) linked to the task.
    pub artifacts: Vec<&'a GraphRecord>,
    /// Verification evidence (`Verification`, `CommandRun`, etc.) linked to the task.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// `Review` nodes (issue comments, PR reviews, etc.) referencing the task.
    pub reviews: Vec<&'a GraphRecord>,
    /// `ExternalLink` nodes referencing source links.
    pub external_links: Vec<&'a GraphRecord>,
    /// Evidence link targets referenced by agent-memory nodes that are absent.
    pub unresolved: Vec<UnresolvedRef>,
}

impl TaskEvidenceContext<'_> {
    /// Returns `true` when no task matching the queried handle exists in the store.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.tasks.is_empty()
            && self.acceptance_criteria.is_empty()
            && self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.reviews.is_empty()
            && self.external_links.is_empty()
            && self.unresolved.is_empty()
    }
}

/// Resolves a task ID or handle to a set of canonical Task record IDs.
///
/// # Errors
///
/// Returns `TaskResolveError` when the handle format is unsupported or ambiguous.
#[allow(clippy::too_many_lines)]
pub fn resolve_task_ids(
    records: &[GraphRecord],
    id_or_handle: &str,
) -> Result<BTreeSet<String>, TaskResolveError> {
    if id_or_handle.is_empty() {
        return Err(TaskResolveError::Unsupported {
            handle: id_or_handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let mut matched_ids = BTreeSet::new();

    // Case 1: Canonical Task record ID
    if id_or_handle.starts_with("project:") {
        let parts: Vec<&str> = id_or_handle.split(':').collect();
        let is_valid = parts.len() == 3
            && parts[0] == "project"
            && parts[1].starts_with('v')
            && parts[1][1..].chars().all(|c| c.is_ascii_digit())
            && parts[2].len() == 64
            && parts[2].chars().all(|c| c.is_ascii_hexdigit());

        if !is_valid {
            return Err(TaskResolveError::Unsupported {
                handle: id_or_handle.to_owned(),
                message: "malformed canonical task ID".to_owned(),
            });
        }

        // Search for a Task node with this ID
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::Task,
                id,
                ..
            } = r
                && id == id_or_handle
            {
                matched_ids.insert(id.clone());
            }
        }
        return Ok(matched_ids);
    }

    // Determine supported handle formats
    let mut is_supported = false;

    // A: GitHub URL
    if id_or_handle.starts_with("https://github.com/")
        || id_or_handle.starts_with("http://github.com/")
    {
        is_supported = true;
    }
    // B: GitHub short handle (owner/repo#num or #num)
    else if id_or_handle.contains('#') {
        if let Some(pos) = id_or_handle.find('#') {
            let num_part = &id_or_handle[pos + 1..];
            if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
                is_supported = true;
            }
        }
    }
    // C: Local JSONL task handle (path ending in .jsonl followed by :local_id)
    else if let Some(pos) = id_or_handle.rfind(':') {
        let (file_path, _) = id_or_handle.split_at(pos);
        if std::path::Path::new(file_path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            is_supported = true;
        }
    }

    if !is_supported {
        return Err(TaskResolveError::Unsupported {
            handle: id_or_handle.to_owned(),
            message: "handle format is not recognized. Supported formats: canonical ID, GitHub URL, GitHub short handle (owner/repo#num), local JSONL handle (path.jsonl:local_id)".to_owned(),
        });
    }

    // Resolve via ExternalLink nodes
    let mut matched_links = BTreeSet::new();

    // Check if handle is a GitHub short handle (owner/repo#num or #num)
    let mut github_short_handle_matches = None;
    if let Some(pos) = id_or_handle.find('#') {
        let repo_part = &id_or_handle[..pos];
        let num_part = &id_or_handle[pos + 1..];
        if !num_part.is_empty() && num_part.chars().all(|c| c.is_ascii_digit()) {
            github_short_handle_matches = Some((repo_part, num_part));
        }
    }

    // Local JSONL handle: convert to native ID representation
    let mut local_native_id = None;
    if let Some(pos) = id_or_handle.rfind(':') {
        let (file_path, local_id) = id_or_handle.split_at(pos);
        let local_id = &local_id[1..];
        if std::path::Path::new(file_path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        {
            let encoded_file = crate::local_project::percent_encode(file_path);
            let encoded_id = crate::local_project::percent_encode(local_id);
            local_native_id = Some(format!("{encoded_file}:{encoded_id}"));
        }
    }

    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::ExternalLink,
            id,
            url,
            system_native_id,
            repository_remote,
            ..
        } = r
        {
            let mut matches = url.as_deref().is_some_and(|u| u == id_or_handle)
                || system_native_id
                    .as_deref()
                    .is_some_and(|n| n == id_or_handle)
                || local_native_id.as_ref().is_some_and(|native_id| {
                    system_native_id.as_deref().is_some_and(|n| n == native_id)
                });

            if let (false, Some((repo_part, num_part))) = (matches, github_short_handle_matches) {
                let native_id_matches = system_native_id.as_deref().is_some_and(|n| {
                    n == format!("issue:{num_part}") || n == format!("pr:{num_part}")
                });
                if native_id_matches {
                    if repo_part.is_empty() {
                        matches = true;
                    } else {
                        let expected_remote =
                            format!("https://github.com/{repo_part}").to_lowercase();
                        matches = repository_remote.as_deref().is_some_and(|r| {
                            r.to_lowercase().trim_end_matches(".git")
                                == expected_remote.trim_end_matches(".git")
                        });
                    }
                }
            }

            if matches {
                matched_links.insert(id.clone());
            }
        }
    }

    // Find Task nodes linked to matched ExternalLinks
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Task,
            id,
            source_external_link_id: Some(sel_id),
            ..
        } = r
            && matched_links.contains(sel_id)
        {
            matched_ids.insert(id.clone());
        }
    }

    // Also check EXTERNAL_HANDLE edges from Task to ExternalLink
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::ExternalHandle,
            source,
            target,
            ..
        } = r
            && matched_links.contains(target)
        {
            for task_record in records {
                if let GraphRecord::Node {
                    kind: NodeKind::Task,
                    id,
                    ..
                } = task_record
                    && id == source
                {
                    matched_ids.insert(id.clone());
                }
            }
        }
    }

    if matched_ids.len() > 1 {
        let candidates: Vec<String> = matched_ids.iter().cloned().collect();
        return Err(TaskResolveError::Ambiguous {
            handle: id_or_handle.to_owned(),
            candidates,
        });
    }

    Ok(matched_ids)
}

/// Retrieves the evidence-backed task context starting from a resolved task ID.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn task_evidence_context<'a>(
    records: &'a [GraphRecord],
    task_id: &str,
) -> TaskEvidenceContext<'a> {
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .collect();

    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();

    let by_id: std::collections::BTreeMap<&str, &GraphRecord> =
        records.iter().map(|r| (r.id(), r)).collect();

    let has_any_temporal_version: BTreeSet<&str> = records
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

    let mut tasks = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            temporal,
            ..
        } = r
            && id == task_id
        {
            let is_historical = temporal.is_some();
            if is_historical || !tombstoned_ids.contains(id.as_str()) {
                tasks.insert(r.id());
            }
        }
    }

    if tasks.is_empty() {
        return TaskEvidenceContext::default();
    }

    let mut acceptance_criteria = BTreeSet::new();
    let mut source_facts = BTreeSet::new();
    let mut observations = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    let mut verification_evidence = BTreeSet::new();
    let mut reviews = BTreeSet::new();
    let mut external_links = BTreeSet::new();

    // Local ExternalLink from Task field
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Task,
            source_external_link_id: Some(sel_id),
            ..
        } = r
            && id == task_id
        {
            external_links.insert(sel_id.as_str());
        }
    }

    // Direct ACs by field
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::AcceptanceCriterion,
            parent_task_id: Some(parent_task_id),
            ..
        } = r
            && parent_task_id == task_id
        {
            acceptance_criteria.insert(id.as_str());
        }
    }

    // Scan all edges to collect direct links
    for r in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            ..
        } = r
        {
            if tombstoned_ids.contains(edge_id.as_str())
                && !has_any_temporal_version.contains(edge_id.as_str())
            {
                continue;
            }

            match label {
                EdgeLabel::OwnedByTask if target == task_id => {
                    acceptance_criteria.insert(source.as_str());
                }
                EdgeLabel::ExternalHandle if source == task_id => {
                    external_links.insert(target.as_str());
                }
                EdgeLabel::TouchesFile | EdgeLabel::MentionsSymbol if source == task_id => {
                    source_facts.insert(target.as_str());
                }
                EdgeLabel::ReferencesTask if target == task_id => {
                    if let Some(GraphRecord::Node { kind, .. }) = by_id.get(source.as_str()) {
                        match kind {
                            NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
                                observations.insert(source.as_str());
                            }
                            NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
                                artifacts.insert(source.as_str());
                            }
                            NodeKind::Verification | NodeKind::CommandRun | NodeKind::TestRun => {
                                verification_evidence.insert(source.as_str());
                            }
                            NodeKind::Review => {
                                reviews.insert(source.as_str());
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Scan all nodes for evidence_links targeting the Task ID
    for r in records {
        if let GraphRecord::Node {
            id,
            kind,
            evidence_links: Some(links),
            ..
        } = r
        {
            let links_to_task = links
                .iter()
                .any(|link| link.target_record_id.as_deref() == Some(task_id));
            if links_to_task {
                match kind {
                    NodeKind::Observation | NodeKind::Decision | NodeKind::Failure => {
                        observations.insert(id.as_str());
                    }
                    NodeKind::Artifact | NodeKind::PatchArtifact | NodeKind::FileEdit => {
                        artifacts.insert(id.as_str());
                    }
                    NodeKind::Verification | NodeKind::CommandRun | NodeKind::TestRun => {
                        verification_evidence.insert(id.as_str());
                    }
                    NodeKind::Review => {
                        reviews.insert(id.as_str());
                    }
                    _ => {}
                }
            }
        }
    }

    // Seed BFS visited and frontier with all collected IDs
    let mut visited = BTreeSet::new();
    visited.insert(task_id);
    for id in &acceptance_criteria {
        visited.insert(*id);
    }
    for id in &source_facts {
        visited.insert(*id);
    }
    for id in &observations {
        visited.insert(*id);
    }
    for id in &artifacts {
        visited.insert(*id);
    }
    for id in &verification_evidence {
        visited.insert(*id);
    }
    for id in &reviews {
        visited.insert(*id);
    }
    for id in &external_links {
        visited.insert(*id);
    }

    let mut frontier: BTreeSet<&str> = visited.clone();
    let mut temporal_evidence_scanned: BTreeSet<String> = BTreeSet::new();
    let mut evidence_links_scanned: BTreeSet<&str> = BTreeSet::new();
    let mut unresolved = Vec::new();

    let classify_and_insert_task = |record_id: &'a str,
                                    source_facts: &mut BTreeSet<&'a str>,
                                    observations: &mut BTreeSet<&'a str>,
                                    artifacts: &mut BTreeSet<&'a str>,
                                    verification_evidence: &mut BTreeSet<&'a str>,
                                    reviews: &mut BTreeSet<&'a str>|
     -> bool {
        if tombstoned_ids.contains(record_id) && !has_any_temporal_version.contains(record_id) {
            return false;
        }
        let Some(rec) = by_id.get(record_id) else {
            return false;
        };
        let GraphRecord::Node { kind, .. } = rec else {
            return false;
        };
        if *kind == NodeKind::Review {
            reviews.insert(record_id);
            return true;
        }
        match classify_node(*kind) {
            Some(ContextSection::SourceFact) => {
                source_facts.insert(record_id);
                true
            }
            Some(ContextSection::Observation) => {
                observations.insert(record_id);
                true
            }
            Some(ContextSection::Artifact) => {
                artifacts.insert(record_id);
                true
            }
            Some(ContextSection::VerificationEvidence) => {
                verification_evidence.insert(record_id);
                true
            }
            _ => false,
        }
    };

    // BFS loop - run 2 more hops
    for _hop in 0..2_usize {
        let mut next_frontier: Vec<&'a str> = Vec::new();

        for record in records {
            match record {
                GraphRecord::Edge {
                    id: edge_id,
                    label,
                    source,
                    target,
                    ..
                } => {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if tombstoned_ids.contains(edge_id.as_str())
                        && !has_any_temporal_version.contains(edge_id.as_str())
                    {
                        continue;
                    }
                    let candidate = if frontier.contains(source.as_str()) {
                        Some(target.as_str())
                    } else if frontier.contains(target.as_str()) && !is_forward_only_label(*label) {
                        Some(source.as_str())
                    } else {
                        None
                    };
                    if let Some(id) = candidate
                        && visited.insert(id)
                    {
                        let was_classified = classify_and_insert_task(
                            id,
                            &mut source_facts,
                            &mut observations,
                            &mut artifacts,
                            &mut verification_evidence,
                            &mut reviews,
                        );
                        if was_classified
                            || is_bfs_relay_node(
                                id,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(id);
                        }
                    }
                }
                GraphRecord::Node {
                    id: node_id,
                    evidence_links: Some(links),
                    temporal,
                    ..
                } => {
                    let already_scanned = temporal.as_ref().map_or_else(
                        || !evidence_links_scanned.insert(node_id.as_str()),
                        |t| {
                            let key = format!("{}@{}", node_id, t.git_commit);
                            !temporal_evidence_scanned.insert(key)
                        },
                    );
                    if already_scanned {
                        continue;
                    }
                    if tombstoned_ids.contains(node_id.as_str())
                        && !has_any_temporal_version.contains(node_id.as_str())
                    {
                        visited.insert(node_id.as_str());
                        continue;
                    }

                    if frontier.contains(node_id.as_str()) {
                        for link in links {
                            if let Some(target_id) = &link.target_record_id {
                                if present_ids.contains(target_id.as_str())
                                    && !visited.contains(target_id.as_str())
                                {
                                    let target_classified = classify_and_insert_task(
                                        target_id.as_str(),
                                        &mut source_facts,
                                        &mut observations,
                                        &mut artifacts,
                                        &mut verification_evidence,
                                        &mut reviews,
                                    );
                                    visited.insert(target_id.as_str());
                                    if target_classified {
                                        next_frontier.push(target_id.as_str());
                                    }
                                } else if !present_ids.contains(target_id.as_str()) {
                                    unresolved.push(UnresolvedRef {
                                        source_record_id: node_id.clone(),
                                        target_handle: target_id.clone(),
                                        relation: link.relation.clone(),
                                        target_domain: link.target_domain.clone(),
                                    });
                                }
                            } else if let Some(handle) = evidence_link_triple_handle(link) {
                                unresolved.push(UnresolvedRef {
                                    source_record_id: node_id.clone(),
                                    target_handle: handle,
                                    relation: link.relation.clone(),
                                    target_domain: link.target_domain.clone(),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier.into_iter().collect();
    }

    let resolve = |ids: &BTreeSet<&str>| -> Vec<&'a GraphRecord> {
        let mut out: Vec<&'a GraphRecord> = records
            .iter()
            .filter(|r| {
                ids.contains(r.id())
                    && match r {
                        GraphRecord::Node {
                            temporal: Some(_), ..
                        }
                        | GraphRecord::Edge {
                            temporal: Some(_), ..
                        } => true,
                        _ => !tombstoned_ids.contains(r.id()),
                    }
            })
            .collect();
        out.sort_by(|a, b| {
            a.id().cmp(b.id()).then_with(|| {
                let a_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = a
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                let b_commit = if let GraphRecord::Node {
                    temporal: Some(t), ..
                } = b
                {
                    t.git_commit.as_str()
                } else {
                    ""
                };
                a_commit.cmp(b_commit)
            })
        });
        out
    };

    TaskEvidenceContext {
        task_id: task_id.to_owned(),
        tasks: resolve(&tasks),
        acceptance_criteria: resolve(&acceptance_criteria),
        source_facts: resolve(&source_facts),
        observations: resolve(&observations),
        artifacts: resolve(&artifacts),
        verification_evidence: resolve(&verification_evidence),
        reviews: resolve(&reviews),
        external_links: resolve(&external_links),
        unresolved: {
            let mut u = unresolved;
            u.sort_by(|a, b| {
                a.source_record_id
                    .cmp(&b.source_record_id)
                    .then_with(|| a.target_handle.cmp(&b.target_handle))
                    .then_with(|| a.relation.cmp(&b.relation))
                    .then_with(|| a.target_domain.cmp(&b.target_domain))
            });
            u.dedup();
            u
        },
    }
}

// ── user_context query helpers ────────────────────────────────────────────────

fn scope_subset(a: &UserContextScope, b: &UserContextScope) -> bool {
    if let Some(r) = &a.repo {
        if Some(r) != b.repo.as_ref() {
            return false;
        }
    }
    if let Some(p) = &a.path_glob {
        if Some(p) != b.path_glob.as_ref() {
            return false;
        }
    }
    if let Some(l) = &a.language {
        if Some(l) != b.language.as_ref() {
            return false;
        }
    }
    if let Some(ph) = &a.lifecycle_phase {
        if Some(ph) != b.lifecycle_phase.as_ref() {
            return false;
        }
    }
    true
}

/// Checks if two UserContextScopes are compatible (one is subset of other or equal)
#[must_use]
pub fn scopes_compatible(a: &UserContextScope, b: &UserContextScope) -> bool {
    scope_subset(a, b) || scope_subset(b, a)
}

/// Checks if a record scope matches a query scope
#[must_use]
pub fn scope_matches(record_scope: &UserContextScope, query_scope: &UserContextScope) -> bool {
    if let (Some(r_repo), Some(q_repo)) = (&record_scope.repo, &query_scope.repo) {
        if r_repo != q_repo {
            return false;
        }
    }
    if let (Some(r_path), Some(q_path)) = (&record_scope.path_glob, &query_scope.path_glob) {
        if r_path != q_path {
            return false;
        }
    }
    if let (Some(r_lang), Some(q_lang)) = (&record_scope.language, &query_scope.language) {
        if r_lang != q_lang {
            return false;
        }
    }
    if let (Some(r_phase), Some(q_phase)) =
        (&record_scope.lifecycle_phase, &query_scope.lifecycle_phase)
    {
        if r_phase != q_phase {
            return false;
        }
    }
    true
}

/// Calculates Jaccard token similarity between two proposed rule texts
#[must_use]
pub fn jaccard_similarity(s1: &str, s2: &str) -> f64 {
    let normalize = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect()
    };
    let w1 = normalize(s1);
    let w2 = normalize(s2);
    if w1.is_empty() && w2.is_empty() {
        return 1.0;
    }
    let set1: std::collections::BTreeSet<String> = w1.into_iter().collect();
    let set2: std::collections::BTreeSet<String> = w2.into_iter().collect();

    let intersection = set1.intersection(&set2).count() as f64;
    let union = set1.union(&set2).count() as f64;
    intersection / union
}

/// Returns the latest decision node for the given candidate ID.
#[must_use]
pub fn latest_decision_for_candidate<'a>(
    records: &'a [GraphRecord],
    candidate_id: &str,
) -> Option<&'a GraphRecord> {
    records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node {
                kind: NodeKind::PromotionDecision,
                user_context,
                ..
            } = r
            {
                user_context.candidate_id.as_deref() == Some(candidate_id)
            } else {
                false
            }
        })
        .max_by(|a, b| {
            let a_time = match a {
                GraphRecord::Node { user_context, .. } => {
                    user_context.decided_at.as_deref().unwrap_or("")
                }
                _ => "",
            };
            let b_time = match b {
                GraphRecord::Node { user_context, .. } => {
                    user_context.decided_at.as_deref().unwrap_or("")
                }
                _ => "",
            };
            if let (Ok(a_t), Ok(b_t)) = (
                chrono::DateTime::parse_from_rfc3339(a_time),
                chrono::DateTime::parse_from_rfc3339(b_time),
            ) {
                a_t.cmp(&b_t)
            } else {
                a_time.cmp(b_time)
            }
        })
}

/// Checks if a candidate is suppressed under the rejection debounce window
#[must_use]
pub fn is_candidate_suppressed(records: &[GraphRecord], cand_id: &str) -> Option<String> {
    let cand = records.iter().rfind(|r| r.id() == cand_id)?;
    let (cand_text, cand_scope, superseded_by_id) = match cand {
        GraphRecord::Node {
            user_context,
            superseded_by,
            ..
        } => {
            let text = user_context.proposed_rule_text.as_deref()?;
            let scope = user_context.scope.as_ref()?;
            (text, scope, superseded_by.as_deref())
        }
        _ => return None,
    };

    if let Some(old_id) = superseded_by_id {
        if let Some(old_rec) = records.iter().rfind(|r| r.id() == old_id) {
            let decision = records.iter().rfind(|r| {
                if let GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } = r
                {
                    user_context.candidate_id.as_deref() == Some(old_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                } else {
                    false
                }
            });
            if let Some(GraphRecord::Node {
                user_context: decision_fields,
                ..
            }) = decision
            {
                if decision_fields.outcome.as_deref() == Some("rejected") {
                    let decided_at_str = decision_fields.decided_at.as_deref()?;
                    let current_time_str = match cand {
                        GraphRecord::Node { valid_time, .. } => valid_time.as_deref(),
                        _ => None,
                    }
                    .unwrap_or("");

                    let elapsed_ok = if let Ok(decided_at) =
                        chrono::DateTime::parse_from_rfc3339(decided_at_str)
                        && let Ok(current_time) =
                            chrono::DateTime::parse_from_rfc3339(current_time_str)
                    {
                        current_time.signed_duration_since(decided_at) >= chrono::Duration::days(30)
                    } else {
                        false
                    };

                    let old_obs: std::collections::BTreeSet<&str> = match old_rec {
                        GraphRecord::Node { user_context, .. } => user_context
                            .supporting_evidence
                            .as_deref()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|l| l.target_record_id.as_deref())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => std::collections::BTreeSet::new(),
                    };

                    let new_obs: std::collections::BTreeSet<&str> = match cand {
                        GraphRecord::Node { user_context, .. } => user_context
                            .supporting_evidence
                            .as_deref()
                            .map(|v| {
                                v.iter()
                                    .filter_map(|l| l.target_record_id.as_deref())
                                    .collect()
                            })
                            .unwrap_or_default(),
                        _ => std::collections::BTreeSet::new(),
                    };

                    let additional_count = new_obs.difference(&old_obs).count();

                    if additional_count < 5 || !elapsed_ok {
                        return Some("rejection_debounce".to_string());
                    }
                }
            }
        }
    }

    for old_rec in records {
        let (old_id, old_text, old_scope) = match old_rec {
            GraphRecord::Node {
                id,
                kind: NodeKind::PromoteCandidate,
                user_context,
                ..
            } => {
                if let Some(text) = &user_context.proposed_rule_text
                    && let Some(scope) = &user_context.scope
                {
                    (id.as_str(), text.as_str(), scope)
                } else {
                    continue;
                }
            }
            _ => continue,
        };
        if old_id == cand_id {
            continue;
        }
        if jaccard_similarity(cand_text, old_text) >= 0.6
            && scopes_compatible(cand_scope, old_scope)
        {
            let decision = records.iter().rfind(|r| {
                if let GraphRecord::Node {
                    kind: NodeKind::PromotionDecision,
                    user_context,
                    ..
                } = r
                {
                    user_context.candidate_id.as_deref() == Some(old_id)
                        && user_context.outcome.as_deref() == Some("rejected")
                } else {
                    false
                }
            });
            if let Some(GraphRecord::Node {
                user_context: decision_fields,
                ..
            }) = decision
            {
                if decision_fields.outcome.as_deref() == Some("rejected") {
                    let decided_at_str = decision_fields.decided_at.as_deref()?;
                    let current_time_str = match cand {
                        GraphRecord::Node { valid_time, .. } => valid_time.as_deref(),
                        _ => None,
                    }
                    .unwrap_or("");

                    let elapsed_ok = if let Ok(decided_at) =
                        chrono::DateTime::parse_from_rfc3339(decided_at_str)
                        && let Ok(current_time) =
                            chrono::DateTime::parse_from_rfc3339(current_time_str)
                    {
                        current_time.signed_duration_since(decided_at) >= chrono::Duration::days(30)
                    } else {
                        false
                    };

                    let old_obs: std::collections::BTreeSet<&str> = {
                        match old_rec {
                            GraphRecord::Node { user_context, .. } => user_context
                                .supporting_evidence
                                .as_deref()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|l| l.target_record_id.as_deref())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            _ => std::collections::BTreeSet::new(),
                        }
                    };

                    let new_obs: std::collections::BTreeSet<&str> = {
                        match cand {
                            GraphRecord::Node { user_context, .. } => user_context
                                .supporting_evidence
                                .as_deref()
                                .map(|v| {
                                    v.iter()
                                        .filter_map(|l| l.target_record_id.as_deref())
                                        .collect()
                                })
                                .unwrap_or_default(),
                            _ => std::collections::BTreeSet::new(),
                        }
                    };

                    let additional_count = new_obs.difference(&old_obs).count();

                    if additional_count < 5 || !elapsed_ok {
                        return Some("rejection_debounce".to_string());
                    }
                }
            }
        }
    }

    None
}

/// Returns a list of pending candidates (those without a decision, or whose decision is deferred)
#[must_use]
pub fn pending_candidates<'a>(
    records: &'a [GraphRecord],
    query_scope: Option<&UserContextScope>,
) -> Vec<&'a GraphRecord> {
    let mut candidates = Vec::new();

    let mut terminal_candidates = std::collections::BTreeSet::new();
    let terminal_outcomes = ["approved", "edited_then_approved", "rejected", "expired"];

    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } = rec
            && let (Some(cand_id), Some(outcome)) =
                (&user_context.candidate_id, &user_context.outcome)
            && terminal_outcomes.contains(&outcome.as_str())
        {
            terminal_candidates.insert(cand_id.clone());
        }
    }

    for rec in records {
        if let GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            ..
        } = rec
        {
            if terminal_candidates.contains(rec.id()) {
                continue;
            }
            if is_candidate_suppressed(records, rec.id()).is_some() {
                continue;
            }
            if let Some(q_scope) = query_scope {
                if let Some(r_scope) = &user_context.scope {
                    if !scope_matches(r_scope, q_scope) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            candidates.push(rec);
        }
    }

    candidates.sort_by_key(|c| c.id());
    candidates
}

/// Returns active policy records matching the query scope
#[must_use]
pub fn active_policy<'a>(
    records: &'a [GraphRecord],
    query_scope: Option<&UserContextScope>,
) -> Vec<&'a GraphRecord> {
    let revoked_ids: std::collections::BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Node {
                kind, user_context, ..
            } = r
            {
                if matches!(
                    kind,
                    NodeKind::Preference
                        | NodeKind::WorkflowRule
                        | NodeKind::NamingDecision
                        | NodeKind::Constraint
                ) && user_context.active_to.is_some()
                {
                    return Some(r.id());
                }
            }
            None
        })
        .collect();

    let mut policy_map = std::collections::BTreeMap::new();
    for rec in records {
        if let GraphRecord::Node {
            kind, user_context, ..
        } = rec
        {
            if matches!(
                kind,
                NodeKind::Preference
                    | NodeKind::WorkflowRule
                    | NodeKind::NamingDecision
                    | NodeKind::Constraint
            ) {
                if revoked_ids.contains(rec.id()) {
                    continue;
                }
                if user_context.active_to.is_some() {
                    continue;
                }
                if audit_trail(records, rec).is_err() {
                    continue;
                }
                if let Some(q_scope) = query_scope {
                    if let Some(r_scope) = &user_context.scope {
                        if !scope_matches(r_scope, q_scope) {
                            continue;
                        }
                    } else {
                        continue;
                    }
                }
                policy_map.insert(rec.id(), rec);
            }
        }
    }
    let mut policy: Vec<&'a GraphRecord> = policy_map.into_values().collect();
    policy.sort_by_key(|p| p.id());
    policy
}

fn validate_contradicting_evidence_links(
    candidate_id: &str,
    links: &[EvidenceLink],
    records: &[GraphRecord],
) -> std::result::Result<(), String> {
    for link in links {
        if link.target_domain != "user_context" || link.relation != "CONTRADICTS" {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence for '{}' must use target_domain 'user_context' and relation CONTRADICTS",
                candidate_id
            ));
        }
        let conf_val: f64 = link.confidence.parse().map_err(|_| {
            format!(
                "PromoteCandidate.contradicting_evidence[].confidence '{}' must be a numeric float string",
                link.confidence
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence[].confidence '{}' must be in the range [0.0, 1.0]",
                link.confidence
            ));
        }
        let target_id = link.target_record_id.as_deref().ok_or_else(|| {
            "PromoteCandidate.contradicting_evidence[].target_record_id is missing".to_owned()
        })?;
        let target_node = records
            .iter()
            .rfind(|r| r.id() == target_id)
            .ok_or_else(|| format!("evidence target '{}' not found", target_id))?;
        let kind = match target_node {
            GraphRecord::Node { kind, .. } => *kind,
            _ => return Err(format!("evidence target '{}' is not a node", target_id)),
        };
        if !matches!(
            kind,
            NodeKind::Preference
                | NodeKind::WorkflowRule
                | NodeKind::NamingDecision
                | NodeKind::Constraint
        ) {
            return Err(format!(
                "PromoteCandidate.contradicting_evidence target '{}' must be a Preference, WorkflowRule, NamingDecision, or Constraint, got {}",
                target_id,
                kind.as_str()
            ));
        }
    }
    Ok(())
}

fn validate_scope(
    scope: Option<&UserContextScope>,
    field_name: &str,
) -> std::result::Result<(), String> {
    let scope = scope.ok_or_else(|| format!("{} lacks scope", field_name))?;
    if let Some(lifecycle_phase) = scope.lifecycle_phase.as_deref() {
        if !["pre_commit", "pre_pr", "pre_merge", "runtime", "any"].contains(&lifecycle_phase) {
            return Err(format!(
                "{} has invalid lifecycle_phase '{}'",
                field_name, lifecycle_phase
            ));
        }
    }
    Ok(())
}

fn validate_durable_fields(
    kind: NodeKind,
    user_context: &crate::ir::UserContextFields,
    durable_id: &str,
) -> std::result::Result<(), String> {
    match kind {
        NodeKind::Preference => {
            if user_context.proposed_rule_kind.as_deref() != Some("preference") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'preference'",
                    durable_id
                ));
            }
            let text = user_context.rule_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!("Durable record '{}' lacks rule_text", durable_id));
            }
        }
        NodeKind::WorkflowRule => {
            if user_context.proposed_rule_kind.as_deref() != Some("workflow_rule") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'workflow_rule'",
                    durable_id
                ));
            }
            let text = user_context.rule_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!("Durable record '{}' lacks rule_text", durable_id));
            }
            let triggers = user_context
                .triggers
                .as_ref()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| format!("Durable record '{}' lacks triggers", durable_id))?;
            for trigger in triggers {
                if trigger.is_empty() {
                    return Err(format!(
                        "Durable record '{}' has empty trigger entry",
                        durable_id
                    ));
                }
                if ![
                    "pre_commit",
                    "pre_pr",
                    "pre_merge",
                    "pre_command",
                    "post_command",
                ]
                .contains(&trigger.as_str())
                {
                    return Err(format!(
                        "Durable record '{}' trigger '{}' is invalid",
                        durable_id, trigger
                    ));
                }
            }
            let action_summary = user_context.action_summary.as_deref().unwrap_or("");
            if action_summary.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks action_summary",
                    durable_id
                ));
            }
        }
        NodeKind::NamingDecision => {
            if user_context.proposed_rule_kind.as_deref() != Some("naming_decision") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'naming_decision'",
                    durable_id
                ));
            }
            let entity_kind = user_context.entity_kind.as_deref().unwrap_or("");
            if ![
                "crate", "module", "type", "function", "field", "feature", "other",
            ]
            .contains(&entity_kind)
            {
                return Err(format!(
                    "Durable record '{}' entity_kind '{}' is invalid",
                    durable_id, entity_kind
                ));
            }
            let name = user_context.canonical_name.as_deref().unwrap_or("");
            if name.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks canonical_name",
                    durable_id
                ));
            }
            if user_context.alternatives_rejected.is_none() {
                return Err(format!(
                    "Durable record '{}' lacks alternatives_rejected",
                    durable_id
                ));
            }
        }
        NodeKind::Constraint => {
            if user_context.proposed_rule_kind.as_deref() != Some("constraint") {
                return Err(format!(
                    "Durable record '{}' proposed_rule_kind must be 'constraint'",
                    durable_id
                ));
            }
            let text = user_context.constraint_text.as_deref().unwrap_or("");
            if text.is_empty() {
                return Err(format!(
                    "Durable record '{}' lacks constraint_text",
                    durable_id
                ));
            }
            let level = user_context.enforcement_level.as_deref().unwrap_or("");
            if !["advisory", "blocking"].contains(&level) {
                return Err(format!(
                    "Durable record '{}' enforcement_level '{}' is invalid",
                    durable_id, level
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Traces the approval chain back to supporting observations
///
/// # Errors
///
/// Returns an error if any hop in the chain is missing, stale, or ambiguous.
pub fn audit_trail<'a>(
    records: &'a [GraphRecord],
    durable: &'a GraphRecord,
) -> std::result::Result<Vec<&'a GraphRecord>, String> {
    let mut chain = Vec::new();

    let durable_id = durable.id();
    let kind = match durable {
        GraphRecord::Node { kind, .. } => *kind,
        _ => return Err(format!("Durable record '{}' is not a node", durable_id)),
    };
    if !matches!(
        kind,
        NodeKind::Preference
            | NodeKind::WorkflowRule
            | NodeKind::NamingDecision
            | NodeKind::Constraint
    ) {
        return Err(format!(
            "Record '{}' is not a durable user-context policy node",
            durable_id
        ));
    }
    chain.push(durable);

    let user_context = match durable {
        GraphRecord::Node { user_context, .. } => user_context,
        _ => unreachable!(),
    };

    validate_scope(
        user_context.scope.as_ref(),
        &format!("Durable record '{}'", durable_id),
    )?;
    validate_durable_fields(kind, user_context, durable_id)?;

    let decision_id = user_context
        .approval_decision_id
        .as_deref()
        .ok_or_else(|| format!("Durable record '{}' lacks approval_decision_id", durable_id))?;

    let decision = records
        .iter()
        .rfind(|r| r.id() == decision_id)
        .ok_or_else(|| {
            format!(
                "Approval decision '{}' not found for durable record '{}'",
                decision_id, durable_id
            )
        })?;
    chain.push(decision);

    let decision_fields = match decision {
        GraphRecord::Node {
            kind: NodeKind::PromotionDecision,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(format!(
                "Decision '{}' is not a PromotionDecision node",
                decision_id
            ));
        }
    };
    let decided_by = decision_fields
        .decided_by
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks decided_by", decision_id))?;
    if decided_by.is_empty() {
        return Err(format!("Decision '{}' has empty decided_by", decision_id));
    }
    let outcome = decision_fields
        .outcome
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks outcome", decision_id))?;
    if outcome != "approved" && outcome != "edited_then_approved" {
        return Err(format!(
            "Decision '{}' has non-approving outcome '{}'",
            decision_id, outcome
        ));
    }
    let mat_id = decision_fields
        .materialized_record_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks materialized_record_id", decision_id))?;
    if mat_id != durable_id {
        return Err(format!(
            "Decision '{}' targets materialized record '{}', expected '{}'",
            decision_id, mat_id, durable_id
        ));
    }
    let decided_at = decision_fields.decided_at.as_deref();
    let active_from = user_context.active_from.as_deref();

    let decided_at_str =
        decided_at.ok_or_else(|| format!("Decision '{}' lacks decided_at", decision_id))?;
    let active_from_str =
        active_from.ok_or_else(|| format!("Durable record '{}' lacks active_from", durable_id))?;

    let decided_at_parsed = chrono::DateTime::parse_from_rfc3339(decided_at_str)
        .map_err(|e| format!("Decision '{}' has invalid decided_at: {}", decision_id, e))?;
    let active_from_parsed =
        chrono::DateTime::parse_from_rfc3339(active_from_str).map_err(|e| {
            format!(
                "Durable record '{}' has invalid active_from: {}",
                durable_id, e
            )
        })?;

    if decided_at_parsed != active_from_parsed {
        return Err(format!(
            "Durable record '{}' active_from '{}' does not match decision '{}' decided_at '{}'",
            durable_id, active_from_str, decision_id, decided_at_str
        ));
    }
    let prompt_id = decision_fields
        .prompt_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks prompt_id", decision_id))?;
    let prompt = records
        .iter()
        .rfind(|r| r.id() == prompt_id)
        .ok_or_else(|| {
            format!(
                "PromotionPrompt '{}' not found for decision '{}'",
                prompt_id, decision_id
            )
        })?;
    chain.push(prompt);

    let prompt_fields = match prompt {
        GraphRecord::Node {
            kind: NodeKind::PromotionPrompt,
            user_context,
            ..
        } => user_context,
        _ => {
            return Err(format!(
                "Prompt '{}' is not a PromotionPrompt node",
                prompt_id
            ));
        }
    };
    let candidate_id = prompt_fields
        .candidate_id
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks candidate_id", prompt_id))?;
    let surface = prompt_fields
        .prompt_surface
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompt_surface", prompt_id))?;
    if !["cli", "mcp", "web", "other"].contains(&surface) {
        return Err(format!(
            "Prompt '{}' prompt_surface '{}' is invalid",
            prompt_id, surface
        ));
    }
    let prompt_text = prompt_fields
        .prompt_text
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompt_text", prompt_id))?;
    if prompt_text.is_empty() {
        return Err(format!("Prompt '{}' has empty prompt_text", prompt_id));
    }
    let prompted_at = prompt_fields
        .prompted_at
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompted_at", prompt_id))?;
    chrono::DateTime::parse_from_rfc3339(prompted_at)
        .map_err(|e| format!("Prompt '{}' has invalid prompted_at: {}", prompt_id, e))?;
    let prompted_to = prompt_fields
        .prompted_to
        .as_deref()
        .ok_or_else(|| format!("Prompt '{}' lacks prompted_to", prompt_id))?;
    if prompted_to.is_empty() {
        return Err(format!("Prompt '{}' has empty prompted_to", prompt_id));
    }
    let decision_candidate_id = decision_fields
        .candidate_id
        .as_deref()
        .ok_or_else(|| format!("Decision '{}' lacks candidate_id", decision_id))?;
    if decision_candidate_id != candidate_id {
        return Err(format!(
            "Decision '{}' targets candidate '{}', but prompt '{}' targets candidate '{}'",
            decision_id, decision_candidate_id, prompt_id, candidate_id
        ));
    }
    let candidate = records
        .iter()
        .rfind(|r| r.id() == candidate_id)
        .ok_or_else(|| {
            format!(
                "PromoteCandidate '{}' not found for prompt '{}'",
                candidate_id, prompt_id
            )
        })?;
    chain.push(candidate);

    let (candidate_fields, cand_confidence, cand_evidence_quality) = match candidate {
        GraphRecord::Node {
            kind: NodeKind::PromoteCandidate,
            user_context,
            confidence,
            evidence_quality,
            ..
        } => (
            user_context,
            confidence.as_deref(),
            evidence_quality.as_deref(),
        ),
        _ => {
            return Err(format!(
                "Candidate '{}' is not a PromoteCandidate node",
                candidate_id
            ));
        }
    };

    let conf_str = cand_confidence
        .ok_or_else(|| format!("PromoteCandidate '{}' lacks confidence", candidate_id))?;
    let conf_val: f64 = conf_str.parse().map_err(|_| {
        format!(
            "PromoteCandidate '{}' confidence '{}' must be a numeric float string",
            candidate_id, conf_str
        )
    })?;
    if !(0.0..=1.0).contains(&conf_val) {
        return Err(format!(
            "PromoteCandidate '{}' confidence '{}' must be in the range [0.0, 1.0]",
            candidate_id, conf_str
        ));
    }

    let eq_str = cand_evidence_quality
        .ok_or_else(|| format!("PromoteCandidate '{}' lacks evidence_quality", candidate_id))?;
    if !["verbatim", "summarized", "referenced_only"].contains(&eq_str) {
        return Err(format!(
            "PromoteCandidate '{}' evidence_quality '{}' is invalid (must be verbatim, summarized, or referenced_only)",
            candidate_id, eq_str
        ));
    }

    validate_scope(
        candidate_fields.scope.as_ref(),
        &format!("PromoteCandidate '{}'", candidate_id),
    )?;

    let contradicting = candidate_fields
        .contradicting_evidence
        .as_ref()
        .ok_or_else(|| {
            format!(
                "PromoteCandidate '{}' lacks contradicting_evidence",
                candidate_id
            )
        })?;
    validate_contradicting_evidence_links(candidate_id, contradicting, records)?;

    // Perform candidate-kind/body consistency checks
    let expected_kind = match kind {
        NodeKind::Preference => "preference",
        NodeKind::WorkflowRule => "workflow_rule",
        NodeKind::NamingDecision => "naming_decision",
        NodeKind::Constraint => "constraint",
        _ => unreachable!(),
    };
    if candidate_fields.proposed_rule_kind.as_deref() != Some(expected_kind) {
        return Err(format!(
            "Candidate '{}' proposed rule kind '{:?}' does not match durable policy kind '{:?}'",
            candidate_id, candidate_fields.proposed_rule_kind, kind
        ));
    }

    let is_edited = outcome == "edited_then_approved";
    match kind {
        NodeKind::Preference | NodeKind::WorkflowRule => {
            let durable_text = user_context.rule_text.as_deref().unwrap_or("");
            if is_edited {
                let edited_text = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_text != edited_text {
                    return Err(format!(
                        "Durable record rule_text '{}' does not match decision edited_rule_text '{}'",
                        durable_text, edited_text
                    ));
                }
            } else {
                let cand_text = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_text = redact_value(cand_text);
                if durable_text != redacted_cand_text {
                    return Err(format!(
                        "Durable record rule_text '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_text, cand_text, redacted_cand_text
                    ));
                }
            }
        }
        NodeKind::Constraint => {
            let durable_text = user_context.constraint_text.as_deref().unwrap_or("");
            if is_edited {
                let edited_text = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_text != edited_text {
                    return Err(format!(
                        "Durable record constraint_text '{}' does not match decision edited_rule_text '{}'",
                        durable_text, edited_text
                    ));
                }
            } else {
                let cand_text = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_text = redact_value(cand_text);
                if durable_text != redacted_cand_text {
                    return Err(format!(
                        "Durable record constraint_text '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_text, cand_text, redacted_cand_text
                    ));
                }
            }
        }
        NodeKind::NamingDecision => {
            let durable_name = user_context.canonical_name.as_deref().unwrap_or("");
            if is_edited {
                let edited_name = decision_fields.edited_rule_text.as_deref().unwrap_or("");
                if durable_name != edited_name {
                    return Err(format!(
                        "Durable record canonical_name '{}' does not match decision edited_rule_text '{}'",
                        durable_name, edited_name
                    ));
                }
            } else {
                let cand_name = candidate_fields.proposed_rule_text.as_deref().unwrap_or("");
                let redacted_cand_name = redact_value(cand_name);
                if durable_name != redacted_cand_name {
                    return Err(format!(
                        "Durable record canonical_name '{}' does not match candidate proposed_rule_text '{}' (redacted: '{}')",
                        durable_name, cand_name, redacted_cand_name
                    ));
                }
            }
        }
        _ => unreachable!(),
    }
    let supporting = candidate_fields
        .supporting_evidence
        .as_deref()
        .ok_or_else(|| format!("Candidate '{}' lacks supporting_evidence", candidate_id))?;

    let mut unique_supporting_targets = std::collections::BTreeSet::new();
    let mut sessions = std::collections::BTreeSet::new();
    let mut obs_nodes = Vec::new();
    for link in supporting {
        if link.target_domain != "agent_memory" {
            return Err(format!(
                "Candidate '{}' supporting evidence target domain '{}' is invalid (must be 'agent_memory')",
                candidate_id, link.target_domain
            ));
        }
        if link.relation != "PROPOSED_BY" {
            return Err(format!(
                "Candidate '{}' supporting evidence relation '{}' is invalid (must be 'PROPOSED_BY')",
                candidate_id, link.relation
            ));
        }
        let conf_val: f64 = link.confidence.parse().map_err(|_| {
            format!(
                "Candidate '{}' supporting evidence link confidence '{}' must be a numeric float string",
                candidate_id, link.confidence
            )
        })?;
        if !(0.0..=1.0).contains(&conf_val) {
            return Err(format!(
                "Candidate '{}' supporting evidence link confidence '{}' must be in the range [0.0, 1.0]",
                candidate_id, link.confidence
            ));
        }
        let obs_id = link.target_record_id.as_deref().ok_or_else(|| {
            format!(
                "Candidate '{}' supporting evidence link lacks target_record_id",
                candidate_id
            )
        })?;
        let obs = records.iter().rfind(|r| r.id() == obs_id).ok_or_else(|| {
            format!(
                "Supporting observation '{}' not found for candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        let session_id = match obs {
            GraphRecord::Node {
                kind, session_id, ..
            } => {
                if !matches!(
                    kind,
                    NodeKind::Observation | NodeKind::AgentTurn | NodeKind::Decision
                ) {
                    return Err(format!(
                        "Supporting evidence '{}' has invalid node kind '{:?}' (must be Observation, AgentTurn, or Decision)",
                        obs_id, kind
                    ));
                }
                session_id.as_deref()
            }
            _ => {
                return Err(format!("Supporting evidence '{}' is not a node", obs_id));
            }
        };
        let sess = session_id.ok_or_else(|| {
            format!(
                "supporting_evidence.session_id is required for evidence target '{}' in candidate '{}'",
                obs_id, candidate_id
            )
        })?;
        if sess.is_empty() {
            return Err(format!(
                "supporting_evidence.session_id is required for evidence target '{}' in candidate '{}'",
                obs_id, candidate_id
            ));
        }
        if unique_supporting_targets.insert(obs_id.to_owned()) {
            sessions.insert(sess.to_owned());
            obs_nodes.push(obs);
        }
    }

    if unique_supporting_targets.len() < 3 {
        return Err(format!(
            "Candidate '{}' has {} unique supporting observations; at least 3 are required",
            candidate_id,
            unique_supporting_targets.len()
        ));
    }
    if sessions.len() < 2 {
        return Err(format!(
            "Candidate '{}' has evidence from {} distinct sessions; at least 2 are required",
            candidate_id,
            sessions.len()
        ));
    }

    obs_nodes.sort_by_key(|o| o.id());
    chain.extend(obs_nodes);

    Ok(chain)
}

// ── Memory Evidence Audit Queries (Issue #64) ──────────────────────────────────

/// Error returned when resolving a memory record ID or source/session handle.
///
/// Mirrors [`TaskResolveError`]: `Ambiguous` and `Unsupported` are the two
/// machine-readable, non-network failure modes. "Missing" (no match) and
/// "stale" (tombstoned) handles are surfaced by the CLI layer as `no_match` /
/// `stale_handle` envelopes, since both are about store state rather than the
/// handle's syntax.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum MemoryResolveError {
    /// The handle resolves to more than one distinct memory claim.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The list of matched memory record IDs (canonical-sorted).
        candidates: Vec<String>,
    },
    /// The handle is empty or a malformed canonical agent-memory ID.
    Unsupported {
        /// The query handle.
        handle: String,
        /// Why the handle is unsupported.
        message: String,
    },
}

/// One stable, machine-readable diagnostic emitted by a memory audit.
///
/// Every diagnostic carries the original source handle so an operator can
/// follow it without the audit inferring a replacement (AC6).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MemoryAuditDiagnostic {
    /// Stable diagnostic code (e.g. `unresolved_evidence_link`).
    pub code: String,
    /// Record ID of the node that carries the issue.
    pub source_record_id: String,
    /// Original handle (record ID, path, or hash) — never an inferred value.
    pub target_handle: String,
    /// Relation that produced the handle, when applicable.
    pub relation: String,
    /// Target domain string, when applicable.
    pub target_domain: String,
}

/// One evidence record reached from the audited memory claim, with the
/// relation (edge label / evidence-link relation) that connected it.
#[derive(Debug, Clone)]
pub struct MemoryEvidenceItem<'a> {
    /// The reached graph record.
    pub record: &'a GraphRecord,
    /// The relation that connected it to the claim (e.g. `CONTRADICTS`).
    pub relation: String,
}

/// Structured memory evidence audit returned by [`memory_audit_context`].
///
/// Sections keep trust classes separate so an agent-authored claim is never
/// presented as source truth or proof by itself (AC3). Every section is
/// canonically ordered by record ID for determinism (AC8).
#[derive(Debug, Default, Clone)]
pub struct MemoryAuditContext<'a> {
    /// The queried memory record ID.
    pub memory_id: String,
    /// The agent-authored claim node(s) under audit.
    pub memory_claim: Vec<&'a GraphRecord>,
    /// `AgentSession` provenance node(s) linked via `AUTHORED_BY`.
    pub agent_sessions: Vec<&'a GraphRecord>,
    /// `Agent` provenance node(s) linked via `SESSION_OF`.
    pub agents: Vec<&'a GraphRecord>,
    /// Supporting evidence that is neither code, project, nor verification
    /// (artifacts, command evidence, other supporting memory).
    pub supporting_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Records connected to the claim via `CONTRADICTS` (either direction).
    pub contradicting_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Records that supersede the claim (`SUPERSEDES` / `superseded_by`).
    pub superseding_records: Vec<MemoryEvidenceItem<'a>>,
    /// Code-graph `File` / `Symbol` handles cited by the claim.
    pub related_code_handles: Vec<MemoryEvidenceItem<'a>>,
    /// Project-domain `Task` / `AcceptanceCriterion` handles cited by the claim.
    pub related_project_handles: Vec<MemoryEvidenceItem<'a>>,
    /// Verification-domain evidence cited by the claim.
    pub verification_evidence: Vec<MemoryEvidenceItem<'a>>,
    /// Stable diagnostics (unresolved links, triple-only targets, etc.).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// Records excluded by `--verified-only`, reported rather than dropped (AC5).
    pub excluded: Vec<MemoryEvidenceItem<'a>>,
}

impl MemoryAuditContext<'_> {
    /// Returns `true` when no claim matching the queried ID exists in the store.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.memory_claim.is_empty()
    }
}

const fn is_verification_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Verification
            | NodeKind::CommandEvidence
            | NodeKind::TestRun
            | NodeKind::CommandRun
            | NodeKind::CIStatus
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
            | NodeKind::ProofResult
    )
}

const fn is_codegraph_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::File
            | NodeKind::Symbol
            | NodeKind::Module
            | NodeKind::Import
            | NodeKind::Commit
            | NodeKind::Change
            | NodeKind::Repository
    )
}

const fn is_project_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Task
            | NodeKind::AcceptanceCriterion
            | NodeKind::LocalTask
            | NodeKind::GitHubIssue
            | NodeKind::PR
            | NodeKind::Review
            | NodeKind::ExternalLink
            | NodeKind::Product
            | NodeKind::Project
            | NodeKind::Plan
    )
}

/// An agent-authored claim shape eligible for the memory audit subject and for
/// the unverified-observation exclusion filter.
const fn is_agent_claim_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Observation | NodeKind::Decision | NodeKind::Failure
    )
}

const fn record_node_kind(record: &GraphRecord) -> Option<NodeKind> {
    match record {
        GraphRecord::Node { kind, .. } => Some(*kind),
        _ => None,
    }
}

/// A claim is **verified** when it cites at least one present verification-domain
/// record through an evidence link (`VALIDATED_BY`, `HAS_EVIDENCE`,
/// `PRODUCED_EVIDENCE`) or an equivalent outgoing edge. This is a structural,
/// non-inferential rule over existing contracts — not a truth judgement.
fn is_verified_claim(
    record: &GraphRecord,
    by_id: &BTreeMap<&str, &GraphRecord>,
    edges_from: &BTreeMap<&str, Vec<(&EdgeLabel, &str)>>,
    tombstoned: &BTreeSet<&str>,
) -> bool {
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = record
    {
        for link in links {
            // A backing relation is required; a generic link (e.g. RELATES_TO)
            // that merely happens to point at a verification record does not make
            // the claim verified. Tombstoned targets are treated as absent.
            let backed = matches!(
                link.relation.as_str(),
                "VALIDATED_BY" | "HAS_EVIDENCE" | "PRODUCED_EVIDENCE"
            );
            if backed
                && let Some(target_id) = link.target_record_id.as_deref()
                && !tombstoned.contains(target_id)
                && let Some(target) = by_id.get(target_id)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                return true;
            }
        }
    }
    if let Some(out) = edges_from.get(record.id()) {
        for (label, target) in out {
            let backed = matches!(
                label,
                EdgeLabel::ValidatedBy | EdgeLabel::HasEvidence | EdgeLabel::ProducedEvidence
            );
            if backed
                && !tombstoned.contains(*target)
                && let Some(target) = by_id.get(*target)
                && record_node_kind(target).is_some_and(is_verification_kind)
            {
                return true;
            }
        }
    }
    false
}

/// Pushes an `unresolved_evidence_link` (absent) or `stale_evidence_target`
/// (tombstoned) diagnostic for an edge whose target is not live.
fn push_missing_target(
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
    memory_id: &str,
    target: &str,
    tombstoned: &BTreeSet<&str>,
    relation: &str,
) {
    let code = if tombstoned.contains(target) {
        "stale_evidence_target"
    } else {
        "unresolved_evidence_link"
    };
    diagnostics.push(MemoryAuditDiagnostic {
        code: code.to_owned(),
        source_record_id: memory_id.to_owned(),
        target_handle: target.to_owned(),
        relation: relation.to_owned(),
        target_domain: String::new(),
    });
}

/// The outcome of resolving a memory handle to live claim IDs.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct MemoryResolution {
    /// Live (non-tombstoned) claim IDs the handle resolved to.
    pub matched: BTreeSet<String>,
    /// True when the handle matched at least one claim but every match was
    /// tombstoned (deleted), so the caller should report `stale_handle` rather
    /// than `no_match`.
    pub tombstoned_only: bool,
}

/// Resolves a memory record ID or source/session handle to live claim IDs.
///
/// Supported handle types (AC2):
/// 1. A canonical memory record ID (`agent_memory:v1:<64-hex>`). When it names
///    an `AgentSession` or `Agent`, it is a scope handle resolving to the claims
///    authored in that scope; a scope with more than one claim returns
///    `Ambiguous` with the candidate IDs (one audit covers one claim).
/// 2. A source artifact / session handle: a string matching a claim node's
///    `source_handle`, `source_artifact_path`, `source_artifact_hash`, or
///    `session_id`.
///
/// Tombstoned (deleted) claims are excluded from `matched` so they neither make
/// a live handle ambiguous nor get audited as current state. When every match
/// was tombstoned, `tombstoned_only` is set so the caller can report
/// `stale_handle` even for a source/session handle (whose text is not the
/// deleted record ID).
///
/// # Errors
///
/// Returns [`MemoryResolveError::Unsupported`] for an empty or malformed
/// canonical ID, and [`MemoryResolveError::Ambiguous`] when the handle resolves
/// to more than one distinct live claim.
#[allow(clippy::too_many_lines)]
pub fn resolve_memory_ids(
    records: &[GraphRecord],
    handle: &str,
) -> Result<MemoryResolution, MemoryResolveError> {
    if handle.is_empty() {
        return Err(MemoryResolveError::Unsupported {
            handle: handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let mut matched: BTreeSet<String> = BTreeSet::new();

    if let Some(rest) = handle.strip_prefix("agent_memory:v1:") {
        let is_valid = rest.len() == 64 && rest.chars().all(|c| c.is_ascii_hexdigit());
        if !is_valid {
            return Err(MemoryResolveError::Unsupported {
                handle: handle.to_owned(),
                message: "malformed canonical agent-memory ID".to_owned(),
            });
        }
        // Find the node carrying this ID.
        let mut subject_kind = None;
        let mut subject_name = None;
        let mut subject_session_id = None;
        let mut subject_agent_id = None;
        for r in records {
            if let GraphRecord::Node {
                id,
                kind,
                name,
                session_id,
                agent_id,
                ..
            } = r
                && id == handle
            {
                subject_kind = Some(*kind);
                subject_name.clone_from(name);
                subject_session_id.clone_from(session_id);
                subject_agent_id.clone_from(agent_id);
            }
        }
        match subject_kind {
            Some(kind) if is_agent_claim_kind(kind) => {
                matched.insert(handle.to_owned());
            }
            Some(NodeKind::AgentSession) => {
                // Prefer the session node's `session_id` field; imported session
                // nodes (e.g. Codex) keep a human summary in `name` while claims
                // store the real key in `session_id`. Fall back to `name`.
                let session_key = subject_session_id.as_deref().or(subject_name.as_deref());
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        session_id: Some(sid),
                        ..
                    } = r
                        && is_agent_claim_kind(*kind)
                        && Some(sid.as_str()) == session_key
                    {
                        matched.insert(id.clone());
                    }
                }
            }
            Some(NodeKind::Agent) => {
                // Prefer the agent node's `agent_id` field; fall back to `name`.
                let agent_key = subject_agent_id.as_deref().or(subject_name.as_deref());
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        agent_id: Some(aid),
                        ..
                    } = r
                        && is_agent_claim_kind(*kind)
                        && Some(aid.as_str()) == agent_key
                    {
                        matched.insert(id.clone());
                    }
                }
            }
            // Present but not an auditable claim, or absent entirely: leave the
            // set empty so the caller emits a `no_match` envelope.
            _ => {}
        }
    } else {
        // Source artifact / session handle.
        for r in records {
            if let GraphRecord::Node {
                id,
                kind,
                session_id,
                source_handle,
                source_artifact_path,
                source_artifact_hash,
                ..
            } = r
                && is_agent_claim_kind(*kind)
                && (source_handle.as_deref() == Some(handle)
                    || source_artifact_path.as_deref() == Some(handle)
                    || source_artifact_hash.as_deref() == Some(handle)
                    || session_id.as_deref() == Some(handle))
            {
                matched.insert(id.clone());
            }
        }
    }

    // Tombstoned (deleted) claims are not part of the current state, so they must
    // not make a live handle ambiguous. Drop them before counting, but remember
    // whether the handle matched anything at all so a handle that pointed only at
    // deleted claims is reported as `stale_handle`, not `no_match`.
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();

    // If the queried canonical ID is itself tombstoned — a deleted claim, or a
    // deleted Agent/AgentSession scope node — the handle names a deleted record
    // and is stale, regardless of whether live claims share its scope key.
    if tombstoned.contains(handle) {
        return Ok(MemoryResolution {
            matched: BTreeSet::new(),
            tombstoned_only: true,
        });
    }

    let had_any_match = !matched.is_empty();
    matched.retain(|id| !tombstoned.contains(id.as_str()));
    let tombstoned_only = had_any_match && matched.is_empty();

    // A single audit covers one claim. A scope handle (Agent / AgentSession ID,
    // or a session_id shared by several claims) that resolves to more than one
    // claim is reported as a stable `Ambiguous` diagnostic listing the candidate
    // claim IDs, so the operator can re-query a specific one. This keeps the
    // single-claim audit contract honest rather than silently picking one.
    if matched.len() > 1 {
        return Err(MemoryResolveError::Ambiguous {
            handle: handle.to_owned(),
            candidates: matched.into_iter().collect(),
        });
    }

    Ok(MemoryResolution {
        matched,
        tombstoned_only,
    })
}

/// Section a reached evidence record belongs to.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum AuditSection {
    Supporting,
    Code,
    Project,
    Verification,
}

const fn classify_audit_target(kind: NodeKind) -> AuditSection {
    if is_verification_kind(kind) {
        AuditSection::Verification
    } else if is_codegraph_kind(kind) {
        AuditSection::Code
    } else if is_project_kind(kind) {
        AuditSection::Project
    } else {
        AuditSection::Supporting
    }
}

/// Builds the evidence audit for a resolved memory claim ID.
///
/// The traversal reads only existing contracts (evidence links + cross-domain
/// edges) and never reads raw transcript bodies or infers replacements for
/// missing handles (AC6, AC11). When `verified_only` is set, unverified
/// agent-authored records are moved out of their sections into `excluded`
/// rather than silently dropped (AC5).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn memory_audit_context<'a>(
    records: &'a [GraphRecord],
    memory_id: &str,
    verified_only: bool,
) -> MemoryAuditContext<'a> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // Outgoing edges keyed by source ID, for verification detection.
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = r
        {
            edges_from
                .entry(source.as_str())
                .or_default()
                .push((label, target.as_str()));
        }
    }

    // Tombstoned IDs are deleted for current-state reads: treat their nodes as
    // absent everywhere in the traversal so a deleted evidence/provenance record
    // is never surfaced as live support (AC6).
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if tombstoned.contains(id) {
            None
        } else {
            by_id.get(id).copied()
        }
    };

    let claim_nodes: Vec<&GraphRecord> = records
        .iter()
        .filter(|r| matches!(r, GraphRecord::Node { id, .. } if id == memory_id))
        .collect();

    let mut ctx = MemoryAuditContext {
        memory_id: memory_id.to_owned(),
        memory_claim: claim_nodes.clone(),
        ..Default::default()
    };

    if claim_nodes.is_empty() {
        return ctx;
    }

    // Dedupe maps keyed by record ID; relation is the first one observed.
    let mut supporting: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut contradicting: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut superseding: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut code: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut project: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut verification: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut sessions: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut agents: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    let mut place = |target: &'a GraphRecord, relation: &str| {
        let Some(kind) = record_node_kind(target) else {
            return;
        };
        let item = MemoryEvidenceItem {
            record: target,
            relation: relation.to_owned(),
        };
        match classify_audit_target(kind) {
            AuditSection::Verification => {
                verification.entry(target.id()).or_insert(item);
            }
            AuditSection::Code => {
                code.entry(target.id()).or_insert(item);
            }
            AuditSection::Project => {
                project.entry(target.id()).or_insert(item);
            }
            AuditSection::Supporting => {
                supporting.entry(target.id()).or_insert(item);
            }
        }
    };

    for claim in &claim_nodes {
        // 1) Denormalized evidence links on the claim node.
        if let GraphRecord::Node {
            evidence_links: Some(links),
            ..
        } = claim
        {
            for link in links {
                let Some(target_id) = link.target_record_id.as_deref() else {
                    // Triple-only target: do not resolve heuristically (AC6).
                    let handle = link
                        .target_repo_relative_path
                        .clone()
                        .unwrap_or_else(|| "<triple>".to_owned());
                    diagnostics.push(MemoryAuditDiagnostic {
                        code: "evidence_target_unresolved".to_owned(),
                        source_record_id: memory_id.to_owned(),
                        target_handle: handle,
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                    continue;
                };
                // A tombstoned target is stale for current-state reads; an absent
                // target is unresolved. Either way it is surfaced, not placed.
                let code = if tombstoned.contains(target_id) {
                    "stale_evidence_target"
                } else if by_id.contains_key(target_id) {
                    ""
                } else {
                    "unresolved_evidence_link"
                };
                if !code.is_empty() {
                    diagnostics.push(MemoryAuditDiagnostic {
                        code: code.to_owned(),
                        source_record_id: memory_id.to_owned(),
                        target_handle: target_id.to_owned(),
                        relation: link.relation.clone(),
                        target_domain: link.target_domain.clone(),
                    });
                    continue;
                }
                let Some(target) = present(target_id) else {
                    continue;
                };
                // A denormalized `CONTRADICTS` evidence link must reach the
                // contradicting section, same as a `CONTRADICTS` graph edge,
                // so a record that retained only the denormalized link does not
                // mis-report a contradiction as generic support.
                if link.relation == "CONTRADICTS" {
                    contradicting
                        .entry(target.id())
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: target,
                            relation: "CONTRADICTS".to_owned(),
                        });
                } else {
                    place(target, &link.relation);
                }
            }
        }

        // 2) Supersession declared inline on the claim — read even when the
        // claim carries no `evidence_links` (a common stale-claim shape).
        if let GraphRecord::Node {
            superseded_by: Some(sup_id),
            ..
        } = claim
        {
            if let Some(target) = present(sup_id) {
                superseding
                    .entry(target.id())
                    .or_insert_with(|| MemoryEvidenceItem {
                        record: target,
                        relation: "SUPERSEDED_BY".to_owned(),
                    });
            } else {
                let code = if tombstoned.contains(sup_id.as_str()) {
                    "stale_evidence_target"
                } else {
                    "unresolved_evidence_link"
                };
                diagnostics.push(MemoryAuditDiagnostic {
                    code: code.to_owned(),
                    source_record_id: memory_id.to_owned(),
                    target_handle: sup_id.clone(),
                    relation: "SUPERSEDED_BY".to_owned(),
                    target_domain: "agent_memory".to_owned(),
                });
            }
        }

        // 3) Edges touching the claim node.
        for r in records {
            let GraphRecord::Edge {
                label,
                source,
                target,
                ..
            } = r
            else {
                continue;
            };
            let claim_id = claim.id();
            if source == claim_id {
                // AUTHORED_BY provenance is resolved by the chain walk below;
                // code-internal and other labels are not claim evidence.
                let is_evidence = matches!(
                    label,
                    EdgeLabel::Contradicts
                        | EdgeLabel::HasEvidence
                        | EdgeLabel::ValidatedBy
                        | EdgeLabel::Observes
                        | EdgeLabel::MentionsSymbol
                        | EdgeLabel::TouchedFile
                        | EdgeLabel::FailedOn
                        | EdgeLabel::ReferencesTask
                        | EdgeLabel::ExplainsChange
                        | EdgeLabel::ProducedPatch
                        | EdgeLabel::ProducedEvidence
                        | EdgeLabel::RelatesTo
                );
                if !is_evidence {
                    continue;
                }
                let Some(other) = present(target) else {
                    // Edge-only evidence whose target is absent or tombstoned is
                    // surfaced as a diagnostic, like a denormalized link, instead
                    // of silently disappearing.
                    push_missing_target(
                        &mut diagnostics,
                        memory_id,
                        target,
                        &tombstoned,
                        label.as_str(),
                    );
                    continue;
                };
                if matches!(label, EdgeLabel::Contradicts) {
                    contradicting
                        .entry(other.id())
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: other,
                            relation: "CONTRADICTS".to_owned(),
                        });
                } else {
                    place(other, label.as_str());
                }
            } else if target == claim_id {
                if !matches!(label, EdgeLabel::Contradicts | EdgeLabel::Supersedes) {
                    continue;
                }
                let Some(other) = present(source) else {
                    push_missing_target(
                        &mut diagnostics,
                        memory_id,
                        source,
                        &tombstoned,
                        label.as_str(),
                    );
                    continue;
                };
                match label {
                    EdgeLabel::Contradicts => {
                        contradicting
                            .entry(other.id())
                            .or_insert_with(|| MemoryEvidenceItem {
                                record: other,
                                relation: "CONTRADICTS".to_owned(),
                            });
                    }
                    EdgeLabel::Supersedes => {
                        superseding
                            .entry(other.id())
                            .or_insert_with(|| MemoryEvidenceItem {
                                record: other,
                                relation: "SUPERSEDES".to_owned(),
                            });
                    }
                    _ => {}
                }
            }
        }
    }

    // Reverse denormalized links: a newer record may declare the relationship on
    // itself (`SUPERSEDES`/`CONTRADICTS` -> audited claim) without a retained
    // edge. Scan live records' evidence_links for entries targeting the claim so
    // the superseding/contradicting record is not omitted in edge-stripped stores.
    for r in records {
        let GraphRecord::Node {
            id: src_id,
            evidence_links: Some(links),
            ..
        } = r
        else {
            continue;
        };
        if src_id == memory_id || tombstoned.contains(src_id.as_str()) {
            continue;
        }
        for link in links {
            if link.target_record_id.as_deref() != Some(memory_id) {
                continue;
            }
            match link.relation.as_str() {
                "SUPERSEDES" => {
                    superseding
                        .entry(src_id)
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: r,
                            relation: "SUPERSEDES".to_owned(),
                        });
                }
                "CONTRADICTS" => {
                    contradicting
                        .entry(src_id)
                        .or_insert_with(|| MemoryEvidenceItem {
                            record: r,
                            relation: "CONTRADICTS".to_owned(),
                        });
                }
                _ => {}
            }
        }
    }

    // Resolve session/agent provenance by walking the AUTHORED_BY / SESSION_OF
    // chain from each claim. Imported records commonly route a claim through an
    // AgentTurn and AgentRun before reaching the AgentSession, with SESSION_OF on
    // the session→agent edge, so the immediate AUTHORED_BY target is not always
    // the session. The walk classifies every reached node by kind; intermediate
    // turns/runs are traversed but never labelled as a session.
    {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut frontier: Vec<&str> = claim_nodes.iter().map(|c| c.id()).collect();
        while let Some(id) = frontier.pop() {
            if !visited.insert(id) {
                continue;
            }
            let Some(out) = edges_from.get(id) else {
                continue;
            };
            for (label, target) in out {
                if !matches!(label, EdgeLabel::AuthoredBy | EdgeLabel::SessionOf) {
                    continue;
                }
                if !visited.contains(*target) {
                    frontier.push(*target);
                }
                if let Some(node) = present(target) {
                    match record_node_kind(node) {
                        Some(NodeKind::AgentSession) => {
                            sessions.entry(node.id()).or_insert(node);
                        }
                        Some(NodeKind::Agent) => {
                            agents.entry(node.id()).or_insert(node);
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    // verified-only filter: move unverified agent-authored records to `excluded`.
    let mut excluded: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    if verified_only {
        for map in [&mut contradicting, &mut superseding, &mut supporting] {
            let drop_ids: Vec<&str> = map
                .iter()
                .filter(|(_, item)| {
                    record_node_kind(item.record).is_some_and(is_agent_claim_kind)
                        && !is_verified_claim(item.record, &by_id, &edges_from, &tombstoned)
                })
                .map(|(id, _)| *id)
                .collect();
            for id in drop_ids {
                if let Some(item) = map.remove(id) {
                    excluded.entry(id).or_insert(item);
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup();

    ctx.agent_sessions = sessions.into_values().collect();
    ctx.agents = agents.into_values().collect();
    ctx.supporting_evidence = supporting.into_values().collect();
    ctx.contradicting_evidence = contradicting.into_values().collect();
    ctx.superseding_records = superseding.into_values().collect();
    ctx.related_code_handles = code.into_values().collect();
    ctx.related_project_handles = project.into_values().collect();
    ctx.verification_evidence = verification.into_values().collect();
    ctx.excluded = excluded.into_values().collect();
    ctx.diagnostics = diagnostics;
    ctx
}

// ── Failure-History Queries (Issue #63) ─────────────────────────────────────
//
// Answer the operator-visible question "what failed here before, and what
// evidence proves that failure happened?" — starting from a code or task handle
// and returning prior FAILED attempts as citable local facts. Runtime
// command/test/CI failures (verification domain) stay separate from
// agent-authored `Failure` claims so neither is presented as source truth, and a
// later passing verification on the same handle is surfaced as a separate
// superseding item rather than hiding the older failure (AC4, AC5). This slice
// reuses existing agent-memory, verification, artifact, project, redaction, and
// evidence-link contracts; it introduces no new graph domain, node kind, or edge
// vocabulary (AC10).

/// Which handle type a failure-history query resolved from.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FailureTargetKind {
    /// A code symbol (record ID or exact name).
    Symbol,
    /// A repo-relative file path.
    File,
    /// A task handle (canonical ID, GitHub handle, or local JSONL handle).
    Task,
    /// A source/provenance handle naming failures directly.
    Source,
}

impl FailureTargetKind {
    /// Stable wire string for the resolved handle type.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::Task => "task",
            Self::Source => "source",
        }
    }
}

/// A resolved failure-history target.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ResolvedFailureTarget {
    /// Original handle as provided by the operator.
    pub handle: String,
    /// Which handle type matched.
    pub kind: FailureTargetKind,
    /// Live code/task record IDs to traverse inbound from. Empty for `Source`.
    pub anchor_ids: BTreeSet<String>,
    /// Failure/verification record IDs matched directly by a source handle.
    pub seed_failures: BTreeSet<String>,
    /// True when the handle named a record that exists only as a tombstone.
    pub stale: bool,
}

impl ResolvedFailureTarget {
    /// Returns true when the handle resolved to nothing live in the store, so
    /// the caller emits a `no_match` (or `stale_handle`) envelope (AC6).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.anchor_ids.is_empty() && self.seed_failures.is_empty()
    }
}

/// Error returned when resolving a failure-history handle (AC2).
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FailureHandleError {
    /// The handle matched targets in more than one repository without `--repo`.
    Ambiguous {
        /// The query handle.
        handle: String,
        /// The candidate record IDs the handle resolved to.
        candidates: Vec<String>,
    },
    /// The handle is malformed (empty or a malformed canonical ID).
    Unsupported {
        /// The query handle.
        handle: String,
        /// Why the handle is unsupported.
        message: String,
    },
}

/// Read-time status of one failed attempt relative to the queried target.
///
/// `SinceResolved` means a later passing verification exists on a shared target
/// handle; `StillFailing` is the conservative default whenever supersession
/// cannot be proven (including missing or unparseable timestamps). The failed
/// attempt is never deleted, hidden, or rewritten — this is a purely additive
/// read-time annotation (AC5).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ResolutionStatus {
    /// No later passing verification supersedes this failure on a shared target.
    StillFailing,
    /// A later passing verification on a shared target supersedes this failure.
    SinceResolved,
}

impl ResolutionStatus {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StillFailing => "still_failing",
            Self::SinceResolved => "since_resolved",
        }
    }
}

/// One prior failed attempt reached by inbound traversal, with its read-time
/// resolution status and the passing verification (if any) that resolved it.
#[derive(Debug, Clone)]
pub struct FailureAttempt<'a> {
    /// The reached failure record plus the relation that connected it.
    pub item: MemoryEvidenceItem<'a>,
    /// Read-time `still_failing` / `since_resolved` status (AC5).
    pub status: ResolutionStatus,
    /// Record ID of the later passing verification that resolved it, if any.
    pub resolved_by: Option<&'a str>,
    /// The target handle (anchor record ID) this attempt linked to.
    pub matched_target: &'a str,
}

/// Structured prior-failed-attempt context returned by [`failure_history_context`].
///
/// Sections keep runtime failure evidence separate from agent-authored failure
/// claims (AC4); every vector is canonically ordered for determinism (AC7).
#[derive(Debug, Default, Clone)]
pub struct FailureHistoryContext<'a> {
    /// Resolved handle type (`symbol`/`file`/`task`/`source`).
    pub target_kind: &'static str,
    /// Resolved code/task anchor record IDs, canonically sorted.
    pub target_ids: Vec<String>,
    /// Runtime command/test/CI failures (verification domain, status
    /// fail/error/timeout) — trust class `verification_evidence`.
    pub runtime_failures: Vec<FailureAttempt<'a>>,
    /// Agent-authored `Failure` claims — trust class `agent_authored`.
    pub agent_failures: Vec<FailureAttempt<'a>>,
    /// Later PASSING verifications on a shared target — a separate contrasting
    /// section that never hides the older failures (AC5).
    pub superseding_successes: Vec<MemoryEvidenceItem<'a>>,
    /// Patch artifacts produced by reached failures (1 hop, `PRODUCED_PATCH`).
    pub patch_artifacts: Vec<MemoryEvidenceItem<'a>>,
    /// `AgentSession` provenance for reached agent failures.
    pub agent_sessions: Vec<&'a GraphRecord>,
    /// `Agent` provenance for reached agent failures.
    pub agents: Vec<&'a GraphRecord>,
    /// Stable diagnostics (unresolved links, stale targets, missing timestamps).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
}

impl FailureHistoryContext<'_> {
    /// Returns true when the resolved target has no recorded failures. This is a
    /// real (exit-0, `ok:true`) empty answer, not a handle no-match.
    #[must_use]
    pub const fn has_no_failures(&self) -> bool {
        self.runtime_failures.is_empty() && self.agent_failures.is_empty()
    }
}

/// Verification statuses that count as a failed runtime attempt.
fn is_failed_status(status: Option<&str>) -> bool {
    matches!(status, Some("fail" | "error" | "timeout"))
}

/// Verification statuses that count as a passing runtime success.
fn is_pass_status(status: Option<&str>) -> bool {
    matches!(status, Some("pass"))
}

/// A reached verification/failure record with its accumulated anchor set and the
/// relation it was first reached through. Keyed by record ID for dedup + order.
type CandidateMap<'a> = BTreeMap<&'a str, (&'a GraphRecord, BTreeSet<&'a str>, &'a str)>;

/// True when a verification record is a failed runtime attempt. Importers emit
/// `CommandRun` nodes with an `exit_code` and no `status`, so a nonzero exit code
/// is consulted as a fallback when `status` is absent (issue #63 review).
fn is_failed_verification(node: &GraphRecord) -> bool {
    let GraphRecord::Node {
        status, exit_code, ..
    } = node
    else {
        return false;
    };
    is_failed_status(status.as_deref())
        || (status.is_none() && matches!(exit_code, Some(c) if *c != 0))
}

/// True for a verification record whose status is `pass`, or — symmetric with
/// [`is_failed_verification`] — a status-absent `CommandRun` with a zero exit
/// code, so a later successful command can supersede a prior failure.
fn is_pass_status_node(node: &GraphRecord) -> bool {
    let GraphRecord::Node {
        status, exit_code, ..
    } = node
    else {
        return false;
    };
    is_pass_status(status.as_deref()) || (status.is_none() && *exit_code == Some(0))
}

/// Merges a reached candidate into a classification map, unioning anchor sets
/// when the same record is reached through more than one target.
fn merge_candidate<'a>(
    map: &mut CandidateMap<'a>,
    node: &'a GraphRecord,
    anchors: &BTreeSet<&'a str>,
    rel: &'a str,
) {
    let entry = map
        .entry(node.id())
        .or_insert_with(|| (node, BTreeSet::new(), rel));
    entry.1.extend(anchors.iter().copied());
}

/// Routes a reached record into the agent-failure, runtime-failure, or passing-
/// success classification map by kind and status.
fn route_candidate<'a>(
    node: &'a GraphRecord,
    anchors: &BTreeSet<&'a str>,
    rel: &'a str,
    agent: &mut CandidateMap<'a>,
    runtime: &mut CandidateMap<'a>,
    success: &mut CandidateMap<'a>,
) {
    match record_node_kind(node) {
        Some(NodeKind::Failure) => merge_candidate(agent, node, anchors, rel),
        Some(k) if is_verification_kind(k) => {
            if is_failed_verification(node) {
                merge_candidate(runtime, node, anchors, rel);
            } else if is_pass_status_node(node) {
                merge_candidate(success, node, anchors, rel);
            }
        }
        _ => {}
    }
}

/// Cross-domain relations that connect a failure/verification record to a code
/// or task target. A record reaching a target through one of these is a
/// candidate prior attempt on that target.
const FAILURE_TARGET_LINK_RELS: &[&str] = &[
    "FAILED_ON",
    "TOUCHED_FILE",
    "MENTIONS_SYMBOL",
    "OBSERVES",
    "REFERENCES_TASK",
    "PRODUCED_EVIDENCE",
    "HAS_EVIDENCE",
    "VALIDATED_BY",
];

/// Parses a node's wall-clock instant (`executed_at` preferred, else
/// `observed_at`) as an RFC-3339 timestamp. Returns `None` for non-nodes or
/// unparseable/absent timestamps so the caller stays conservative (AC5/AC7).
fn node_instant(record: &GraphRecord) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let GraphRecord::Node {
        executed_at,
        observed_at,
        ..
    } = record
    else {
        return None;
    };
    let raw = executed_at.as_deref().or(observed_at.as_deref())?;
    chrono::DateTime::parse_from_rfc3339(raw).ok()
}

/// Resolves a code or task handle to the target record IDs a failure-history
/// query traverses inbound from (AC2).
///
/// Resolution is structural — it never falls back to transcript text search
/// (AC6). The attempt order is: canonical code record ID, then task / task-source
/// handle (reusing [`resolve_task_ids`]), then repo-relative file path, then
/// exact symbol name, then a source/provenance handle naming failures directly.
///
/// `repo_scope`, when set, restricts file/symbol resolution to one repository;
/// without it, a file path or symbol name matching targets in more than one
/// repository is reported as `Ambiguous` rather than resolved implicitly.
///
/// # Errors
///
/// Returns [`FailureHandleError::Unsupported`] for an empty handle or a malformed
/// canonical task ID, and [`FailureHandleError::Ambiguous`] for a cross-repository
/// collision.
#[allow(clippy::too_many_lines)]
pub fn resolve_failure_handle(
    records: &[GraphRecord],
    handle: &str,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<ResolvedFailureTarget, FailureHandleError> {
    if handle.is_empty() {
        return Err(FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }

    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let in_scope = |id: &str| -> bool {
        repo_scope.is_none_or(|scope| repo_index.owner_of(id) == Some(scope))
    };

    let empty_target = |kind: FailureTargetKind, stale: bool| ResolvedFailureTarget {
        handle: handle.to_owned(),
        kind,
        anchor_ids: BTreeSet::new(),
        seed_failures: BTreeSet::new(),
        stale,
    };

    // 1) Canonical code record ID (codegraph:vN:<hex>). A malformed canonical ID
    //    is unsupported (exit 1); a well-formed but absent or out-of-scope ID
    //    resolves to nothing (caller emits no_match).
    if handle.starts_with("codegraph:") {
        let parts: Vec<&str> = handle.split(':').collect();
        let well_formed = parts.len() == 3
            && parts[0] == "codegraph"
            && parts[1].starts_with('v')
            && parts[1].len() > 1
            && parts[1][1..].chars().all(|c| c.is_ascii_digit())
            && parts[2].len() == 64
            && parts[2].chars().all(|c| c.is_ascii_hexdigit());
        if !well_formed {
            return Err(FailureHandleError::Unsupported {
                handle: handle.to_owned(),
                message: "malformed canonical codegraph ID".to_owned(),
            });
        }
        if tombstoned.contains(handle) {
            return Ok(empty_target(FailureTargetKind::Symbol, true));
        }
        for r in records {
            if let GraphRecord::Node { id, kind, .. } = r
                && id == handle
                && is_codegraph_kind(*kind)
                && in_scope(handle)
            {
                let kind = if matches!(kind, NodeKind::File) {
                    FailureTargetKind::File
                } else {
                    FailureTargetKind::Symbol
                };
                let mut anchor_ids = BTreeSet::new();
                anchor_ids.insert(handle.to_owned());
                return Ok(ResolvedFailureTarget {
                    handle: handle.to_owned(),
                    kind,
                    anchor_ids,
                    seed_failures: BTreeSet::new(),
                    stale: false,
                });
            }
        }
        return Ok(empty_target(FailureTargetKind::Symbol, false));
    }

    // 2) Task / task-source handle — reuse the task resolver verbatim.
    match resolve_task_ids(records, handle) {
        Ok(ids) => {
            // Drop tombstoned (deleted) task IDs from the current-state read, the
            // same way code/file/symbol handles are filtered. A handle that named
            // only deleted tasks is stale, not a live target.
            let had_match = !ids.is_empty();
            let live: BTreeSet<String> = ids
                .into_iter()
                .filter(|id| !tombstoned.contains(id.as_str()))
                .collect();
            if !live.is_empty() {
                // Expand to the tasks' acceptance criteria so failures/verifications
                // attached to an `AcceptanceCriterion` are included (mirrors the
                // task-evidence query, which expands tasks to their ACs).
                let mut anchor_ids = live.clone();
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind: NodeKind::AcceptanceCriterion,
                        parent_task_id: Some(parent),
                        ..
                    } = r
                        && live.contains(parent)
                        && !tombstoned.contains(id.as_str())
                    {
                        anchor_ids.insert(id.clone());
                    }
                }
                return Ok(ResolvedFailureTarget {
                    handle: handle.to_owned(),
                    kind: FailureTargetKind::Task,
                    anchor_ids,
                    seed_failures: BTreeSet::new(),
                    stale: false,
                });
            }
            // No live task. Only a canonical `project:` ID definitively names a
            // (now-absent or deleted) task and stops here; a GitHub/JSONL handle
            // may also be a failure's source handle, so fall through to the
            // source/file/symbol steps rather than returning no_match early.
            if handle.starts_with("project:") {
                return Ok(empty_target(
                    FailureTargetKind::Task,
                    had_match || tombstoned.contains(handle),
                ));
            }
        }
        Err(TaskResolveError::Ambiguous {
            handle: h,
            candidates,
        }) => {
            return Err(FailureHandleError::Ambiguous {
                handle: h,
                candidates,
            });
        }
        Err(TaskResolveError::Unsupported { handle: h, message }) => {
            // A malformed canonical task ID is a hard error; an unrecognized
            // format merely means "not a task handle" — fall through.
            if handle.starts_with("project:") {
                return Err(FailureHandleError::Unsupported { handle: h, message });
            }
        }
    }

    // A path/name handle that matches only tombstoned (deleted) records is stale,
    // not a never-seen handle: track that so step 6 reports `stale_handle` rather
    // than `no_match`, the same distinction code/task handles already make.
    let mut saw_tombstoned = false;

    // 3) Repo-relative file path.
    let mut file_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
            && path == handle
            && in_scope(id)
        {
            if tombstoned.contains(id.as_str()) {
                saw_tombstoned = true;
            } else {
                file_matches.insert(id.clone());
            }
        }
    }
    if !file_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&file_matches, repo_index, repo_scope) {
            return Err(FailureHandleError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        return Ok(ResolvedFailureTarget {
            handle: handle.to_owned(),
            kind: FailureTargetKind::File,
            anchor_ids: file_matches,
            seed_failures: BTreeSet::new(),
            stale: false,
        });
    }

    // 4) Exact symbol name. Several symbols of the same name in one repository
    //    form a multi-ID target; the same name across repositories is ambiguous.
    let mut symbol_matches: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: Some(name),
            ..
        } = r
            && name == handle
            && in_scope(id)
        {
            if tombstoned.contains(id.as_str()) {
                saw_tombstoned = true;
            } else {
                symbol_matches.insert(id.clone());
            }
        }
    }
    if !symbol_matches.is_empty() {
        if let Some(candidates) = cross_repo_ambiguity(&symbol_matches, repo_index, repo_scope) {
            return Err(FailureHandleError::Ambiguous {
                handle: handle.to_owned(),
                candidates,
            });
        }
        return Ok(ResolvedFailureTarget {
            handle: handle.to_owned(),
            kind: FailureTargetKind::Symbol,
            anchor_ids: symbol_matches,
            seed_failures: BTreeSet::new(),
            stale: false,
        });
    }

    // 5) Source / provenance handle naming failures directly.
    let mut seeds: BTreeSet<String> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind,
            session_id,
            source_handle,
            source_artifact_path,
            source_artifact_hash,
            ..
        } = r
            && (matches!(kind, NodeKind::Failure) || is_verification_kind(*kind))
            && !tombstoned.contains(id.as_str())
            && (source_handle.as_deref() == Some(handle)
                || source_artifact_path.as_deref() == Some(handle)
                || source_artifact_hash.as_deref() == Some(handle)
                || session_id.as_deref() == Some(handle))
        {
            seeds.insert(id.clone());
        }
    }
    if !seeds.is_empty() {
        return Ok(ResolvedFailureTarget {
            handle: handle.to_owned(),
            kind: FailureTargetKind::Source,
            anchor_ids: BTreeSet::new(),
            seed_failures: seeds,
            stale: false,
        });
    }

    // 6) Nothing matched. A handle that named a tombstoned record — or only
    //    tombstoned path/name matches — is stale; otherwise it is a plain
    //    no-match. The resolver never guesses a replacement (AC6).
    Ok(empty_target(
        FailureTargetKind::Symbol,
        saw_tombstoned || tombstoned.contains(handle),
    ))
}

/// Returns the sorted candidate IDs when `matches` spans more than one
/// repository and no `--repo` scope was given, else `None`.
fn cross_repo_ambiguity(
    matches: &BTreeSet<String>,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Option<Vec<String>> {
    if repo_scope.is_some() {
        return None;
    }
    // Unattributed (legacy) records form their own ambiguity group, matching the
    // repository-scoped query behavior: a handle matching both a repo-owned record
    // and an unattributed one must fail closed rather than silently merge them.
    let owners: BTreeSet<Option<&str>> = matches.iter().map(|id| repo_index.owner_of(id)).collect();
    if owners.len() > 1 {
        Some(matches.iter().cloned().collect())
    } else {
        None
    }
}

/// Builds the prior-failed-attempt context for a resolved target (AC1, AC3-AC7).
///
/// The traversal reads only existing edges and evidence links and never reads
/// raw transcript bodies or infers a failure cause when supporting evidence is
/// absent (AC8, success metric). It is bounded to two hops: failures /
/// verifications linked directly to a target (hop 1), and the patch artifacts
/// and session provenance attached to those failures (hop 2).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn failure_history_context<'a>(
    records: &'a [GraphRecord],
    target: &ResolvedFailureTarget,
) -> FailureHistoryContext<'a> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if tombstoned.contains(id) {
            None
        } else {
            by_id.get(id).copied()
        }
    };

    // Outgoing edges keyed by source (for PRODUCED_PATCH + provenance walk).
    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    // Inbound index: target_id -> sorted (source_id, relation), from both graph
    // edges and denormalized node evidence_links (dual-source robustness).
    let mut inbound: BTreeMap<&str, Vec<(&str, &str)>> = BTreeMap::new();
    for r in records {
        match r {
            GraphRecord::Edge {
                id,
                label,
                source,
                target,
                ..
            } => {
                // Skip retracted edges: a tombstoned `FAILED_ON` / `VALIDATED_BY`
                // / `PRODUCED_PATCH` edge must not surface stale relationships on
                // current-state reads, matching `symbol_context`'s convention.
                if tombstoned.contains(id.as_str()) {
                    continue;
                }
                edges_from
                    .entry(source.as_str())
                    .or_default()
                    .push((label, target.as_str()));
                inbound
                    .entry(target.as_str())
                    .or_default()
                    .push((source.as_str(), label.as_str()));
            }
            GraphRecord::Node {
                id,
                evidence_links: Some(links),
                ..
            } => {
                for link in links {
                    if let Some(t) = link.target_record_id.as_deref() {
                        inbound
                            .entry(t)
                            .or_default()
                            .push((id.as_str(), link.relation.as_str()));
                    }
                }
            }
            _ => {}
        }
    }
    for list in inbound.values_mut() {
        list.sort_unstable();
        list.dedup();
    }

    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    // Anchor universe: the code/task targets to traverse inbound from. For a
    // source handle, derive it from the seed failures' outbound code/task links.
    let mut anchor_universe: BTreeSet<&str> = BTreeSet::new();
    for a in &target.anchor_ids {
        if let Some((id, _)) = by_id.get_key_value(a.as_str()) {
            anchor_universe.insert(id);
        } else {
            let code = if matches!(target.kind, FailureTargetKind::Task) {
                "missing_task_ref"
            } else if tombstoned.contains(a.as_str()) {
                "stale_code_handle"
            } else {
                "missing_code_handle"
            };
            diagnostics.push(MemoryAuditDiagnostic {
                code: code.to_owned(),
                source_record_id: target.handle.clone(),
                target_handle: a.clone(),
                relation: String::new(),
                target_domain: String::new(),
            });
        }
    }
    if matches!(target.kind, FailureTargetKind::Source) {
        for seed in &target.seed_failures {
            if let Some(node) = present(seed) {
                for anchor in outbound_code_task_targets(node, &edges_from, &present) {
                    anchor_universe.insert(anchor);
                }
            }
        }
    }

    // Candidate prior attempts: records linking to any anchor through a relevant
    // relation, plus the forced seed failures of a source handle. Track each
    // candidate's anchor set and the first relation it linked through.
    let mut candidate_anchors: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut candidate_rel: BTreeMap<&str, &str> = BTreeMap::new();
    for anchor in &anchor_universe {
        let Some(srcs) = inbound.get(*anchor) else {
            continue;
        };
        for (src, rel) in srcs {
            if !FAILURE_TARGET_LINK_RELS.contains(rel) {
                continue;
            }
            if present(src).is_none() {
                continue;
            }
            candidate_anchors.entry(src).or_default().insert(anchor);
            candidate_rel.entry(src).or_insert(rel);
        }
    }
    for seed in &target.seed_failures {
        if let Some((id, _)) = by_id.get_key_value(seed.as_str())
            && present(id).is_some()
        {
            candidate_anchors.entry(id).or_default();
            candidate_rel.entry(id).or_insert("SOURCE_HANDLE");
        }
    }
    // `CLOSES_ACCEPTANCE_CRITERION` runs AcceptanceCriterion -> Verification, so
    // the verification that closes an AC is reached by following the AC anchor's
    // OUTBOUND closure edge (or its denormalized `verification_link_id`) rather
    // than an inbound link. Without this, a task whose AC was closed by a passing
    // run would show no superseding success.
    for anchor in &anchor_universe {
        let Some(ac_node) = present(anchor) else {
            continue;
        };
        if !matches!(
            record_node_kind(ac_node),
            Some(NodeKind::AcceptanceCriterion)
        ) {
            continue;
        }
        if let Some(edges) = edges_from.get(*anchor) {
            for (label, t) in edges {
                if matches!(label, EdgeLabel::ClosesAcceptanceCriterion) && present(t).is_some() {
                    candidate_anchors.entry(t).or_default().insert(anchor);
                    candidate_rel
                        .entry(t)
                        .or_insert("CLOSES_ACCEPTANCE_CRITERION");
                }
            }
        }
        // Denormalized form: an AC may carry `verification_link_id` without a
        // synthesized closure edge (project-imported / daemon-written data).
        if let GraphRecord::Node {
            verification_link_id: Some(vid),
            ..
        } = ac_node
            && present(vid).is_some()
        {
            candidate_anchors
                .entry(vid.as_str())
                .or_default()
                .insert(anchor);
            candidate_rel
                .entry(vid.as_str())
                .or_insert("CLOSES_ACCEPTANCE_CRITERION");
        }
    }

    // PatchArtifact relay: `link_evidence` attaches a patch to a File via
    // `TOUCHED_FILE` while the failing attempt is `Failure --FAILED_ON-->
    // PatchArtifact`. A file/symbol query therefore reaches the patch, not the
    // failure; walk each reached patch's inbound `FAILED_ON` edges so those
    // failures enter the candidate set on the same anchor.
    let patch_relays: Vec<(&str, BTreeSet<&str>)> = candidate_anchors
        .iter()
        .filter(|(cid, _)| {
            present(cid)
                .is_some_and(|n| matches!(record_node_kind(n), Some(NodeKind::PatchArtifact)))
        })
        .map(|(cid, anchors)| (*cid, anchors.clone()))
        .collect();
    for (patch_id, anchors) in patch_relays {
        let Some(srcs) = inbound.get(patch_id) else {
            continue;
        };
        for (src, rel) in srcs {
            if *rel != "FAILED_ON"
                || !present(src)
                    .is_some_and(|n| matches!(record_node_kind(n), Some(NodeKind::Failure)))
            {
                continue;
            }
            for a in &anchors {
                candidate_anchors.entry(src).or_default().insert(a);
            }
            candidate_rel.entry(src).or_insert("FAILED_ON");
        }
    }

    // ── Classify candidates into agent failures, runtime failures, and passing
    //    successes, unioning anchor sets when a record is reached more than once. ──
    let source_kind = matches!(target.kind, FailureTargetKind::Source);
    let mut agent: CandidateMap<'a> = BTreeMap::new();
    let mut runtime: CandidateMap<'a> = BTreeMap::new();
    let mut success: CandidateMap<'a> = BTreeMap::new();

    for (cid, anchors) in &candidate_anchors {
        let Some(node) = present(cid) else { continue };
        let rel = candidate_rel.get(cid).copied().unwrap_or("RELATES_TO");
        // A source/provenance handle names failures directly: only the matched
        // seeds are prior attempts. Anchor-linked records are kept solely as
        // superseding successes, never as unrelated failures from other sessions.
        if source_kind && !target.seed_failures.contains(*cid) {
            if is_pass_status_node(node) {
                merge_candidate(&mut success, node, anchors, rel);
            }
            continue;
        }
        route_candidate(node, anchors, rel, &mut agent, &mut runtime, &mut success);
    }

    // Hop 2: from each reached agent `Failure`, follow PRODUCED_PATCH / FAILED_ON
    // to its patch artifact and runtime command/test evidence — the Codex/traj
    // importers attach the rejected patch and failed `CommandRun` to the `Failure`
    // via FAILED_ON (PRODUCED_PATCH comes from the AgentTurn) — and walk the
    // AUTHORED_BY / SESSION_OF chain for provenance.
    let mut patch: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    let mut sessions: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut agents: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let agent_seeds: Vec<(&'a str, &'a GraphRecord, BTreeSet<&'a str>)> = agent
        .iter()
        .map(|(id, (node, anchors, _))| (*id, *node, anchors.clone()))
        .collect();
    for (fid, fnode, fanchors) in &agent_seeds {
        collect_failure_links(
            fnode,
            fid,
            fanchors,
            &edges_from,
            &present,
            &mut patch,
            &mut runtime,
            &mut success,
            &mut diagnostics,
        );
        collect_provenance(fid, &edges_from, &present, &mut sessions, &mut agents);
    }

    // Reached-failure instants per anchor: used both to compute read-time status
    // and to keep only successes that actually supersede a failure (AC5).
    let mut failure_instant_by_anchor: BTreeMap<&str, Vec<chrono::DateTime<chrono::FixedOffset>>> =
        BTreeMap::new();
    for (node, anchors, _) in agent.values().chain(runtime.values()) {
        if let Some(inst) = node_instant(node) {
            for a in anchors {
                failure_instant_by_anchor.entry(a).or_default().push(inst);
            }
        }
    }

    // A passing verification is surfaced only when it is strictly later than at
    // least one reached failure on a shared target. A pass with no failures, or a
    // pass that predates every failure, superseded nothing and is not shown (AC5).
    let mut success_by_anchor: BTreeMap<&str, Vec<(chrono::DateTime<chrono::FixedOffset>, &str)>> =
        BTreeMap::new();
    let mut superseding: BTreeMap<&str, MemoryEvidenceItem<'a>> = BTreeMap::new();
    for (sid, (node, anchors, rel)) in &success {
        let Some(inst) = node_instant(node) else {
            diagnostics.push(MemoryAuditDiagnostic {
                code: "missing_timestamp".to_owned(),
                source_record_id: (*sid).to_owned(),
                target_handle: (*sid).to_owned(),
                relation: "executed_at".to_owned(),
                target_domain: "verification".to_owned(),
            });
            continue;
        };
        let supersedes = anchors.iter().any(|a| {
            failure_instant_by_anchor
                .get(a)
                .is_some_and(|fs| fs.iter().any(|fi| *fi < inst))
        });
        if !supersedes {
            continue;
        }
        for a in anchors {
            success_by_anchor
                .entry(a)
                .or_default()
                .push((inst, node.id()));
        }
        superseding
            .entry(node.id())
            .or_insert_with(|| MemoryEvidenceItem {
                record: node,
                relation: (*rel).to_owned(),
            });
    }
    for list in success_by_anchor.values_mut() {
        list.sort_unstable();
    }

    // Build the failed-attempt items, computing each one's read-time status.
    let mut agent_failures: Vec<FailureAttempt<'a>> = Vec::new();
    let mut runtime_failures: Vec<FailureAttempt<'a>> = Vec::new();
    for (is_agent, source) in [(true, &agent), (false, &runtime)] {
        for (node, anchors, rel) in source.values() {
            // AC6: surface this attempt's own unresolved / stale / triple-only
            // evidence links rather than silently dropping them.
            push_attempt_link_diagnostics(node, &tombstoned, &by_id, &mut diagnostics);
            // AC5/AC6: an undated failure cannot be proven resolved; record why
            // its status stays `still_failing` so callers can tell "no later pass"
            // apart from "timestamp unusable".
            if node_instant(node).is_none() {
                diagnostics.push(MemoryAuditDiagnostic {
                    code: "missing_timestamp".to_owned(),
                    source_record_id: node.id().to_owned(),
                    target_handle: node.id().to_owned(),
                    relation: if is_agent {
                        "observed_at"
                    } else {
                        "executed_at"
                    }
                    .to_owned(),
                    target_domain: if is_agent {
                        "agent_memory"
                    } else {
                        "verification"
                    }
                    .to_owned(),
                });
            }
            let (status, resolved_by) =
                compute_resolution_status(node, anchors, &success_by_anchor);
            let matched_target = anchors.iter().min().copied().unwrap_or("");
            let attempt = FailureAttempt {
                item: MemoryEvidenceItem {
                    record: node,
                    relation: (*rel).to_owned(),
                },
                status,
                resolved_by,
                matched_target,
            };
            if is_agent {
                agent_failures.push(attempt);
            } else {
                runtime_failures.push(attempt);
            }
        }
    }

    // Canonical ordering: oldest-first by parsed instant (None last), then ID.
    sort_attempts(&mut agent_failures);
    sort_attempts(&mut runtime_failures);

    let mut superseding_successes: Vec<MemoryEvidenceItem<'a>> =
        superseding.into_values().collect();
    superseding_successes.sort_by(|a, b| {
        node_instant(a.record)
            .cmp(&node_instant(b.record))
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.target_domain.cmp(&b.target_domain))
    });
    diagnostics.dedup();

    let target_kind = target.kind.as_str();
    let mut target_ids: Vec<String> = anchor_universe.iter().map(|s| (*s).to_owned()).collect();
    target_ids.sort();

    FailureHistoryContext {
        target_kind,
        target_ids,
        runtime_failures,
        agent_failures,
        superseding_successes,
        patch_artifacts: patch.into_values().collect(),
        agent_sessions: sessions.into_values().collect(),
        agents: agents.into_values().collect(),
        diagnostics,
    }
}

/// Sorts failed attempts oldest-first by parsed instant (absent last), then ID.
fn sort_attempts(attempts: &mut [FailureAttempt<'_>]) {
    attempts.sort_by(|a, b| {
        let ai = node_instant(a.item.record);
        let bi = node_instant(b.item.record);
        // `None` (absent timestamp) sorts last: present-and-ordered first.
        match (ai, bi) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.item.record.id().cmp(b.item.record.id()))
    });
}

/// Returns the read-time resolution status for one failed attempt: `SinceResolved`
/// iff a passing verification on a shared anchor has a parsed instant strictly
/// after the attempt's, else `StillFailing` (AC5).
fn compute_resolution_status<'a>(
    node: &'a GraphRecord,
    anchors: &BTreeSet<&str>,
    success_by_anchor: &BTreeMap<&str, Vec<(chrono::DateTime<chrono::FixedOffset>, &'a str)>>,
) -> (ResolutionStatus, Option<&'a str>) {
    let Some(fail_time) = node_instant(node) else {
        return (ResolutionStatus::StillFailing, None);
    };
    let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, &str)> = None;
    for anchor in anchors {
        let Some(list) = success_by_anchor.get(anchor) else {
            continue;
        };
        for (instant, sid) in list {
            if *instant <= fail_time {
                continue;
            }
            let better = match best {
                None => true,
                Some((bt, bid)) => (*instant, *sid) > (bt, bid),
            };
            if better {
                best = Some((*instant, sid));
            }
        }
    }
    best.map_or((ResolutionStatus::StillFailing, None), |(_, sid)| {
        (ResolutionStatus::SinceResolved, Some(sid))
    })
}

/// Emits an attempt's own unresolved / stale / triple-only evidence-link
/// diagnostics, carrying the original handles (AC6).
fn push_attempt_link_diagnostics(
    node: &GraphRecord,
    tombstoned: &BTreeSet<&str>,
    by_id: &BTreeMap<&str, &GraphRecord>,
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
) {
    let GraphRecord::Node {
        id,
        evidence_links: Some(links),
        ..
    } = node
    else {
        return;
    };
    for link in links {
        let Some(target_id) = link.target_record_id.as_deref() else {
            let handle = link
                .target_repo_relative_path
                .clone()
                .unwrap_or_else(|| "<triple>".to_owned());
            diagnostics.push(MemoryAuditDiagnostic {
                code: "evidence_target_unresolved".to_owned(),
                source_record_id: id.clone(),
                target_handle: handle,
                relation: link.relation.clone(),
                target_domain: link.target_domain.clone(),
            });
            continue;
        };
        let code = if tombstoned.contains(target_id) {
            "stale_evidence_target"
        } else if by_id.contains_key(target_id) {
            continue;
        } else {
            "unresolved_evidence_link"
        };
        diagnostics.push(MemoryAuditDiagnostic {
            code: code.to_owned(),
            source_record_id: id.clone(),
            target_handle: target_id.to_owned(),
            relation: link.relation.clone(),
            target_domain: link.target_domain.clone(),
        });
    }
}

/// Returns the live code/task record IDs a node links to outbound, via graph
/// edges or denormalized evidence links with a target-linking relation.
fn outbound_code_task_targets<'a>(
    node: &'a GraphRecord,
    edges_from: &BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>,
    present: &impl Fn(&str) -> Option<&'a GraphRecord>,
) -> BTreeSet<&'a str> {
    let mut out: BTreeSet<&str> = BTreeSet::new();
    let mut consider = |id: &'a str| {
        if let Some(t) = present(id)
            && record_node_kind(t).is_some_and(|k| is_codegraph_kind(k) || is_project_kind(k))
        {
            out.insert(id);
        }
    };
    if let Some(edges) = edges_from.get(node.id()) {
        for (label, t) in edges {
            if FAILURE_TARGET_LINK_RELS.contains(&label.as_str()) {
                consider(t);
            }
        }
    }
    if let GraphRecord::Node {
        evidence_links: Some(links),
        ..
    } = node
    {
        for link in links {
            if FAILURE_TARGET_LINK_RELS.contains(&link.relation.as_str())
                && let Some(t) = link.target_record_id.as_deref()
            {
                consider(t);
            }
        }
    }
    out
}

/// From a reached agent `Failure`, follows `PRODUCED_PATCH` / `FAILED_ON` edges
/// and denormalized links to its patch artifact and runtime command/test
/// evidence, inheriting the failure's anchor set for the reached runtime records.
///
/// The Codex/trajectory importers link a patch-invalid failure to its rejected
/// `PatchArtifact` and a failed command to its `CommandRun` via `FAILED_ON`
/// (`PRODUCED_PATCH` is emitted from the AgentTurn), so following only
/// `PRODUCED_PATCH` from the failure would lose those citable artifacts.
#[expect(clippy::too_many_arguments)]
fn collect_failure_links<'a>(
    failure: &'a GraphRecord,
    failure_id: &str,
    anchors: &BTreeSet<&'a str>,
    edges_from: &BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>,
    present: &impl Fn(&str) -> Option<&'a GraphRecord>,
    patch: &mut BTreeMap<&'a str, MemoryEvidenceItem<'a>>,
    runtime: &mut CandidateMap<'a>,
    success: &mut CandidateMap<'a>,
    diagnostics: &mut Vec<MemoryAuditDiagnostic>,
) {
    // (relation, target_id) from both graph edges and denormalized links.
    let mut links: Vec<(&'a str, &'a str)> = Vec::new();
    if let Some(edges) = edges_from.get(failure_id) {
        for (label, target) in edges {
            if matches!(label, EdgeLabel::ProducedPatch | EdgeLabel::FailedOn) {
                links.push((label.as_str(), *target));
            }
        }
    }
    if let GraphRecord::Node {
        evidence_links: Some(el),
        ..
    } = failure
    {
        for link in el {
            if matches!(link.relation.as_str(), "PRODUCED_PATCH" | "FAILED_ON")
                && let Some(t) = link.target_record_id.as_deref()
            {
                links.push((link.relation.as_str(), t));
            }
        }
    }
    links.sort_unstable();
    links.dedup();

    for (rel, target) in links {
        let Some(node) = present(target) else {
            diagnostics.push(MemoryAuditDiagnostic {
                code: "unresolved_evidence_link".to_owned(),
                source_record_id: failure_id.to_owned(),
                target_handle: target.to_owned(),
                relation: rel.to_owned(),
                target_domain: String::new(),
            });
            continue;
        };
        match record_node_kind(node) {
            Some(NodeKind::PatchArtifact) => {
                patch
                    .entry(node.id())
                    .or_insert_with(|| MemoryEvidenceItem {
                        record: node,
                        relation: rel.to_owned(),
                    });
            }
            Some(k) if is_verification_kind(k) => {
                if is_failed_verification(node) {
                    merge_candidate(runtime, node, anchors, rel);
                } else if is_pass_status_node(node) {
                    merge_candidate(success, node, anchors, rel);
                }
            }
            _ => {}
        }
    }
}

/// Walks `AUTHORED_BY` / `SESSION_OF` from a failure to its session and agent.
fn collect_provenance<'a>(
    failure_id: &'a str,
    edges_from: &BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>,
    present: &impl Fn(&str) -> Option<&'a GraphRecord>,
    sessions: &mut BTreeMap<&'a str, &'a GraphRecord>,
    agents: &mut BTreeMap<&'a str, &'a GraphRecord>,
) {
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    let mut frontier: Vec<&str> = vec![failure_id];
    while let Some(id) = frontier.pop() {
        if !visited.insert(id) {
            continue;
        }
        let Some(out) = edges_from.get(id) else {
            continue;
        };
        for (label, target) in out {
            if !matches!(label, EdgeLabel::AuthoredBy | EdgeLabel::SessionOf) {
                continue;
            }
            if !visited.contains(*target) {
                frontier.push(target);
            }
            if let Some(node) = present(target) {
                match record_node_kind(node) {
                    Some(NodeKind::AgentSession) => {
                        sessions.entry(node.id()).or_insert(node);
                    }
                    Some(NodeKind::Agent) => {
                        agents.entry(node.id()).or_insert(node);
                    }
                    _ => {}
                }
            }
        }
    }
}
