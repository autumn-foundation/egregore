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
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use chrono::DateTime;

use crate::ir::{
    CallResolution, EdgeLabel, EvidenceLink, GraphRecord, NodeKind, OutputHandle, PatchHandle,
    SemanticDriftMetadata, SnapshotHead, SourceSpan, TemporalMetadata, UserContextScope,
    parse_codegraph_id,
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

/// Returns `true` when a symbol `name` matches a partial-name `pattern`
/// (issue #102).
///
/// Semantics (deterministic, no regex engine):
///
/// - A pattern containing `*` is an **anchored glob** over the whole name:
///   each `*` matches any (possibly empty) run of characters and every other
///   character is literal. `handle_*` is a prefix match, `*_sink` a suffix
///   match, and a starless glob would be an exact match.
/// - A pattern without `*` matches as a **literal substring** anywhere in the
///   name.
/// - Matching is case-sensitive unless `case_insensitive` is set, in which
///   case both sides are Unicode-lowercased first.
#[must_use]
pub fn symbol_name_matches(pattern: &str, name: &str, case_insensitive: bool) -> bool {
    if case_insensitive {
        return symbol_name_matches(&pattern.to_lowercase(), &name.to_lowercase(), false);
    }
    if !pattern.contains('*') {
        return name.contains(pattern);
    }
    glob_matches(pattern, name)
}

/// Anchored `*`-glob match: `pattern` must cover the whole of `name`.
///
/// Standard greedy algorithm: the segment before the first `*` must be a
/// prefix, the segment after the last `*` must be a non-overlapping suffix,
/// and the middle segments must appear in order (earliest match) in between.
fn glob_matches(pattern: &str, name: &str) -> bool {
    let segments: Vec<&str> = pattern.split('*').collect();
    let (first, rest_segments) = segments.split_first().expect("split yields >= 1 segment");
    if rest_segments.is_empty() {
        // No `*` in the pattern; anchored means exact.
        return name == *first;
    }
    let Some(core) = name.strip_prefix(first) else {
        return false;
    };
    let (last, middle) = rest_segments.split_last().expect("checked non-empty");
    let Some(mut core) = core.strip_suffix(last) else {
        return false;
    };
    for segment in middle {
        match core.find(segment) {
            Some(idx) => core = &core[idx + segment.len()..],
            None => return false,
        }
    }
    true
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
    /// Repository record ID → highest-version repository record ID.
    highest_version: BTreeMap<String, String>,
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
        // Remap owners to their highest-version counterpart to preserve all schema-version owners.
        let mut suffix_to_versions: HashMap<&str, Vec<(u32, &str)>> = HashMap::new();
        for repo_id in repos.keys() {
            if let Some((version, suffix)) = parse_codegraph_id(repo_id) {
                suffix_to_versions
                    .entry(suffix)
                    .or_default()
                    .push((version, repo_id.as_str()));
            }
        }
        let mut repo_translation: HashMap<String, String> = HashMap::new();
        for versions in suffix_to_versions.values() {
            if let Some((_, highest_repo_id)) = versions.iter().max_by_key(|(v, _)| v) {
                for (_, repo_id) in versions {
                    repo_translation.insert((*repo_id).to_owned(), (*highest_repo_id).to_owned());
                }
            }
        }
        for val in owner.values_mut() {
            if let Some(highest_id) = repo_translation.get(val) {
                *val = highest_id.clone();
            }
        }

        let highest_version: BTreeMap<String, String> = repo_translation.into_iter().collect();

        Self {
            owner,
            repos,
            highest_version,
        }
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
        let resolved = if let Some((id, _)) = self.repos.get_key_value(selector) {
            id.as_str()
        } else {
            let mut candidates: Vec<&str> = self
                .repos
                .iter()
                .filter(|(_, entry)| entry.selectors.contains(selector))
                .map(|(id, _)| id.as_str())
                .collect();

            // Deduplicate candidates that represent the same repository under different schema versions.
            if candidates.len() > 1 {
                let mut groups: std::collections::HashMap<&str, (u32, &str)> =
                    std::collections::HashMap::new();
                let mut has_unparseable = false;
                for candidate in &candidates {
                    if let Some((version, suffix)) = parse_codegraph_id(candidate) {
                        let entry = groups.entry(suffix).or_insert((0, ""));
                        if version > entry.0 {
                            *entry = (version, candidate);
                        }
                    } else {
                        has_unparseable = true;
                        break;
                    }
                }
                if !has_unparseable {
                    candidates = groups.values().map(|(_, id)| *id).collect();
                    candidates.sort_unstable();
                }
            }

            match candidates.as_slice() {
                [] => {
                    return Err(RepositorySelectorError::Unknown {
                        selector: selector.to_owned(),
                    });
                }
                [single] => *single,
                _ => {
                    return Err(RepositorySelectorError::Ambiguous {
                        selector: selector.to_owned(),
                        candidates: candidates.into_iter().map(str::to_owned).collect(),
                    });
                }
            }
        };

        Ok(self
            .highest_version
            .get(resolved)
            .map_or(resolved, String::as_str))
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
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
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

    context_from_seeds(records, symbol_name, source_facts, &symbol_ids)
}

/// Resolves the trust-separated context sections from a frozen set of
/// source-fact seed IDs.
///
/// Shared core used by [`symbol_context`] (seeds collected by symbol *name*)
/// and [`record_context`] (seeds collected from a specific *record ID*, so a
/// File-typed match is first-class). `source_facts` is the seed set already
/// containing the anchor node(s) plus their co-located/defined neighbors;
/// `symbol_ids` is the set of *primary* query nodes (the thing the caller asked
/// about), used to keep them in the source-facts section and to avoid
/// re-scanning them during backfill. The bounded cross-domain BFS, backfill to
/// convergence, and per-section sort are identical regardless of how the seeds
/// were chosen, so both entry points share one implementation and one set of
/// determinism guarantees.
#[must_use]
#[allow(clippy::too_many_lines)]
fn context_from_seeds<'a>(
    records: &'a [GraphRecord],
    symbol_name: &str,
    source_facts: BTreeSet<&'a str>,
    symbol_ids: &BTreeSet<&'a str>,
) -> SymbolContext<'a> {
    // Recompute the prelim lookups the core needs. These are cheap O(n) scans
    // and are derived deterministically from `records`, so computing them here
    // (rather than threading them through the seeding step) keeps the seam
    // narrow without changing behavior.
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
    let mut source_facts = source_facts;

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
    for sid in symbol_ids {
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

/// Returns evidence-backed context anchored on a specific record ID, so a
/// File-typed semantic match (which has no symbol name) is first-class (#90).
///
/// Mirrors [`symbol_context`] but seeds from the record itself rather than from
/// a name:
/// - a `Symbol` anchor seeds the symbol plus its co-located `File` (via
///   DEFINES, falling back to the shared repo-relative path);
/// - a `File` anchor seeds the file plus the `Symbol`s it DEFINES;
/// - any other anchor seeds just itself.
///
/// Returns an empty [`SymbolContext`] (`is_no_match()` is `true`) when the
/// anchor is absent from the slice or is a tombstoned current-state record.
/// The bounded BFS, backfill, trust separation, and deterministic ordering are
/// shared with [`symbol_context`] via [`context_from_seeds`].
#[must_use]
pub fn record_context<'a>(records: &'a [GraphRecord], anchor_id: &str) -> SymbolContext<'a> {
    let tombstoned_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
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

    // A record id is live unless it is a tombstoned current-state record; a
    // historical (temporal) version survives the current-state tombstone.
    let is_live = |id: &str| has_any_temporal_version.contains(id) || !tombstoned_ids.contains(id);

    // Canonical `&'a str` for a record id, if present in the slice.
    let id_ref = |wanted: &str| -> Option<&'a str> {
        records
            .iter()
            .find_map(|r| if r.id() == wanted { Some(r.id()) } else { None })
    };

    // Node kind for an id (from any version present in the slice).
    let kind_of = |wanted: &str| -> Option<NodeKind> {
        records.iter().find_map(|r| match r {
            GraphRecord::Node { id, kind, .. } if id == wanted => Some(*kind),
            _ => None,
        })
    };

    // No-match for an anchor that is absent, not a node, or tombstoned in the
    // current state without a surviving historical version.
    let (Some(anchor_ref), Some(anchor_kind)) = (id_ref(anchor_id), kind_of(anchor_id)) else {
        return SymbolContext {
            symbol_name: anchor_id.to_owned(),
            ..Default::default()
        };
    };
    if !is_live(anchor_id) {
        return SymbolContext {
            symbol_name: anchor_id.to_owned(),
            ..Default::default()
        };
    }

    let mut source_facts: BTreeSet<&str> = BTreeSet::new();
    let mut primary: BTreeSet<&str> = BTreeSet::new();
    source_facts.insert(anchor_ref);
    primary.insert(anchor_ref);

    if anchor_kind == NodeKind::File {
        // File anchor: BFS over DEFINES, CONTAINS, and IMPORTS edges to seed
        // all code-graph nodes belonging to this file — top-level items
        // (File → DEFINES → Symbol), modules (File → CONTAINS → Module),
        // deeper nesting (Module → DEFINES → Symbol, ImplBlock → DEFINES →
        // Method), and imports (owner → IMPORTS → Import). Pure edge traversal
        // stays within the file's own tree so same-path nodes from other
        // repositories are never mixed in.
        let mut frontier: Vec<&str> = vec![anchor_ref];
        while !frontier.is_empty() {
            let mut next_frontier: Vec<&str> = Vec::new();
            for &container in &frontier {
                for r in records {
                    let GraphRecord::Edge {
                        id: edge_id,
                        label,
                        source,
                        target,
                        temporal,
                        ..
                    } = r
                    else {
                        continue;
                    };
                    if source.as_str() != container {
                        continue;
                    }
                    if !matches!(
                        label,
                        EdgeLabel::Defines | EdgeLabel::Contains | EdgeLabel::Imports
                    ) {
                        continue;
                    }
                    let edge_live =
                        temporal.is_some() || !tombstoned_ids.contains(edge_id.as_str());
                    if !edge_live || !is_live(target.as_str()) {
                        continue;
                    }
                    let target_kind = kind_of(target.as_str());
                    // Seed Symbol, Module, and Import nodes; skip File (already
                    // the anchor) and infrastructure kinds.
                    if !matches!(
                        target_kind,
                        Some(NodeKind::Symbol | NodeKind::Module | NodeKind::Import)
                    ) {
                        continue;
                    }
                    if let Some(t) = id_ref(target.as_str())
                        && !source_facts.contains(t)
                    {
                        source_facts.insert(t);
                        primary.insert(t);
                        // Symbol and Module can contain further items — keep
                        // them in the frontier to continue the traversal.
                        if matches!(target_kind, Some(NodeKind::Symbol | NodeKind::Module)) {
                            next_frontier.push(t);
                        }
                    }
                }
            }
            frontier = next_frontier;
        }
    } else {
        // Symbol (or other) anchor: find the co-located File by traversing
        // upward through DEFINES and CONTAINS edges. Handles both top-level
        // items (File → DEFINES → Symbol) and nested items
        // (File → CONTAINS → Module → DEFINES → Symbol and
        //  File → DEFINES → ImplBlock → DEFINES → Method). No path-only
        // fallback is used, so same-path files from other repositories cannot
        // bleed into this match's source_facts.
        let mut to_search: Vec<&str> = vec![anchor_ref];
        let mut visited_up: BTreeSet<&str> = BTreeSet::new();
        visited_up.insert(anchor_ref);
        while !to_search.is_empty() {
            let mut next: Vec<&str> = Vec::new();
            for &target_id in &to_search {
                for r in records {
                    let GraphRecord::Edge {
                        id: edge_id,
                        label,
                        source,
                        target,
                        temporal,
                        ..
                    } = r
                    else {
                        continue;
                    };
                    if target.as_str() != target_id {
                        continue;
                    }
                    if !matches!(label, EdgeLabel::Defines | EdgeLabel::Contains) {
                        continue;
                    }
                    let edge_live =
                        temporal.is_some() || !tombstoned_ids.contains(edge_id.as_str());
                    if !edge_live || !is_live(source.as_str()) {
                        continue;
                    }
                    if kind_of(source.as_str()) == Some(NodeKind::File) {
                        if let Some(file) = id_ref(source.as_str()) {
                            source_facts.insert(file);
                        }
                    } else if let Some(container) = id_ref(source.as_str())
                        && !visited_up.contains(container)
                    {
                        visited_up.insert(container);
                        next.push(container);
                    }
                }
            }
            to_search = next;
        }
    }

    let label = records
        .iter()
        .find_map(|r| match r {
            GraphRecord::Node {
                id, name: Some(n), ..
            } if id == anchor_id => Some(n.clone()),
            _ => None,
        })
        .unwrap_or_else(|| anchor_id.to_owned());

    context_from_seeds(records, &label, source_facts, &primary)
}

// ── semantic → context bridge (issue #90) ──────────────────────────────────

/// A single semantic retrieval lead handed to [`semantic_context_bundle`].
///
/// Decoupled from the embeddings-feature `SemanticMatch` so the bridge — and
/// its tests — need no embedding model: callers (the CLI) convert each
/// `SemanticMatch` into one of these before context resolution. Carries only
/// the bounded retrieval-lead fields (record id, optional name/path/span, and
/// the relevance score), never raw content.
#[derive(Debug, Clone)]
pub struct SemanticLead {
    /// Stable record ID of the matched node.
    pub record_id: String,
    /// Human-readable name when the match carries one (absent for File nodes).
    pub name: Option<String>,
    /// Repository-relative path when available.
    pub repo_relative_path: Option<String>,
    /// Relevance score (higher = more similar).
    pub score: f32,
    /// Source span when available.
    pub span: Option<crate::ir::SourceSpan>,
}

/// How a semantic match anchored its context. Documents how File matches differ
/// from Symbol matches in the response (AC3 of #90).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AnchorKind {
    /// The match resolved to a `Symbol` node.
    Symbol,
    /// The match resolved to a `File` node (no symbol name; defined symbols are
    /// seeded into the context instead).
    File,
    /// The match resolved to some other embeddable node kind.
    Other,
}

impl AnchorKind {
    /// Stable lowercase tag for serialization.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::File => "file",
            Self::Other => "other",
        }
    }
}

/// One semantic match expanded into evidence-backed context.
pub struct SemanticMatchContext<'a> {
    /// The retrieval lead (handle + score) that produced this row.
    pub lead: SemanticLead,
    /// Whether the match anchored on a Symbol, File, or other node.
    pub anchor_kind: AnchorKind,
    /// Every candidate record ID when the match name resolves to more than one
    /// live symbol (AC4 — ambiguity is surfaced, not guessed). Sorted and
    /// deduplicated; empty when the match is unambiguous.
    pub candidate_record_ids: Vec<String>,
    /// The trust-separated context anchored on the match's record ID.
    pub context: SymbolContext<'a>,
}

/// The combined natural-language → evidence-backed-context answer.
pub struct SemanticContextBundle<'a> {
    /// One row per lead that cleared the relevance floor, in ranking order.
    pub matches: Vec<SemanticMatchContext<'a>>,
}

impl SemanticContextBundle<'_> {
    /// Returns `true` when no lead cleared the relevance floor — the documented
    /// no-match condition (AC7). Callers MUST check this before reading
    /// `matches`; the CLI maps it to a stable diagnostic and a distinct exit
    /// code rather than an empty success.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.matches.is_empty()
    }
}

/// Bridges ranked semantic leads into evidence-backed context (#90).
///
/// For each lead whose `score` is at or above `min_score` — the documented
/// relevance floor — in the leads' given (already-deterministic) ranking order,
/// resolves [`record_context`] anchored on the lead's record ID. File-typed
/// leads are first-class (defined symbols are seeded); an ambiguous symbol name
/// surfaces every candidate record ID instead of silently picking one. This
/// consumes the existing semantic ranking and symbol-context contracts and adds
/// no new domain, schema, or model. Read-only: it borrows `records` and mutates
/// nothing, and identical inputs produce identical output.
#[must_use]
pub fn semantic_context_bundle<'a>(
    records: &'a [GraphRecord],
    leads: &[SemanticLead],
    min_score: f32,
) -> SemanticContextBundle<'a> {
    let mut matches = Vec::new();
    for lead in leads {
        if lead.score < min_score {
            continue;
        }
        let anchor_kind = match record_kind(records, &lead.record_id) {
            Some(NodeKind::Symbol) => AnchorKind::Symbol,
            Some(NodeKind::File) => AnchorKind::File,
            _ => AnchorKind::Other,
        };
        let candidate_record_ids = lead
            .name
            .as_deref()
            .map(|name| live_symbol_ids_for_name(records, name))
            .filter(|ids| ids.len() > 1)
            .unwrap_or_default();
        let context = record_context(records, &lead.record_id);
        matches.push(SemanticMatchContext {
            lead: lead.clone(),
            anchor_kind,
            candidate_record_ids,
            context,
        });
    }
    SemanticContextBundle { matches }
}

/// Node kind for a record id (from any version present in the slice).
fn record_kind(records: &[GraphRecord], id: &str) -> Option<NodeKind> {
    records.iter().find_map(|r| match r {
        GraphRecord::Node { id: nid, kind, .. } if nid == id => Some(*kind),
        _ => None,
    })
}

/// All live `Symbol` record IDs matching `name`, sorted and deduplicated.
///
/// Mirrors the current-state filter used by [`symbol_context`]: a historical
/// (temporal) version survives a current-state tombstone; a tombstoned
/// current-state symbol is excluded.
fn live_symbol_ids_for_name(records: &[GraphRecord], name: &str) -> Vec<String> {
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let mut ids: Vec<String> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Symbol,
                name: Some(n),
                temporal,
                ..
            } = r
            else {
                return None;
            };
            if n != name {
                return None;
            }
            let is_historical = temporal.is_some();
            if !is_historical && tombstoned.contains(id.as_str()) {
                return None;
            }
            Some(id.clone())
        })
        .collect();
    ids.sort();
    ids.dedup();
    ids
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

// ── Subsystem-scoped cross-domain context query (issue #83) ──────────────────

/// Why a subsystem prefix was rejected.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum SubsystemPrefixError {
    /// The prefix is empty or reduces to nothing after stripping trailing slashes.
    Malformed {
        /// The prefix as supplied by the caller.
        prefix: String,
    },
}

impl SubsystemPrefixError {
    /// Stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Malformed { .. } => "malformed_prefix",
        }
    }
}

/// Returns `true` when `path` is the prefix itself or lies strictly under it
/// on a path-segment boundary.
///
/// Both the bare form (`src/alpha`) and the trailing-slash form (`src/alpha/`)
/// resolve identically. Segment awareness prevents sibling-path bleed:
/// `src/alpha` matches `src/alpha/foo.rs` but never `src/alphabet/x.rs`.
#[must_use]
pub fn path_is_under_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    match path.strip_prefix(prefix) {
        Some("") => true,
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Evidence-backed subsystem context returned by [`subsystem_context`].
///
/// Sections mirror [`SymbolContext`] but the entry point is a repo-relative
/// directory / module prefix rather than a symbol name. The `semantic_drift`
/// section is added because path-scoped subsystem triage benefits from knowing
/// which symbols under the prefix have drifted (unlike symbol context, which
/// omits drift as out-of-scope).
///
/// An [`Observation`] node MUST NOT appear in `source_facts`.
///
/// [`Observation`]: crate::ir::NodeKind::Observation
#[derive(Debug, Default, Clone)]
pub struct SubsystemContext<'a> {
    /// The queried path prefix (normalized: trailing slash stripped).
    pub prefix: String,
    /// Code-graph records: `File` and `Symbol` nodes under the prefix.
    pub source_facts: Vec<&'a GraphRecord>,
    /// Codegraph topology edges between nodes in `source_facts`.
    pub topology_edges: Vec<&'a GraphRecord>,
    /// Agent-authored `Observation` nodes linked to source facts.
    pub observations: Vec<&'a GraphRecord>,
    /// `Task` and `AcceptanceCriterion` nodes linked to source facts.
    pub project_state: Vec<&'a GraphRecord>,
    /// `Artifact` and `PatchArtifact` nodes linked to source facts.
    pub artifacts: Vec<&'a GraphRecord>,
    /// `Verification`, `TestRun`, `CommandRun` nodes linked to source facts.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// `SemanticDrift` nodes whose resolved target path is under the prefix.
    pub semantic_drift: Vec<&'a GraphRecord>,
    /// Evidence link targets referenced by agent-memory nodes that are absent
    /// from this store slice.
    pub unresolved: Vec<UnresolvedRef>,
}

impl SubsystemContext<'_> {
    /// Returns `true` when no records exist under the queried prefix.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.project_state.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.semantic_drift.is_empty()
            && self.unresolved.is_empty()
    }
}

/// Returns all known cross-domain context for a repo-relative path prefix.
///
/// # Errors
///
/// Returns [`SubsystemPrefixError::Malformed`] when the prefix is empty or
/// reduces to nothing after stripping trailing slashes.
///
/// # Algorithm
///
/// 1. Validate and normalize the prefix.
/// 2. Collect seed `File` and `Symbol` nodes under the prefix.
/// 3. Run the same bounded-BFS + backfill traversal as `symbol_context` to
///    discover cross-domain linked records, classified by trust section.
/// 4. Separately collect `SemanticDrift` nodes whose resolved target path lies
///    under the prefix via `resolve_drift_target`.
///
/// Output ordering is deterministic: sorted by record ID within each section.
#[allow(clippy::too_many_lines)]
pub fn subsystem_context<'a>(
    records: &'a [GraphRecord],
    prefix: &str,
) -> Result<SubsystemContext<'a>, SubsystemPrefixError> {
    let normalized = prefix.trim_end_matches('/');
    if normalized.is_empty() {
        return Err(SubsystemPrefixError::Malformed {
            prefix: prefix.to_owned(),
        });
    }

    // Step 0: tombstone set (mirrors symbol_context).
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

    // Build fast lookup map and temporal-version index.
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

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

    // Step 1: collect File and Symbol node IDs under the prefix.
    let seed_ids: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            let GraphRecord::Node {
                id,
                kind,
                repo_relative_path: Some(path),
                ..
            } = r
            else {
                return None;
            };
            if !matches!(kind, NodeKind::File | NodeKind::Symbol) {
                return None;
            }
            let is_historical = matches!(
                r,
                GraphRecord::Node {
                    temporal: Some(_),
                    ..
                }
            );
            if !is_historical && tombstoned_ids.contains(id.as_str()) {
                return None;
            }
            if path_is_under_prefix(path.as_str(), normalized) {
                Some(id.as_str())
            } else {
                None
            }
        })
        .collect();

    if seed_ids.is_empty() {
        return Ok(SubsystemContext {
            prefix: normalized.to_owned(),
            ..Default::default()
        });
    }

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

    // Step 1b: collect topology edges between seed nodes.
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
            && (has_any_temporal_version.contains(id.as_str())
                || !tombstoned_ids.contains(id.as_str()))
            && seed_ids.contains(source.as_str())
            && seed_ids.contains(target.as_str())
        {
            topology_edge_ids.insert(id.as_str());
        }
    }

    // Steps 2+: bounded BFS + backfill — same logic as symbol_context.
    let mut source_facts: BTreeSet<&str> = seed_ids.clone();
    let mut observations: BTreeSet<&str> = BTreeSet::new();
    let mut project_state: BTreeSet<&str> = BTreeSet::new();
    let mut artifacts: BTreeSet<&str> = BTreeSet::new();
    let mut verification_evidence: BTreeSet<&str> = BTreeSet::new();

    let present_ids: BTreeSet<&str> = records.iter().map(GraphRecord::id).collect();
    let mut unresolved: Vec<UnresolvedRef> = Vec::new();

    let classify_and_insert = |record_id: &'a str,
                               source_facts: &mut BTreeSet<&'a str>,
                               observations: &mut BTreeSet<&'a str>,
                               project_state: &mut BTreeSet<&'a str>,
                               artifacts: &mut BTreeSet<&'a str>,
                               verification_evidence: &mut BTreeSet<&'a str>|
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

    let mut visited: BTreeSet<&str> = seed_ids.clone();
    let mut frontier: BTreeSet<&str> = seed_ids.clone();
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
                        || visited.contains(node_id.as_str()),
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
                        let has_triple = links.iter().any(|link| {
                            link.target_record_id.is_none()
                                && evidence_link_triple_handle(link).is_some()
                        });
                        if has_triple {
                            for link in links {
                                if link.target_record_id.is_none() {
                                    let Some(handle) = evidence_link_triple_handle(link) else {
                                        continue;
                                    };
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
            break;
        }
        frontier = next_frontier.into_iter().collect();
    }

    // Backfill: scan evidence_links of classified nodes.
    let mut backfill_scanned: BTreeSet<String> = seed_ids.iter().map(ToString::to_string).collect();

    loop {
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
                        && !seed_ids.contains(target_id.as_str())
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

    // Frontier expansion for backfill discoveries.
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

    // Remove seed node IDs from non-source-fact sections.
    for sid in &seed_ids {
        observations.remove(sid);
        project_state.remove(sid);
        artifacts.remove(sid);
        verification_evidence.remove(sid);
    }

    // Resolve ID sets → sorted record slices.
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

    // Semantic drift: every SemanticDrift node whose resolved target path is under the prefix.
    let semantic_drift: Vec<&'a GraphRecord> = {
        let mut drift_records: Vec<&'a GraphRecord> = records
            .iter()
            .filter_map(|r| {
                let drift_meta = semantic_drift(r)?;
                // Skip tombstoned non-temporal drift nodes.
                let is_temporal = matches!(
                    r,
                    GraphRecord::Node {
                        temporal: Some(_),
                        ..
                    }
                );
                if !is_temporal && tombstoned_ids.contains(r.id()) {
                    return None;
                }
                let (resolved_path, _, _) =
                    resolve_drift_target(records, r.id(), drift_meta, None, None);
                let path = resolved_path?;
                if path_is_under_prefix(path, normalized) {
                    Some(r)
                } else {
                    None
                }
            })
            .collect();
        drift_records.sort_by_key(|r| r.id());
        drift_records
    };

    Ok(SubsystemContext {
        prefix: normalized.to_owned(),
        source_facts: resolve(&source_facts),
        topology_edges: {
            let mut out: Vec<&'a GraphRecord> = records
                .iter()
                .filter(|r| {
                    topology_edge_ids.contains(r.id())
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
        semantic_drift,
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
    })
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

/// Resolves the symbol nodes representing the current HEAD state of their respective repositories.
/// Fallbacks to maximum-timestamp matching if Repository metadata or Git context is missing.
#[must_use]
pub fn resolve_head_symbols<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Vec<&'records GraphRecord> {
    let mut repo_heads = HashMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot: Some(snapshot),
            ..
        } = record
        {
            if let SnapshotHead::Commit { sha } = &snapshot.head {
                repo_heads.insert(id.as_str(), sha.as_str());
            }
        }
    }

    let mut best: BTreeMap<Option<&str>, &GraphRecord> = BTreeMap::new();
    let mut repos_with_match = HashSet::new();

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Symbol,
            name,
            temporal: Some(t),
            ..
        } = record
        else {
            continue;
        };
        if name.as_deref() != Some(symbol_name) {
            continue;
        }
        let owner = index.owner_of(record.id());
        if let Some(repo_id) = repo {
            if owner != Some(repo_id) {
                continue;
            }
        }
        if let Some(owner_id) = owner {
            if let Some(&head_sha) = repo_heads.get(owner_id) {
                if t.git_commit == head_sha {
                    let is_better = best
                        .get(&owner)
                        .is_none_or(|prev_r| record.id() < prev_r.id());
                    if is_better {
                        best.insert(owner, record);
                        repos_with_match.insert(owner_id);
                    }
                }
            }
        }
    }

    let mut matched: Vec<&GraphRecord> = best.into_values().collect();

    let mut fallback_repos = Vec::new();
    if let Some(repo_id) = repo {
        if !repos_with_match.contains(repo_id) {
            fallback_repos.push(Some(repo_id));
        }
    } else {
        for record in records {
            if let GraphRecord::Node {
                kind: NodeKind::Symbol,
                name,
                ..
            } = record
            {
                if name.as_deref() == Some(symbol_name) {
                    let owner = index.owner_of(record.id());
                    if let Some(o) = owner {
                        if !repos_with_match.contains(o) {
                            fallback_repos.push(Some(o));
                        }
                    } else if repos_with_match.is_empty() {
                        fallback_repos.push(None);
                    }
                }
            }
        }
    }

    fallback_repos.sort();
    fallback_repos.dedup();

    for r_opt in fallback_repos {
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
            let owner = index.owner_of(record.id());
            if owner != r_opt {
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
            let is_better = best.as_ref().is_none_or(|(prev_r, prev_vt)| {
                vt > *prev_vt || (vt == *prev_vt && record.id() < prev_r.id())
            });
            if is_better {
                best = Some((record, vt));
            }
        }
        if let Some((r, _)) = best {
            matched.push(r);
        }
    }

    matched.sort_by(|left, right| left.id().cmp(right.id()));
    matched
}

/// Resolves the symbol nodes representing the state of their respective repositories as of a specific valid time.
///
/// Strictly filters candidate symbol commits by repository HEAD lineage before picking the newest.
/// Fallbacks to maximum-timestamp matching if Repository metadata or Git context is missing.
///
/// # Errors
///
/// Returns an error if the `--as-of` timestamp is not a valid RFC3339 string.
#[allow(clippy::implicit_hasher)]
pub fn resolve_as_of_symbols<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    as_of: &str,
    index: &RepositoryIndex,
    repo: Option<&str>,
    commit_parents: &HashMap<&str, &'records [String]>,
    commit_nodes: &HashMap<&str, Vec<&'records GraphRecord>>,
) -> Result<Vec<&'records GraphRecord>, String> {
    let as_of_dt = DateTime::parse_from_rfc3339(as_of)
        .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;

    // Find HEAD commit of each repository
    let mut repo_heads = HashMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot: Some(snapshot),
            ..
        } = record
        {
            if let SnapshotHead::Commit { sha } = &snapshot.head {
                repo_heads.insert(id.as_str(), sha.as_str());
            }
        }
    }

    // For each repository owner, compute its filtered lineage (ancestors of HEAD as of as_of_dt)
    let mut repo_lineages = HashMap::new();
    for (&repo_id, &head_sha) in &repo_heads {
        if let Some(repo_filter) = repo {
            if repo_id != repo_filter {
                continue;
            }
        }

        // Traverse ancestry from head_sha
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(head_sha);

        while let Some(sha) = queue.pop_front() {
            if visited.insert(sha) {
                if let Some(&parents) = commit_parents.get(sha) {
                    for parent in parents {
                        let p_str = parent.as_str();
                        if !visited.contains(p_str) {
                            queue.push_back(p_str);
                        }
                    }
                }
            }
        }

        // Filter visited commits by valid_time <= as_of_dt
        let mut filtered = HashSet::new();
        for sha in visited {
            if let Some(c_nodes) = commit_nodes.get(sha) {
                let has_valid_node = c_nodes.iter().any(|c_node| {
                    let owner = index.owner_of(c_node.id());
                    if owner.is_some_and(|o| o != repo_id) {
                        return false;
                    }
                    if let GraphRecord::Node {
                        temporal: Some(t), ..
                    } = c_node
                    {
                        if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                            return vt <= as_of_dt;
                        }
                    }
                    false
                });
                if has_valid_node {
                    filtered.insert(sha);
                }
            }
        }
        repo_lineages.insert(repo_id, filtered);
    }

    // Now, find all candidate symbols that are on the computed lineages and <= as_of_dt
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
        if let Some(repo_id) = repo {
            if owner != Some(repo_id) {
                continue;
            }
        }

        // Must be on the lineage of its owner repository
        if let Some(owner_id) = owner {
            if let Some(lineage) = repo_lineages.get(owner_id) {
                let Some(t) = temporal else {
                    continue;
                };
                if !lineage.contains(t.git_commit.as_str()) {
                    continue;
                }
            }
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

/// Finds the Commit node that last changed the symbol's file at or before the queried commit/time.
///
/// Returns `Ok(Some((symbol_node, commit_node)))` if found.
///
/// # Errors
///
/// Returns an error string when a temporal selector is malformed or the requested commit is not found.
#[allow(clippy::option_if_let_else)]
pub fn who_last_changed<'records>(
    records: &'records [GraphRecord],
    symbol_name: &str,
    at_commit: Option<&str>,
    as_of_time: Option<&str>,
    index: &RepositoryIndex,
    repo: Option<&str>,
) -> Result<Option<(&'records GraphRecord, &'records GraphRecord)>, String> {
    // 1. Build indices in a single O(N) pre-pass to prevent multiple scans
    let mut commit_nodes: HashMap<&str, Vec<&GraphRecord>> = HashMap::new();
    let mut commit_parents = HashMap::new();
    let mut tombstoned_ids = HashSet::new();
    let mut matching_commits = Vec::new();
    let mut symbol_by_commit_and_name: HashMap<(&str, &str), Vec<&GraphRecord>> = HashMap::new();

    for record in records {
        match record {
            GraphRecord::Tombstone { deleted_id, .. } => {
                tombstoned_ids.insert(deleted_id.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::Commit,
                temporal: Some(t),
                ..
            } => {
                let sha = t.git_commit.as_str();
                commit_nodes.entry(sha).or_default().push(record);
                commit_parents.insert(sha, t.git_parent_commits.as_slice());
                if let Some(prefix) = at_commit {
                    if sha.starts_with(prefix) {
                        matching_commits.push(sha);
                    }
                }
            }
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                name: Some(name),
                temporal: Some(t),
                ..
            } => {
                symbol_by_commit_and_name
                    .entry((t.git_commit.as_str(), name.as_str()))
                    .or_default()
                    .push(record);
            }
            _ => {}
        }
    }

    // Build a CommitOrder helper for topological rankings
    let commit_order = CommitOrder::build(records);

    // 2. Resolve the symbol node
    let symbol_nodes = if let Some(commit) = at_commit {
        if let Some(repo_id) = repo {
            matching_commits.retain(|sha| {
                commit_nodes.get(sha).is_some_and(|c_nodes| {
                    c_nodes.iter().any(|c_node| {
                        let owner = index.owner_of(c_node.id());
                        owner == Some(repo_id)
                    })
                })
            });
        }

        // Deduplicate matching_commits to ensure unique SHAs
        matching_commits.sort_unstable();
        matching_commits.dedup();

        // Resolve prefix and enforce uniqueness
        if matching_commits.is_empty() {
            return Err(format!("commit prefix '{commit}' not found"));
        } else if matching_commits.len() > 1 {
            return Err(format!(
                "commit prefix '{commit}' is ambiguous, matched: {:?}",
                matching_commits
            ));
        }

        let mut matches = symbols_at_commit(records, symbol_name, commit);
        if let Some(repo_id) = repo {
            matches.retain(|r| index.owner_of(r.id()) == Some(repo_id));
        }
        matches
    } else {
        let mut matches = if let Some(as_of) = as_of_time {
            resolve_as_of_symbols(
                records,
                symbol_name,
                as_of,
                index,
                repo,
                &commit_parents,
                &commit_nodes,
            )?
        } else {
            resolve_head_symbols(records, symbol_name, index, repo)
        };

        // Filter matches to only include live symbols in their repository lineage at as_of
        matches.retain(|r| {
            let Some(repo_id) = index.owner_of(r.id()) else {
                return true; // legacy/unattributed repository, keep it
            };
            let r_commit = if let GraphRecord::Node {
                temporal: Some(t), ..
            } = r
            {
                t.git_commit.as_str()
            } else {
                return true;
            };

            // Find HEAD commit of repo_id
            let mut head_sha = None;
            for record in records {
                if let GraphRecord::Node {
                    kind: NodeKind::Repository,
                    id,
                    source_snapshot: Some(snapshot),
                    ..
                } = record
                {
                    if id == repo_id {
                        if let SnapshotHead::Commit { sha } = &snapshot.head {
                            head_sha = Some(sha.as_str());
                        }
                        break;
                    }
                }
            }

            let Some(start_sha) = head_sha else {
                return true; // no git context/head commit, keep it
            };

            // Traverse ancestry from start_sha
            let mut visited = HashSet::new();
            let mut queue = VecDeque::new();
            queue.push_back(start_sha);

            while let Some(sha) = queue.pop_front() {
                if visited.insert(sha) {
                    if let Some(parents) = commit_parents.get(sha) {
                        for parent in *parents {
                            let p_str = parent.as_str();
                            if !visited.contains(p_str) {
                                queue.push_back(p_str);
                            }
                        }
                    }
                }
            }

            // Filter by as_of_time
            let filtered_lineage = if let Some(as_of_t) = as_of_time {
                let Ok(as_of_dt) = DateTime::parse_from_rfc3339(as_of_t) else {
                    return false; // invalid timestamp format
                };
                let mut filtered = HashSet::new();
                for sha in visited {
                    if let Some(c_nodes) = commit_nodes.get(sha) {
                        let has_valid_node = c_nodes.iter().any(|c_node| {
                            let owner = index.owner_of(c_node.id());
                            if owner.is_some_and(|o| o != repo_id) {
                                return false;
                            }
                            if let GraphRecord::Node {
                                temporal: Some(t), ..
                            } = c_node
                            {
                                if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                                    return vt <= as_of_dt;
                                }
                            }
                            false
                        });
                        if has_valid_node {
                            filtered.insert(sha);
                        }
                    }
                }
                filtered
            } else {
                visited
            };

            if !filtered_lineage.contains(r_commit) {
                return false;
            }

            // Find lineage_head (latest commit in the lineage)
            let lineage_head = filtered_lineage
                .iter()
                .max_by_key(|&&sha| commit_order.rank(sha))
                .copied();

            let Some(head_commit_sha) = lineage_head else {
                return false; // no commits in lineage at this time
            };

            let r_kind = match r {
                GraphRecord::Node {
                    symbol_kind: Some(k),
                    ..
                } => k.as_str(),
                _ => "fn",
            };
            let r_disambiguator = match r {
                GraphRecord::Node {
                    disambiguator: Some(d),
                    ..
                } => *d,
                _ => 0,
            };
            let r_path = match r {
                GraphRecord::Node {
                    repo_relative_path, ..
                } => repo_relative_path.as_deref(),
                _ => None,
            };

            // Check if there is a Symbol node for symbol_name at head_commit_sha owned by repo_id
            // that matches candidate r's path, kind, and disambiguator.
            records.iter().any(|rec| {
                if let GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    name,
                    temporal: Some(t),
                    symbol_kind: Some(rec_kind),
                    disambiguator: Some(rec_disambiguator),
                    repo_relative_path: rec_path,
                    ..
                } = rec
                {
                    if name.as_deref() == Some(symbol_name) && t.git_commit == head_commit_sha {
                        if index.owner_of(rec.id()) == Some(repo_id) {
                            return rec_path.as_deref() == r_path
                                && rec_kind == r_kind
                                && *rec_disambiguator == r_disambiguator;
                        }
                    }
                }
                false
            })
        });

        // Suppress tombstoned symbol nodes for active HEAD / current-state queries
        if as_of_time.is_none() {
            matches.retain(|r| !tombstoned_ids.contains(r.id()));
        }
        matches
    };

    if symbol_nodes.len() > 1 {
        let repos: Vec<&str> = symbol_nodes
            .iter()
            .filter_map(|r| index.owner_of(r.id()))
            .collect();
        return Err(format!(
            "symbol '{symbol_name}' is defined in multiple repositories ({repos:?}). Please specify --repo to resolve ambiguity."
        ));
    }
    let symbol_node = symbol_nodes.into_iter().next();

    let Some(symbol_node) = symbol_node else {
        return Ok(None);
    };

    let (target_symbol_kind, target_disambiguator, target_file_path, start_sha) = {
        if let GraphRecord::Node {
            symbol_kind,
            disambiguator,
            repo_relative_path: Some(path),
            temporal: Some(t),
            ..
        } = symbol_node
        {
            (
                symbol_kind.as_deref(),
                *disambiguator,
                path.as_str(),
                t.git_commit.as_str(),
            )
        } else {
            return Ok(None);
        }
    };

    let target_repo_id = index.owner_of(symbol_node.id());

    // 3. Define candidate commits, scoping them to the active lineage (prevent branch bleeding)
    let candidate_commit_shas = if at_commit.is_some() {
        let start_sha = matching_commits[0];

        // Traverse ancestry from target commit
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start_sha);

        while let Some(sha) = queue.pop_front() {
            if visited.insert(sha) {
                if let Some(parents) = commit_parents.get(sha) {
                    for parent in *parents {
                        let p_str = parent.as_str();
                        if !visited.contains(p_str) {
                            queue.push_back(p_str);
                        }
                    }
                }
            }
        }

        // Apply valid-time limit to target ancestry if --as-of is co-specified
        if let Some(as_of) = as_of_time {
            let as_of_dt = DateTime::parse_from_rfc3339(as_of)
                .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;
            let mut filtered = HashSet::new();
            for sha in visited {
                if let Some(c_nodes) = commit_nodes.get(sha) {
                    let has_valid_node = c_nodes.iter().any(|c_node| {
                        if let Some(repo_id) = target_repo_id {
                            let owner = index.owner_of(c_node.id());
                            if owner != Some(repo_id) && owner.is_some() {
                                return false;
                            }
                        }
                        if let GraphRecord::Node {
                            temporal: Some(t), ..
                        } = c_node
                        {
                            if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                                return vt <= as_of_dt;
                            }
                        }
                        false
                    });
                    if has_valid_node {
                        filtered.insert(sha);
                    }
                }
            }
            filtered
        } else {
            visited
        }
    } else {
        // Lineage traversal for HEAD/as-of queries starting from resolved symbol commit
        let mut visited = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(start_sha);

        while let Some(sha) = queue.pop_front() {
            if visited.insert(sha) {
                if let Some(parents) = commit_parents.get(sha) {
                    for parent in *parents {
                        let p_str = parent.as_str();
                        if !visited.contains(p_str) {
                            queue.push_back(p_str);
                        }
                    }
                }
            }
        }

        if let Some(as_of) = as_of_time {
            let as_of_dt = DateTime::parse_from_rfc3339(as_of)
                .map_err(|e| format!("invalid --as-of timestamp '{as_of}': {e}"))?;
            let mut filtered = HashSet::new();
            for sha in visited {
                if let Some(c_nodes) = commit_nodes.get(sha) {
                    let has_valid_node = c_nodes.iter().any(|c_node| {
                        if let Some(repo_id) = target_repo_id {
                            let owner = index.owner_of(c_node.id());
                            if owner != Some(repo_id) && owner.is_some() {
                                return false;
                            }
                        }
                        if let GraphRecord::Node {
                            temporal: Some(t), ..
                        } = c_node
                        {
                            if let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) {
                                return vt <= as_of_dt;
                            }
                        }
                        false
                    });
                    if has_valid_node {
                        filtered.insert(sha);
                    }
                }
            }
            filtered
        } else {
            visited
        }
    };

    let get_matching_symbols = |commit_sha: &str| -> Vec<&GraphRecord> {
        symbol_by_commit_and_name
            .get(&(commit_sha, symbol_name))
            .map(|syms| {
                syms.iter()
                    .filter(|sym| {
                        let owner = index.owner_of(sym.id());
                        if owner != target_repo_id {
                            return false;
                        }
                        if let GraphRecord::Node {
                            symbol_kind,
                            disambiguator,
                            ..
                        } = sym
                        {
                            if symbol_kind.as_deref() != target_symbol_kind {
                                return false;
                            }
                            if *disambiguator != target_disambiguator {
                                return false;
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .copied()
                    .collect()
            })
            .unwrap_or_default()
    };

    let mut symbol_path_by_commit: HashMap<&str, &str> = HashMap::new();
    symbol_path_by_commit.insert(start_sha, target_file_path);

    let mut path_queue = VecDeque::new();
    path_queue.push_back(start_sha);

    let mut path_visited = HashSet::new();

    while let Some(sha) = path_queue.pop_front() {
        if !path_visited.insert(sha) {
            continue;
        }

        let Some(&current_path) = symbol_path_by_commit.get(sha) else {
            continue;
        };

        if let Some(parents) = commit_parents.get(sha) {
            for parent in *parents {
                let parent_sha = parent.as_str();
                if !candidate_commit_shas.contains(parent_sha) {
                    continue;
                }
                if symbol_path_by_commit.contains_key(parent_sha) {
                    path_queue.push_back(parent_sha);
                    continue;
                }

                let parent_syms = get_matching_symbols(parent_sha);

                let parent_path = if let Some(p_sym) = parent_syms.iter().find(|s| {
                    if let GraphRecord::Node {
                        repo_relative_path: Some(path),
                        ..
                    } = s
                    {
                        path.as_str() == current_path
                    } else {
                        false
                    }
                }) {
                    if let GraphRecord::Node {
                        repo_relative_path: Some(path),
                        ..
                    } = p_sym
                    {
                        Some(path.as_str())
                    } else {
                        None
                    }
                } else if parent_syms.len() == 1 {
                    if let GraphRecord::Node {
                        repo_relative_path: Some(path),
                        ..
                    } = parent_syms[0]
                    {
                        Some(path.as_str())
                    } else {
                        None
                    }
                } else if !parent_syms.is_empty() {
                    let mut found_path = None;
                    for p_sym in &parent_syms {
                        if let GraphRecord::Node {
                            repo_relative_path: Some(p_path),
                            ..
                        } = p_sym
                        {
                            let has_change = records.iter().any(|rec| {
                                if let GraphRecord::Node {
                                    kind: NodeKind::Change,
                                    repo_relative_path: Some(change_path),
                                    temporal: Some(t_change),
                                    ..
                                } = rec
                                {
                                    t_change.git_commit == sha && change_path == p_path
                                } else {
                                    false
                                }
                            });
                            if has_change {
                                found_path = Some(p_path.as_str());
                                break;
                            }
                        }
                    }
                    found_path.or_else(|| {
                        if let GraphRecord::Node {
                            repo_relative_path: Some(p_path),
                            ..
                        } = parent_syms[0]
                        {
                            Some(p_path.as_str())
                        } else {
                            None
                        }
                    })
                } else {
                    None
                };

                if let Some(path) = parent_path {
                    symbol_path_by_commit.insert(parent_sha, path);
                    path_queue.push_back(parent_sha);
                }
            }
        }
    }

    // Helper to find the symbol node at a specific commit SHA that matches target repo, kind, disambiguator, and tracked path
    let get_symbol_node = |commit_sha: &str| -> Option<&GraphRecord> {
        let tracked_path = symbol_path_by_commit.get(commit_sha).copied();
        symbol_by_commit_and_name
            .get(&(commit_sha, symbol_name))
            .and_then(|syms| {
                syms.iter()
                    .find(|sym| {
                        let owner = index.owner_of(sym.id());
                        if owner != target_repo_id {
                            return false;
                        }
                        if let GraphRecord::Node {
                            symbol_kind,
                            disambiguator,
                            repo_relative_path,
                            ..
                        } = sym
                        {
                            if symbol_kind.as_deref() != target_symbol_kind {
                                return false;
                            }
                            if *disambiguator != target_disambiguator {
                                return false;
                            }
                            if tracked_path.is_some()
                                && repo_relative_path.as_deref() != tracked_path
                            {
                                return false;
                            }
                            true
                        } else {
                            false
                        }
                    })
                    .copied()
            })
    };

    // 4. Find the latest commit that changed the file, ordering topologically
    let mut latest_commit: Option<(&GraphRecord, usize, DateTime<chrono::FixedOffset>)> = None;

    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::Change,
            repo_relative_path: Some(change_path),
            temporal: Some(t),
            ..
        } = record
        else {
            continue;
        };

        let sha = t.git_commit.as_str();
        if !candidate_commit_shas.contains(sha) {
            continue;
        }

        // O(1) commit lookup
        let commit_node = commit_nodes.get(sha).and_then(|c_nodes| {
            target_repo_id.map_or_else(
                || c_nodes.first().copied(),
                |repo_id| {
                    c_nodes
                        .iter()
                        .find(|c_node| {
                            let owner = index.owner_of(c_node.id());
                            owner == Some(repo_id)
                        })
                        .copied()
                },
            )
        });
        let Some(commit_node) = commit_node else {
            continue;
        };

        let Ok(vt) = DateTime::parse_from_rfc3339(&t.valid_time) else {
            continue;
        };

        // Check if the symbol actually changed in this commit
        let Some(sym_node_at_sha) = get_symbol_node(sha) else {
            continue;
        };

        let GraphRecord::Node {
            repo_relative_path: Some(sym_path_at_sha),
            summary: sym_summary,
            ..
        } = sym_node_at_sha
        else {
            continue;
        };

        if change_path != sym_path_at_sha {
            continue;
        }

        let parents = commit_parents.get(sha).copied().unwrap_or(&[]);
        let has_unchanged_parent = parents.iter().any(|parent_sha| {
            let parent_sha_str = parent_sha.as_str();
            if !candidate_commit_shas.contains(parent_sha_str) {
                return false;
            }
            if let Some(GraphRecord::Node {
                summary: parent_summary,
                ..
            }) = get_symbol_node(parent_sha_str)
            {
                return parent_summary == sym_summary;
            }
            false
        });

        if has_unchanged_parent {
            continue;
        }

        let current_rank = commit_order.rank(sha);

        let is_better = if let Some((prev_commit, prev_rank, prev_vt)) = latest_commit {
            // Rank topological descendant first, fall back to timestamp for parallel branches,
            // and use lexicographical ID comparison as the final tie-breaker.
            current_rank > prev_rank
                || (current_rank == prev_rank && vt > prev_vt)
                || (current_rank == prev_rank
                    && vt == prev_vt
                    && commit_node.id() < prev_commit.id())
        } else {
            true
        };

        if is_better {
            latest_commit = Some((commit_node, current_rank, vt));
        }
    }

    Ok(latest_commit.map(|(commit, _, _)| (symbol_node, commit)))
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
struct CommitOrder<'a> {
    /// Commit SHA → longest ancestor-chain length (topological rank).
    rank: BTreeMap<&'a str, usize>,
    /// Commit SHA → its direct child commits (parent links inverted).
    children: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> CommitOrder<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        // Longest chain over commits that are themselves present in the store
        // (absent shallow-boundary parents anchor at 0).
        fn rank_of<'b>(
            sha: &'b str,
            parents: &BTreeMap<&'b str, Vec<&'b str>>,
            memo: &mut BTreeMap<&'b str, usize>,
            stack: &mut BTreeSet<&'b str>,
        ) -> usize {
            if let Some(&r) = memo.get(sha) {
                return r;
            }
            if !stack.insert(sha) {
                return 0; // cycle guard (not expected in a Git history)
            }
            let mut best = 0;
            if let Some(ps) = parents.get(sha) {
                for &p in ps {
                    if parents.contains_key(p) {
                        best = best.max(rank_of(p, parents, memo, stack) + 1);
                    }
                }
            }
            stack.remove(sha);
            memo.insert(sha, best);
            best
        }

        // commit → deduplicated parent SHAs, for every commit observed as a
        // commit record.
        let mut parents: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node {
                temporal: Some(t), ..
            } = record
            {
                let entry = parents.entry(t.git_commit.as_str()).or_default();
                for parent in &t.git_parent_commits {
                    if !entry.contains(&parent.as_str()) {
                        entry.push(parent.as_str());
                    }
                }
            }
        }

        // Topological rank per commit.
        let mut rank: BTreeMap<&'a str, usize> = BTreeMap::new();
        let mut stack: BTreeSet<&'a str> = BTreeSet::new();
        for &commit in parents.keys() {
            rank_of(commit, &parents, &mut rank, &mut stack);
        }

        // Invert parent links into a child adjacency map for descendant walks.
        let mut children: BTreeMap<&'a str, Vec<&'a str>> = BTreeMap::new();
        for (&commit, ps) in &parents {
            for &p in ps {
                let entry = children.entry(p).or_default();
                if !entry.contains(&commit) {
                    entry.push(commit);
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
    fn strict_descendants(&self, sha: &str) -> BTreeSet<&'a str> {
        let mut out: BTreeSet<&'a str> = BTreeSet::new();
        let mut stack: Vec<&'a str> = self
            .children
            .get(sha)
            .into_iter()
            .flatten()
            .copied()
            .collect();
        while let Some(c) = stack.pop() {
            if out.insert(c) {
                if let Some(kids) = self.children.get(c) {
                    stack.extend(kids.iter().copied());
                }
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

    // Source-link records (ExternalLink nodes, EXTERNAL_HANDLE edges) that were
    // tombstoned must not resolve their task on current-state reads: a retracted
    // external handle is stale, not a live handle.
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();

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

            if matches && !tombstoned.contains(id.as_str()) {
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
            id: edge_id,
            label: EdgeLabel::ExternalHandle,
            source,
            target,
            ..
        } = r
            && matched_links.contains(target)
            && !tombstoned.contains(edge_id.as_str())
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

    // Tombstoned (deleted) tasks are not part of the current state: drop them
    // before reporting ambiguity so a re-created/re-imported task sharing a
    // handle with an older deleted one resolves the live task instead of failing
    // `Ambiguous`.
    matched_ids.retain(|id| !tombstoned.contains(id.as_str()));

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

/// Final `::`-delimited segment of a symbol name or import path, used for the
/// name-based import resolution in change-impact.
fn last_path_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// Final segment of one import item, stripping a trailing `as` alias.
/// Returns `None` for globs (`*`), `self`, or empty items.
fn import_item_name(item: &str) -> Option<&str> {
    let base = item.trim();
    let base = base.split(" as ").next().unwrap_or(base).trim();
    let seg = last_path_segment(base).trim();
    if seg.is_empty() || seg == "*" || seg == "self" {
        None
    } else {
        Some(seg)
    }
}

/// Climb inbound `Defines`/`Contains` edges from an owner node until a File or
/// Module is reached, returning that owner and the edge connecting to it. Used
/// so a method owned by an impl-block `Symbol` resolves to its containing file
/// for `containing_context`. Returns `None` if no File/Module owner is found.
fn containing_file_or_module<'a>(
    start: &'a GraphRecord,
    start_edge: &'a GraphRecord,
    by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    inbound_edges: &BTreeMap<&'a str, Vec<(&'a str, &'a EdgeLabel, &'a str)>>,
) -> Option<(&'a GraphRecord, &'a GraphRecord)> {
    let mut node = start;
    let mut edge = start_edge;
    // Bound the climb so a malformed cyclic ownership chain cannot loop forever.
    for _ in 0..16 {
        match record_node_kind(node) {
            Some(NodeKind::File | NodeKind::Module) => return Some((node, edge)),
            Some(NodeKind::Symbol) => {
                let (parent, parent_edge) = inbound_edges
                    .get(node.id())
                    .into_iter()
                    .flatten()
                    .find(|&&(_, l, _)| matches!(l, EdgeLabel::Defines | EdgeLabel::Contains))
                    .and_then(|&(eid, _, pid)| Some((*by_id.get(pid)?, *by_id.get(eid)?)))?;
                node = parent;
                edge = parent_edge;
            }
            _ => return None,
        }
    }
    None
}

/// Imported symbol names from a `use` path, expanding a brace group and
/// stripping aliases. `a::b::{X, Y as Z}` → `[X, Y]`; `a::b::C` → `[C]`.
fn imported_symbol_names(import_path: &str) -> Vec<&str> {
    let trimmed = import_path.trim();
    trimmed.find('{').map_or_else(
        || import_item_name(trimmed).into_iter().collect(),
        |open| {
            let inner = &trimmed[open + 1..];
            let inner = inner.strip_suffix('}').unwrap_or(inner);
            inner.split(',').filter_map(import_item_name).collect()
        },
    )
}

/// A claim is **verified** when it cites at least one present verification-domain
/// record through an evidence link (`VALIDATED_BY`, `HAS_EVIDENCE`,
/// `PRODUCED_EVIDENCE`) or an equivalent outgoing edge. This is a structural,
/// non-inferential rule over existing contracts — not a truth judgement.
///
/// Shared by the memory-audit `--verified-only` filter and the semantic-memory
/// recall `--verified-only` filter (issue #91) so both surfaces apply the
/// identical rule: a resolvable, non-tombstoned verification record is required;
/// a triple-only citation stub that names no record never counts as verified.
pub(crate) fn is_verified_claim(
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

/// Outgoing edges keyed by source record ID, used for edge-backed verification.
pub(crate) type OutgoingEdgeIndex<'a> = BTreeMap<&'a str, Vec<(&'a EdgeLabel, &'a str)>>;

/// Set of tombstoned record IDs, treated as absent during verification checks.
pub(crate) type TombstonedSet<'a> = BTreeSet<&'a str>;

/// Builds the support indexes [`is_verified_claim`] needs: outgoing edges keyed
/// by source record ID (for edge-backed verification) and the set of tombstoned
/// record IDs (treated as absent). Shared so the semantic-memory recall surface
/// (issue #91) applies the exact rule the memory audit does.
#[must_use]
pub(crate) fn verification_support_indexes(
    records: &[GraphRecord],
) -> (OutgoingEdgeIndex<'_>, TombstonedSet<'_>) {
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();

    let mut edges_from: BTreeMap<&str, Vec<(&EdgeLabel, &str)>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if !tombstoned.contains(id.as_str()) {
                edges_from
                    .entry(source.as_str())
                    .or_default()
                    .push((label, target.as_str()));
            }
        }
    }
    (edges_from, tombstoned)
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

/// Represents a changed file in a commit range.
///
/// Serialization is deliberately bounded to identity/path/span/commit metadata.
/// The backing `GraphRecord` is retained for in-process traversal only and is
/// never emitted, because `File`/`Symbol` summaries from `scan-history` embed
/// normalized source bodies; dumping them would leak whole file/symbol snippets
/// into the response instead of the leads the section promises.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesFileItem<'a> {
    /// The graph record for the file or change (traversal only; not serialized).
    #[serde(skip)]
    pub record: &'a GraphRecord,
    /// Stable record ID of the changed file fact.
    pub record_id: &'a str,
    /// The repository-relative path of the file.
    pub path: &'a str,
    /// Source span of the file fact, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// The Git commit SHA containing this change.
    pub git_commit: &'a str,
}

/// Represents a changed symbol in a commit range.
///
/// Like [`ChangesFileItem`], the raw record is held for traversal but excluded
/// from serialization to keep source bodies out of the response.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesSymbolItem<'a> {
    /// The graph record for the symbol (traversal only; not serialized).
    #[serde(skip)]
    pub record: &'a GraphRecord,
    /// Stable record ID of the changed symbol fact.
    pub record_id: &'a str,
    /// The name of the symbol.
    pub name: &'a str,
    /// The repository-relative path of the symbol definition.
    pub path: &'a str,
    /// Source span of the symbol definition, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// The Git commit SHA containing this change.
    pub git_commit: &'a str,
}

/// Represents a commit in a commit range.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesCommitItem<'a> {
    /// The graph record for the commit.
    pub record: &'a GraphRecord,
    /// The full Git commit SHA.
    pub commit: &'a str,
    /// The commit author timestamp if available.
    pub author_time: Option<&'a str>,
}

/// Represents a tombstone (deleted node marker) associated with a commit range.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct ChangesTombstoneItem<'a> {
    /// The graph record for the tombstone.
    pub record: &'a GraphRecord,
    /// The stable ID of the deleted node.
    pub deleted_id: &'a str,
}

/// Represents a semantic drift record in a commit range.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChangesDriftItem<'a> {
    /// The graph record for the semantic drift.
    pub record: &'a GraphRecord,
    /// The stable ID of the target node.
    pub target_record_id: &'a str,
    /// The computed drift score.
    pub score: f64,
}

/// Represents a changed code fact that lacks explaining cross-domain evidence.
///
/// Bounded by design: identity, kind, path, and commit only. The node `summary`
/// is deliberately omitted because `File`/`Symbol` summaries from `scan-history`
/// embed normalized source bodies, which must not leak into the response.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct UnexplainedChange<'a> {
    /// The stable ID of the unexplained node.
    pub record_id: &'a str,
    /// The node kind (e.g. "Symbol", "File").
    pub kind: &'a str,
    /// The repository-relative path of the unexplained code fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<&'a str>,
    /// The Git commit SHA the unexplained snapshot belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<&'a str>,
}

/// One item in the `observations` section of a query response.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextObservation<'a> {
    pub record_id: &'a str,
    pub kind: &'static str,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_handle: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_links: Vec<&'a EvidenceLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temporal_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

/// One item in the `project_state`, `artifacts`, or `verification_evidence` sections.
#[allow(missing_docs)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct ContextLinkedItem<'a> {
    pub record_id: &'a str,
    pub kind: &'static str,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_quality: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_handle: Option<OutputHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_handle: Option<OutputHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_artifact_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_artifact_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edit_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename_to: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hunk_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_turn_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linked_patch_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_status: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_handle: Option<PatchHandle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_bytes_hash: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_bytes_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_files: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validation_summary: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unknown_base_reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_handle: Option<OutputHandle>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub evidence_links: Vec<&'a EvidenceLink>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_record: Option<Box<Self>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<&'a str>,
}

/// Returns a copy of an `OutputHandle` with the `inline` payload stripped,
/// leaving only the bounded hash/size metadata. Used by the redacted change
/// wrappers so captured stdout/stderr bytes never reach the changes response.
fn output_handle_metadata_only(handle: &OutputHandle) -> OutputHandle {
    OutputHandle {
        inline: None,
        hash: handle.hash.clone(),
        bytes: handle.bytes,
    }
}

/// Returns a copy of a `PatchHandle` with the inline patch bytes stripped,
/// leaving only the stored-path handle.
fn patch_handle_metadata_only(handle: &PatchHandle) -> PatchHandle {
    PatchHandle {
        path: handle.path.clone(),
        inline: None,
    }
}

/// Helper function to convert a GraphRecord to ContextObservation.
#[must_use]
pub fn context_observation(record: &GraphRecord) -> Option<ContextObservation<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        summary,
        text,
        agent_id,
        session_id,
        observed_at,
        confidence,
        failure_kind,
        exit_code,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    Some(ContextObservation {
        record_id: id,
        kind: kind.as_str(),
        summary: summary.to_owned(),
        text: text.as_deref(),
        provenance_handle,
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        failure_kind: failure_kind.as_deref(),
        exit_code: *exit_code,
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        temporal_status: None,
        superseded_by: None,
        contradicted_by: None,
    })
}

/// Helper function to convert a GraphRecord to ContextObservation with redacted payloads.
#[must_use]
pub fn redacted_context_observation(record: &GraphRecord) -> Option<ContextObservation<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        agent_id,
        session_id,
        observed_at,
        confidence,
        failure_kind,
        exit_code,
        evidence_links,
        ..
    } = record
    else {
        return None;
    };
    let provenance_handle = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => Some(format!("{a}:{s}")),
        (Some(a), None) => Some(a.to_owned()),
        _ => None,
    };
    let summary = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(a), Some(s)) => format!("{} by {a}:{s}", kind.as_str()),
        (Some(a), None) => format!("{} by {a}", kind.as_str()),
        _ => kind.as_str().to_owned(),
    };
    Some(ContextObservation {
        record_id: id,
        kind: kind.as_str(),
        summary,
        text: None,
        provenance_handle,
        agent_id: agent_id.as_deref(),
        session_id: session_id.as_deref(),
        observed_at: observed_at.as_deref(),
        confidence: confidence.as_deref(),
        failure_kind: failure_kind.as_deref(),
        exit_code: *exit_code,
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        temporal_status: None,
        superseded_by: None,
        contradicted_by: None,
    })
}

/// Helper function to convert a GraphRecord to ContextLinkedItem.
#[must_use]
pub fn context_linked_item(record: &GraphRecord) -> Option<ContextLinkedItem<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        text,
        summary,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        hunk_count,
        linked_turn_id,
        linked_patch_id,
        patch_status,
        patch_handle,
        patch_bytes_hash,
        patch_bytes_size,
        target_files,
        validation_summary,
        base_commit,
        unknown_base_reason,
        producer_session_id,
        body_handle,
        evidence_links,
        author,
        ..
    } = record
    else {
        return None;
    };
    Some(ContextLinkedItem {
        record_id: id,
        kind: kind.as_str(),
        summary: summary.to_owned(),
        title: title.as_deref(),
        name: name.as_deref(),
        text: text.as_deref(),
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        executed_at: executed_at.as_deref(),
        evidence_quality: evidence_quality.as_deref(),
        stdout_handle: stdout_handle.as_deref().cloned(),
        stderr_handle: stderr_handle.as_deref().cloned(),
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        edit_kind: edit_kind.as_deref(),
        before_hash: before_hash.as_deref(),
        after_hash: after_hash.as_deref(),
        rename_to: rename_to.as_deref(),
        hunk_count: *hunk_count,
        linked_turn_id: linked_turn_id.as_deref(),
        linked_patch_id: linked_patch_id.as_deref(),
        patch_status: patch_status.as_deref(),
        patch_handle: patch_handle.as_deref().cloned(),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        patch_bytes_size: *patch_bytes_size,
        target_files: target_files.as_deref(),
        validation_summary: validation_summary.as_deref(),
        base_commit: base_commit.as_deref(),
        unknown_base_reason: unknown_base_reason.as_deref(),
        producer_session_id: producer_session_id.as_deref(),
        body_handle: body_handle.as_deref().cloned(),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        verification_record: None,
        author: author.as_deref(),
    })
}

/// Helper function to convert a GraphRecord to ContextLinkedItem with redacted payloads.
#[must_use]
pub fn redacted_context_linked_item(record: &GraphRecord) -> Option<ContextLinkedItem<'_>> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        status,
        verification_kind,
        exit_code,
        executed_at,
        evidence_quality,
        stdout_handle,
        stderr_handle,
        source_artifact_path,
        source_artifact_hash,
        repo_relative_path,
        edit_kind,
        before_hash,
        after_hash,
        rename_to,
        hunk_count,
        linked_turn_id,
        linked_patch_id,
        patch_status,
        patch_handle,
        patch_bytes_hash,
        patch_bytes_size,
        target_files,
        base_commit,
        unknown_base_reason,
        producer_session_id,
        body_handle,
        evidence_links,
        author,
        agent_id,
        session_id,
        ..
    } = record
    else {
        return None;
    };
    let display_author = author.as_deref().or(agent_id.as_deref());
    let display_session = session_id.as_deref().or(producer_session_id.as_deref());
    let summary = match (display_author, display_session) {
        (Some(a), Some(s)) => format!("{} by {a}:{s}", kind.as_str()),
        (Some(a), None) => format!("{} by {a}", kind.as_str()),
        _ => kind.as_str().to_owned(),
    };
    Some(ContextLinkedItem {
        record_id: id,
        kind: kind.as_str(),
        summary,
        title: None,
        name: name.as_deref(),
        text: None,
        status: status.as_deref(),
        verification_kind: verification_kind.as_deref(),
        exit_code: *exit_code,
        executed_at: executed_at.as_deref(),
        evidence_quality: evidence_quality.as_deref(),
        stdout_handle: stdout_handle.as_deref().map(output_handle_metadata_only),
        stderr_handle: stderr_handle.as_deref().map(output_handle_metadata_only),
        source_artifact_path: source_artifact_path.as_deref(),
        source_artifact_hash: source_artifact_hash.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        edit_kind: edit_kind.as_deref(),
        before_hash: before_hash.as_deref(),
        after_hash: after_hash.as_deref(),
        rename_to: rename_to.as_deref(),
        hunk_count: *hunk_count,
        linked_turn_id: linked_turn_id.as_deref(),
        linked_patch_id: linked_patch_id.as_deref(),
        patch_status: patch_status.as_deref(),
        patch_handle: patch_handle.as_deref().map(patch_handle_metadata_only),
        patch_bytes_hash: patch_bytes_hash.as_deref(),
        patch_bytes_size: *patch_bytes_size,
        target_files: target_files.as_deref(),
        validation_summary: None,
        base_commit: base_commit.as_deref(),
        unknown_base_reason: unknown_base_reason.as_deref(),
        producer_session_id: producer_session_id.as_deref(),
        body_handle: body_handle.as_deref().map(output_handle_metadata_only),
        evidence_links: evidence_links.as_deref().unwrap_or(&[]).iter().collect(),
        verification_record: None,
        author: author.as_deref(),
    })
}

/// Context of changed facts and trust-separated evidence over a commit range.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ChangesContext<'a> {
    /// All files added, modified, or deleted in the commit range.
    pub changed_files: Vec<ChangesFileItem<'a>>,
    /// All syntax symbols added or modified in the commit range.
    pub changed_symbols: Vec<ChangesSymbolItem<'a>>,
    /// Commits within the range.
    pub commits: Vec<ChangesCommitItem<'a>>,
    /// Deleted code graph node markers within the range.
    pub tombstones: Vec<ChangesTombstoneItem<'a>>,
    /// Semantic drift records within the range.
    pub drift_records: Vec<ChangesDriftItem<'a>>,

    /// Subjective agent observations referencing nodes in the range.
    pub observations: Vec<ContextObservation<'a>>,
    /// Task and project management state referencing nodes in the range.
    pub project_state: Vec<ContextLinkedItem<'a>>,
    /// Persistent generated artifacts referencing nodes in the range.
    pub artifacts: Vec<ContextLinkedItem<'a>>,
    /// Verification runs, proof outcomes, and test results referencing nodes in the range.
    pub verification_evidence: Vec<ContextLinkedItem<'a>>,
    /// Changed code facts that do not map to any explaining evidence.
    pub unexplained: Vec<UnexplainedChange<'a>>,
    /// Citations from observations/tasks to absent target records.
    pub unresolved: Vec<UnresolvedRef>,
}

/// Errors that can occur during commit range query resolution.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum ChangesError {
    /// The specified commit prefix could not be resolved to any commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The specified commit prefix was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// The range is reversed (base is a descendant of head).
    ReversedRange {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// There is no ancestor path between base and head.
    NoPath {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// The store history is empty (no commits present).
    EmptyHistory,
}

/// Query context of changed facts and trust-separated evidence over a commit range.
///
/// Walks the commit topology from `head_prefix` back to `base_prefix`, identifies all
/// code facts changed in that range, and aggregates related cross-domain evidence up to 3 hops.
///
/// # Errors
///
/// Returns a [`ChangesError`] if a commit is missing, ambiguous, or the range is reversed or unconnected.
#[allow(clippy::missing_panics_doc)]
pub fn changes_context<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
) -> Result<ChangesContext<'a>, ChangesError> {
    // 0. Check for empty history
    let has_any_commits = records
        .iter()
        .any(|r| matches!(r.node_kind_name(), Some("Commit")));
    if !has_any_commits {
        return Err(ChangesError::EmptyHistory);
    }

    // Resolve repository ownership only when a scope is requested. In a shared
    // store two repositories can carry the same commit SHA; scoping commit
    // resolution and code-fact selection by owning repository keeps one repo's
    // range from mixing in another's files/symbols/evidence. Cross-domain
    // evidence (agent memory, verification) is intentionally not repo-owned in
    // the containment topology, so the BFS that fans out from in-scope seeds is
    // left unscoped — only the source-fact and commit selection is gated.
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

    // 1. Resolve commit prefixes
    let resolve_prefix = |prefix: &str| -> Result<&'a str, ChangesError> {
        let mut matches = Vec::new();
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                ..
            } = r
            {
                if sha.to_lowercase().starts_with(&prefix.to_lowercase()) && in_scope(r.id()) {
                    matches.push(sha.as_str());
                }
            }
        }
        matches.sort_unstable();
        matches.dedup();

        if matches.is_empty() {
            return Err(ChangesError::MissingCommit {
                commit_prefix: prefix.to_owned(),
            });
        }
        if matches.len() > 1 {
            let string_matches = matches.iter().map(|s| (*s).to_owned()).collect();
            return Err(ChangesError::AmbiguousCommitPrefix {
                commit_prefix: prefix.to_owned(),
                matches: string_matches,
            });
        }
        Ok(matches.into_iter().next().unwrap())
    };

    let base_sha = resolve_prefix(base_prefix)?;
    let head_sha = resolve_prefix(head_prefix)?;

    // 2. Build parent map
    let mut parent_map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut by_id: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut tombstoned_ids = BTreeSet::new();
    let mut has_any_temporal_version = BTreeSet::new();

    for r in records {
        by_id.insert(r.id(), r);
        if let GraphRecord::Tombstone { deleted_id, .. } = r {
            tombstoned_ids.insert(deleted_id.as_str());
        }
        match r {
            GraphRecord::Node {
                id,
                temporal: Some(_),
                ..
            }
            | GraphRecord::Edge {
                id,
                temporal: Some(_),
                ..
            } => {
                has_any_temporal_version.insert(id.as_str());
            }
            _ => {}
        }
    }

    // Commits specify parents via temporal.git_parent_commits or ParentOf edges.
    // When a repository scope is active, only that repository's commit nodes
    // contribute to the topology so a same-SHA commit owned by another repo
    // cannot bleed into the range.
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
        {
            if in_scope(r.id()) {
                let entry = parent_map.entry(sha.as_str()).or_default();
                for parent in &t.git_parent_commits {
                    entry.push(parent.as_str());
                }
            }
        }
        if let GraphRecord::Edge {
            label: EdgeLabel::ParentOf,
            source,
            target,
            ..
        } = r
        {
            if !in_scope(source.as_str()) || !in_scope(target.as_str()) {
                continue;
            }
            if let (Some(parent_node), Some(child_node)) =
                (by_id.get(source.as_str()), by_id.get(target.as_str()))
            {
                if let (
                    GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(psha),
                        ..
                    },
                    GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(csha),
                        ..
                    },
                ) = (parent_node, child_node)
                {
                    let entry = parent_map.entry(csha.as_str()).or_default();
                    entry.push(psha.as_str());
                }
            }
        }
    }

    for parents in parent_map.values_mut() {
        parents.sort_unstable();
        parents.dedup();
    }

    // 3. Compute reachable sets
    let get_reachable = |start_sha: &'a str| -> BTreeSet<&'a str> {
        let mut reachable = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut queue = vec![start_sha];
        while let Some(current) = queue.pop() {
            if !visited.insert(current) {
                continue;
            }
            reachable.insert(current);
            if let Some(parents) = parent_map.get(current) {
                for parent in parents {
                    if !visited.contains(*parent) {
                        queue.push(*parent);
                    }
                }
            }
        }
        reachable
    };

    let reachable_head = get_reachable(head_sha);
    let reachable_base = get_reachable(base_sha);

    // 4. Validate range ancestry
    if !reachable_head.contains(base_sha) {
        if reachable_base.contains(head_sha) {
            return Err(ChangesError::ReversedRange {
                base: base_prefix.to_owned(),
                head: head_prefix.to_owned(),
            });
        }
        return Err(ChangesError::NoPath {
            base: base_prefix.to_owned(),
            head: head_prefix.to_owned(),
        });
    }

    let range_commit_shas: BTreeSet<&str> = reachable_head
        .difference(&reachable_base)
        .copied()
        .collect();

    // 5. Gather code facts in the range.
    //
    // History graphs emit a `File`/`Symbol` snapshot for every path in every
    // commit but only attach a `CHANGED_IN` edge when that path actually changed
    // in the commit. The stable source ID of a `CHANGED_IN` edge is reused by
    // every temporal snapshot of the same path/symbol, so seeding from the edge's
    // source ID alone (or gating on a global "any CHANGED_IN edge exists" flag)
    // would report unchanged base/other-commit snapshots as changed and would
    // disable the commit-membership fallback for ranges that legitimately lack
    // those edges.
    //
    // To stay precise we record the exact `(stable source id, commit sha)` pairs
    // that changed, plus the set of range commits that actually carry CHANGED_IN
    // coverage. A snapshot then counts as changed only when its own
    // `(id, git_commit)` pair is marked, and the commit-membership fallback is
    // applied per-commit for range commits that have no CHANGED_IN edges.
    let mut range_target_commit: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        match r {
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                ..
            } if range_commit_shas.contains(sha.as_str()) && in_scope(r.id()) => {
                range_target_commit.insert(r.id(), sha.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::Change,
                temporal: Some(t),
                ..
            } if range_commit_shas.contains(t.git_commit.as_str()) && in_scope(r.id()) => {
                range_target_commit.insert(r.id(), t.git_commit.as_str());
            }
            _ => {}
        }
    }

    let mut changed_pairs: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut commits_with_changed_in: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label: EdgeLabel::ChangedIn,
            source,
            target,
            ..
        } = r
        {
            let Some(commit) = range_target_commit.get(target.as_str()) else {
                continue;
            };
            // Coverage is recorded from the presence of an in-range CHANGED_IN
            // edge even when that edge is tombstoned: the history format uses
            // CHANGED_IN, so the legacy commit-membership fallback must stay off.
            // Recording coverage only for live edges would let a retracted edge
            // that is a commit's sole marker re-enable the fallback and report the
            // very snapshot whose CHANGED_IN was revoked as changed.
            commits_with_changed_in.insert(*commit);
            // A CHANGED_IN edge retracted by an active tombstone no longer marks
            // its fact as changed (the has_any_temporal_version exception keeps
            // versioned edges live), so it is excluded from changed_pairs while
            // still counting as coverage above.
            if tombstoned_ids.contains(edge_id.as_str())
                && !has_any_temporal_version.contains(edge_id.as_str())
            {
                continue;
            }
            changed_pairs.insert((source.as_str(), *commit));
        }
    }

    // Coverage is decided at the range level, not per commit. If any commit in
    // the range carries CHANGED_IN edges, the history format records them, so we
    // trust them exactly: a snapshot counts as changed only when its own
    // `(id, commit)` pair is marked. A commit that merely re-emits snapshots
    // without a CHANGED_IN edge (e.g. a doc/config-only commit) then contributes
    // nothing here — only its explicit `Change` records surface via pass 2. The
    // commit-membership fallback applies only when the range carries no
    // CHANGED_IN edges at all (history written without them); this also keeps the
    // fallback scoped to the queried range rather than disabled globally by a
    // single CHANGED_IN edge elsewhere in a shared store.
    let range_uses_changed_in = !commits_with_changed_in.is_empty();
    let is_changed_node = |r: &GraphRecord, t: &TemporalMetadata| -> bool {
        if range_uses_changed_in {
            changed_pairs.contains(&(r.id(), t.git_commit.as_str()))
        } else {
            range_commit_shas.contains(t.git_commit.as_str())
        }
    };

    // Symbol-level change gate. `scan-history` adds a `CHANGED_IN` edge for every
    // `Symbol` snapshot in a touched file, not only the symbol whose body the
    // commit actually edited (see `src/history.rs`). Reporting all of them would
    // send agents to inspect unchanged code, contradicting the section contract.
    // We index each symbol snapshot's body (its summary, which the scanner builds
    // deterministically from source bytes with no commit-specific content) by
    // `(stable id, commit)` and treat a snapshot as a real change only when it
    // differs from — or has no — parent-commit snapshot of the same symbol id.
    // The check is conservative: when the parent topology or parent snapshot is
    // unavailable we cannot prove the body is unchanged, so we keep the row.
    let mut symbol_snapshot_bodies: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Symbol,
            temporal: Some(t),
            summary,
            ..
        } = r
        {
            symbol_snapshot_bodies.insert((r.id(), t.git_commit.as_str()), summary.as_str());
        }
    }
    let symbol_body_changed = |id: &str, commit: &str, summary: &str| -> bool {
        let Some(parents) = parent_map.get(commit) else {
            return true;
        };
        if parents.is_empty() {
            return true;
        }
        let mut saw_parent_snapshot = false;
        for parent in parents {
            if let Some(parent_body) = symbol_snapshot_bodies.get(&(id, *parent)) {
                saw_parent_snapshot = true;
                if *parent_body != summary {
                    return true;
                }
            }
        }
        // Every parent snapshot we could find matched this body: unchanged. If we
        // found none, we cannot prove it unchanged, so report it.
        !saw_parent_snapshot
    };

    let mut changed_files = Vec::new();
    let mut changed_symbols = Vec::new();
    let mut commits = Vec::new();
    let mut drift_records = Vec::new();
    let mut changed_paths = BTreeSet::new();
    let mut added_file_commits = BTreeSet::new();

    // Pass 1: commits, file nodes, symbol nodes, drift records
    for r in records {
        match r {
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                temporal,
                ..
            } if range_commit_shas.contains(sha.as_str()) && in_scope(r.id()) => {
                commits.push(ChangesCommitItem {
                    record: r,
                    commit: sha,
                    author_time: temporal.as_ref().and_then(|t| t.author_time.as_deref()),
                });
            }
            GraphRecord::Node {
                kind: NodeKind::File,
                repo_relative_path: Some(path),
                span,
                temporal: Some(t),
                ..
            } if is_changed_node(r, t) && in_scope(r.id()) => {
                changed_files.push(ChangesFileItem {
                    record: r,
                    record_id: r.id(),
                    path,
                    span: *span,
                    git_commit: &t.git_commit,
                });
                added_file_commits.insert((path.as_str(), t.git_commit.as_str()));
                changed_paths.insert(path.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                name: Some(sym_name),
                repo_relative_path: Some(path),
                span,
                temporal: Some(t),
                summary,
                ..
            } if is_changed_node(r, t)
                && in_scope(r.id())
                && symbol_body_changed(r.id(), t.git_commit.as_str(), summary) =>
            {
                changed_symbols.push(ChangesSymbolItem {
                    record: r,
                    record_id: r.id(),
                    name: sym_name,
                    path,
                    span: *span,
                    git_commit: &t.git_commit,
                });
            }
            GraphRecord::Node {
                kind: NodeKind::SemanticDrift,
                temporal,
                semantic_drift: Some(drift),
                ..
            } => {
                let in_range = temporal
                    .as_ref()
                    .is_some_and(|t| range_commit_shas.contains(t.git_commit.as_str()))
                    || range_commit_shas.contains(drift.after_git_commit.as_str());
                if in_range && in_scope(r.id()) {
                    drift_records.push(ChangesDriftItem {
                        record: r,
                        target_record_id: &drift.target_record_id,
                        score: drift.score,
                    });
                }
            }
            _ => {}
        }
    }

    // Pass 2: Change nodes. A `Change` is added to `changed_files` only when a
    // File snapshot did not already cover its `(path, commit)`, but its stable id
    // is always recorded as an evidence seed: `EXPLAINS_CHANGE` citations target
    // the `Change`/`Commit`, so an observation explaining a normal Rust
    // modification (which also has a File snapshot) must still be discovered.
    let mut change_seed_ids = BTreeSet::new();
    // `(path, commit)` → `Change` record id, so the `unexplained` check can carry
    // evidence that targets the per-commit `Change` (which the output BFS already
    // surfaces because the change is seeded) through to the `File`/`Symbol` fact
    // for the same `(path, commit)`. Without this a normal Rust modification whose
    // explaining observation cites the de-duped `Change` would appear with its
    // evidence and still be listed as unexplained.
    let mut change_id_by_path_commit: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    // Repo-relative paths deleted in the range. `scan-history` emits no
    // File/Symbol snapshot at a deletion commit and no CHANGED_IN edge for the
    // deleted path, so evidence that cites the deleted code id from an earlier
    // snapshot needs a bridge into the evidence traversal (built below).
    let mut deletion_paths: BTreeSet<&str> = BTreeSet::new();
    // Commits where a deleted path was last live (the deletion commit's parents).
    // Evidence explaining a deletion is normally anchored to the file's prior
    // live commit, which is out of the queried range, so the in-range anchor
    // filter must additionally admit these commits for deletion-bridge targets.
    let mut deletion_live_commits: BTreeSet<&str> = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Change,
            repo_relative_path: Some(path),
            name,
            span,
            temporal: Some(t),
            ..
        } = r
        {
            if range_commit_shas.contains(t.git_commit.as_str()) && in_scope(r.id()) {
                change_seed_ids.insert(r.id());
                change_id_by_path_commit.insert((path.as_str(), t.git_commit.as_str()), r.id());
                // The Change `name` is "<git status> <path>"; a leading "D"
                // marks a deletion whose prior code ids must be bridged for
                // evidence discovery.
                if name.as_deref().and_then(|n| n.split_whitespace().next()) == Some("D") {
                    deletion_paths.insert(path.as_str());
                    if let Some(parents) = parent_map.get(t.git_commit.as_str()) {
                        deletion_live_commits.extend(parents.iter().copied());
                    }
                }
                if added_file_commits.insert((path.as_str(), t.git_commit.as_str())) {
                    changed_files.push(ChangesFileItem {
                        record: r,
                        record_id: r.id(),
                        path,
                        span: *span,
                        git_commit: &t.git_commit,
                    });
                    changed_paths.insert(path.as_str());
                }
            }
        }
    }

    let mut tombstones = Vec::new();
    for r in records {
        if let GraphRecord::Tombstone {
            deleted_id,
            summary,
            ..
        } = r
        {
            // Tombstones carry no commit and cannot be temporally scoped to the
            // range (tracked separately); but when the deleted node's owning
            // repository is known it must still match the requested scope so a
            // sibling repo's deletions never appear.
            let owner_ok = repo_scope.is_none_or(|scope| {
                repo_index
                    .as_ref()
                    .and_then(|index| index.owner_of(deleted_id))
                    .is_none_or(|owner| owner == scope)
            });
            if owner_ok
                && changed_paths
                    .iter()
                    .any(|path| summary.contains(path) || deleted_id.contains(path))
            {
                tombstones.push(ChangesTombstoneItem {
                    record: r,
                    deleted_id,
                });
            }
        }
    }

    // 6. Gather cross-domain evidence
    let mut seed_ids = BTreeSet::new();
    for item in &changed_files {
        seed_ids.insert(item.record.id());
    }
    for item in &changed_symbols {
        seed_ids.insert(item.record.id());
    }
    for item in &commits {
        seed_ids.insert(item.record.id());
    }
    for item in &drift_records {
        seed_ids.insert(item.record.id());
    }
    for item in &tombstones {
        seed_ids.insert(item.deleted_id);
    }
    // Change nodes are not output facts, but seeding them lets the BFS reach
    // EXPLAINS_CHANGE evidence that targets the change rather than the File/Symbol.
    seed_ids.extend(change_seed_ids.iter().copied());

    // Bridge prior path-backed `File`/`Symbol` ids for deleted paths into the
    // evidence traversal. A deletion has no in-range code snapshot, but earlier
    // snapshots (and the observations/verification that cite their stable ids)
    // remain in the store, so without this an explained deletion would surface no
    // evidence. These ids seed only the evidence BFS — never `seed_ids` — so they
    // are not reported as changed facts and never appear in `unexplained`. Stale
    // out-of-range citations to the reused id are still excluded by
    // `direct_evidence_link_in_range`.
    let mut deletion_bridge_ids: BTreeSet<&str> = BTreeSet::new();
    if !deletion_paths.is_empty() {
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::File | NodeKind::Symbol,
                repo_relative_path: Some(path),
                ..
            } = r
            {
                if deletion_paths.contains(path.as_str()) && in_scope(r.id()) {
                    deletion_bridge_ids.insert(r.id());
                }
            }
        }
    }

    let mut observations = BTreeSet::new();
    let mut project_state = BTreeSet::new();
    let mut artifacts = BTreeSet::new();
    let mut verification_evidence = BTreeSet::new();

    let mut present_ids = BTreeSet::new();
    for r in records {
        present_ids.insert(r.id());
    }
    let mut unresolved = Vec::new();

    let mut visited = seed_ids.clone();
    let mut frontier = seed_ids.clone();
    // Deletion bridges expand the evidence traversal without being reported facts.
    for id in &deletion_bridge_ids {
        if visited.insert(id) {
            frontier.insert(id);
        }
    }
    let mut temporal_evidence_scanned = BTreeSet::new();
    let mut evidence_links_scanned = BTreeSet::new();

    let mut edges_from: BTreeMap<&str, Vec<(EdgeLabel, &str)>> = BTreeMap::new();
    let mut edges_to: BTreeMap<&str, Vec<(EdgeLabel, &str)>> = BTreeMap::new();
    let mut evidence_links_to: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

    // Admits a direct evidence link for indexing/traversal. Anchored links must
    // cite an in-range commit, except that evidence for a bridged deletion target
    // may instead be anchored to the deleted fact's prior live commit (which is
    // out of range by definition). Unanchored links are always admitted.
    let direct_link_admissible = |link: &EvidenceLink| -> bool {
        link.target_git_commit
            .as_deref()
            .or(link.as_of_commit.as_deref())
            .is_none_or(|commit| {
                range_commit_shas.contains(commit)
                    || (link
                        .target_record_id
                        .as_deref()
                        .is_some_and(|t| deletion_bridge_ids.contains(t))
                        && deletion_live_commits.contains(commit))
            })
    };

    for r in records {
        if let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            temporal,
            ..
        } = r
        {
            if tombstoned_ids.contains(edge_id.as_str())
                && !has_any_temporal_version.contains(edge_id.as_str())
            {
                continue;
            }
            // A materialized cross-domain evidence edge (e.g. EXPLAINS_CHANGE,
            // TOUCHED_FILE) carries its commit anchor on temporal metadata. Since
            // history reuses the same stable File/Symbol id across commits, an
            // edge anchored outside the queried range is stale context, so skip it
            // — mirroring `direct_link_admissible` for `EvidenceLink`s. Deletion
            // evidence anchored to the deleted fact's prior live commit (where one
            // endpoint is a deletion-bridge id) is still admitted. Unanchored
            // edges and structural (non-cross-domain) edges are unaffected.
            if is_cross_domain_label(*label) {
                if let Some(t) = temporal {
                    let commit = t.git_commit.as_str();
                    let deletion_ok = (deletion_bridge_ids.contains(source.as_str())
                        || deletion_bridge_ids.contains(target.as_str()))
                        && deletion_live_commits.contains(commit);
                    if !range_commit_shas.contains(commit) && !deletion_ok {
                        continue;
                    }
                }
            }
            edges_from
                .entry(source.as_str())
                .or_default()
                .push((*label, target.as_str()));
            edges_to
                .entry(target.as_str())
                .or_default()
                .push((*label, source.as_str()));
        }
        if let GraphRecord::Node {
            id,
            evidence_links: Some(links),
            ..
        } = r
        {
            for link in links {
                if let Some(tid) = &link.target_record_id {
                    if !direct_link_admissible(link) {
                        continue;
                    }
                    evidence_links_to
                        .entry(tid.as_str())
                        .or_default()
                        .push(id.as_str());
                }
            }
        }
    }

    let classify_and_insert_change = |record_id: &'a str,
                                      observations: &mut BTreeSet<&'a str>,
                                      project_state: &mut BTreeSet<&'a str>,
                                      artifacts: &mut BTreeSet<&'a str>,
                                      verification_evidence: &mut BTreeSet<&'a str>|
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
        match classify_node(*kind) {
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
            _ => false,
        }
    };

    // BFS loop - 3 hops
    for _hop in 0..3 {
        let mut next_frontier = Vec::new();
        for current in frontier {
            if let Some(outs) = edges_from.get(current) {
                for (label, target) in outs {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if visited.insert(*target) {
                        let was_classified = classify_and_insert_change(
                            target,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        if was_classified
                            || is_bfs_relay_node(
                                target,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(*target);
                        }
                    }
                }
            }

            if let Some(ins) = edges_to.get(current) {
                for (label, source) in ins {
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if is_forward_only_label(*label) {
                        continue;
                    }
                    if visited.insert(*source) {
                        let was_classified = classify_and_insert_change(
                            source,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        if was_classified
                            || is_bfs_relay_node(
                                source,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(*source);
                        }
                    }
                }
            }

            if let Some(GraphRecord::Node {
                id: node_id,
                evidence_links: Some(links),
                temporal,
                ..
            }) = by_id.get(current)
            {
                let already_scanned = temporal.as_ref().map_or_else(
                    || !evidence_links_scanned.insert(node_id.as_str()),
                    |t| {
                        let key = format!("{}@{}", node_id, t.git_commit);
                        !temporal_evidence_scanned.insert(key)
                    },
                );
                if !already_scanned {
                    for link in links {
                        if let Some(target_id) = &link.target_record_id {
                            if !direct_link_admissible(link) {
                                continue;
                            }
                            if present_ids.contains(target_id.as_str()) {
                                if visited.insert(target_id.as_str()) {
                                    let was_classified = classify_and_insert_change(
                                        target_id.as_str(),
                                        &mut observations,
                                        &mut project_state,
                                        &mut artifacts,
                                        &mut verification_evidence,
                                    );
                                    if was_classified
                                        || is_bfs_relay_node(
                                            target_id.as_str(),
                                            &by_id,
                                            &tombstoned_ids,
                                            &has_any_temporal_version,
                                        )
                                    {
                                        next_frontier.push(target_id.as_str());
                                    }
                                }
                            } else {
                                unresolved.push(UnresolvedRef {
                                    source_record_id: (*node_id).clone(),
                                    target_handle: target_id.clone(),
                                    relation: link.relation.clone(),
                                    target_domain: link.target_domain.clone(),
                                });
                            }
                        } else if let Some(handle) = evidence_link_triple_handle(link) {
                            // A triple-only citation anchored to an out-of-range
                            // commit is stale context, not an explanation of an
                            // in-range change — mirror the seed-path pass's anchor
                            // check so a stale citation reached here via the BFS is
                            // not re-emitted as unresolved for the new range.
                            let anchor = link
                                .target_git_commit
                                .as_deref()
                                .or(link.as_of_commit.as_deref());
                            if anchor.is_some_and(|commit| !range_commit_shas.contains(commit)) {
                                continue;
                            }
                            unresolved.push(UnresolvedRef {
                                source_record_id: (*node_id).clone(),
                                target_handle: handle,
                                relation: link.relation.clone(),
                                target_domain: link.target_domain.clone(),
                            });
                        }
                    }
                }
            }

            if let Some(sources) = evidence_links_to.get(current) {
                for source in sources {
                    if visited.insert(*source) {
                        let was_classified = classify_and_insert_change(
                            source,
                            &mut observations,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        // A relay such as a ToolCall/AgentTurn can cite a changed
                        // File/Symbol via its own evidence_links; expand it so its
                        // forward PRODUCED_EVIDENCE edges still reach the
                        // CommandRun/TestRun it produced.
                        if was_classified
                            || is_bfs_relay_node(
                                source,
                                &by_id,
                                &tombstoned_ids,
                                &has_any_temporal_version,
                            )
                        {
                            next_frontier.push(*source);
                        }
                    }
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier.into_iter().collect();
    }

    let mut output_observations = Vec::new();
    for id in observations {
        if let Some(rec) = by_id.get(id) {
            if let Some(obs) = redacted_context_observation(rec) {
                output_observations.push(obs);
            }
        }
    }
    let mut output_project_state = Vec::new();
    for id in project_state {
        if let Some(rec) = by_id.get(id) {
            if let Some(item) = redacted_context_linked_item(rec) {
                output_project_state.push(item);
            }
        }
    }
    let mut output_artifacts = Vec::new();
    for id in artifacts {
        if let Some(rec) = by_id.get(id) {
            if let Some(item) = redacted_context_linked_item(rec) {
                output_artifacts.push(item);
            }
        }
    }
    let mut output_verification_evidence = Vec::new();
    for id in verification_evidence {
        if let Some(rec) = by_id.get(id) {
            if let Some(item) = redacted_context_linked_item(rec) {
                output_verification_evidence.push(item);
            }
        }
    }

    // Surface triple-only evidence links (path/span/commit citations with no
    // resolved `target_record_id`) whose cited path matches a changed file or
    // symbol. The BFS above only reaches such citations when their source node is
    // otherwise connected by an edge or a resolved link, so an imported
    // observation that cites a changed path purely by triple — e.g. before
    // `link-evidence` materializes the edge — would be invisible and the change
    // would be reported unexplained even though the graph holds a citation to it.
    // Mirrors `symbol_context`'s seed-path gating.
    let changed_seed_paths: BTreeSet<&str> = changed_files
        .iter()
        .map(|f| f.path)
        .chain(changed_symbols.iter().map(|s| s.path))
        .collect();
    if !changed_seed_paths.is_empty() {
        for r in records {
            let GraphRecord::Node {
                id,
                evidence_links: Some(links),
                ..
            } = r
            else {
                continue;
            };
            if tombstoned_ids.contains(id.as_str())
                && !has_any_temporal_version.contains(id.as_str())
            {
                continue;
            }
            // Under a repository scope, a sibling repo can carry the same path and
            // the same commit SHA. Surface a triple-only citation only when its
            // source node is not owned by a different repository, otherwise a
            // sibling repo's observation would be attached as context for the
            // selected repo's change purely by path/commit match. Records with no
            // owning repository (cross-domain agent memory) are still surfaced.
            let owner_ok = repo_scope.is_none_or(|scope| {
                repo_index
                    .as_ref()
                    .and_then(|index| index.owner_of(id))
                    .is_none_or(|owner| owner == scope)
            });
            if !owner_ok {
                continue;
            }
            for link in links {
                if link.target_record_id.is_some() {
                    continue;
                }
                if !link
                    .target_repo_relative_path
                    .as_deref()
                    .is_some_and(|p| changed_seed_paths.contains(p))
                {
                    continue;
                }
                // If the citation is anchored to a specific commit, only surface
                // it when that commit is within the queried range. A citation to
                // an out-of-range version of the same path is stale context, not
                // an explanation of a change in this range. Exception: a deletion's
                // explaining citation is anchored to the deleted file's prior live
                // commit (out of range); admit it when the cited path is an
                // in-range deletion path, mirroring the direct-link deletion bridge.
                let anchor = link
                    .target_git_commit
                    .as_deref()
                    .or(link.as_of_commit.as_deref());
                if let Some(commit) = anchor {
                    let deletion_ok = link
                        .target_repo_relative_path
                        .as_deref()
                        .is_some_and(|p| deletion_paths.contains(p))
                        && deletion_live_commits.contains(commit);
                    if !range_commit_shas.contains(commit) && !deletion_ok {
                        continue;
                    }
                }
                let Some(handle) = evidence_link_triple_handle(link) else {
                    continue;
                };
                unresolved.push(UnresolvedRef {
                    source_record_id: id.clone(),
                    target_handle: handle,
                    relation: link.relation.clone(),
                    target_domain: link.target_domain.clone(),
                });
            }
        }
    }

    // `commit sha` → `Commit` record id: the second proxy through which an
    // explanation can reach a code fact. An `EXPLAINS_CHANGE` link may target the
    // seeded `Commit` rather than the per-path `Change`.
    let commit_id_by_sha: BTreeMap<&str, &str> =
        commits.iter().map(|c| (c.commit, c.record.id())).collect();

    let linked = |id: &'a str| -> bool {
        is_linked_to_evidence(
            id,
            &by_id,
            &edges_from,
            &edges_to,
            &evidence_links_to,
            &tombstoned_ids,
            &has_any_temporal_version,
            &range_commit_shas,
        )
    };

    let mut unexplained = Vec::new();
    for seed_id in &seed_ids {
        if linked(seed_id) {
            continue;
        }
        let Some(GraphRecord::Node {
            kind,
            repo_relative_path,
            temporal,
            ..
        }) = by_id.get(seed_id)
        else {
            continue;
        };
        if !matches!(kind, NodeKind::File | NodeKind::Symbol) {
            continue;
        }
        // The output BFS seeds the per-commit `Change` and `Commit` for this
        // `(path, commit)`, so an observation that explains the change via either
        // is already emitted. Mirror that here: a code fact whose `Change` or
        // `Commit` proxy carries the evidence is explained, even when no
        // cross-domain edge touches the `File`/`Symbol` node directly. Without
        // this, a normal modification would appear with its explanation and still
        // be reported unexplained.
        let explained_via_proxy = temporal.as_ref().is_some_and(|t| {
            let commit = t.git_commit.as_str();
            let change_proxy = repo_relative_path
                .as_deref()
                .and_then(|p| change_id_by_path_commit.get(&(p, commit)).copied());
            let commit_proxy = commit_id_by_sha.get(commit).copied();
            change_proxy.into_iter().chain(commit_proxy).any(&linked)
        });
        if explained_via_proxy {
            continue;
        }
        unexplained.push(UnexplainedChange {
            record_id: seed_id,
            kind: kind.as_str(),
            path: repo_relative_path.as_deref(),
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
        });
    }

    changed_files.sort_by(|a, b| {
        a.path
            .cmp(b.path)
            .then_with(|| a.git_commit.cmp(b.git_commit))
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    changed_symbols.sort_by(|a, b| {
        a.name
            .cmp(b.name)
            .then_with(|| a.path.cmp(b.path))
            .then_with(|| a.git_commit.cmp(b.git_commit))
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    commits.sort_by(|a, b| a.commit.cmp(b.commit));
    tombstones.sort_by(|a, b| {
        a.deleted_id
            .cmp(b.deleted_id)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    drift_records.sort_by(|a, b| {
        a.target_record_id
            .cmp(b.target_record_id)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

    output_observations.sort_by(|a, b| a.record_id.cmp(b.record_id));
    output_project_state.sort_by(|a, b| a.record_id.cmp(b.record_id));
    output_artifacts.sort_by(|a, b| a.record_id.cmp(b.record_id));
    output_verification_evidence.sort_by(|a, b| a.record_id.cmp(b.record_id));
    unexplained.sort_by(|a, b| a.record_id.cmp(b.record_id));
    unresolved.sort_by(|a, b| {
        a.source_record_id
            .cmp(&b.source_record_id)
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
            .then_with(|| a.target_domain.cmp(&b.target_domain))
    });
    // The same triple citation can be reached by both the BFS and the seed-path
    // pass above; collapse exact duplicates after sorting.
    unresolved.dedup();

    Ok(ChangesContext {
        changed_files,
        changed_symbols,
        commits,
        tombstones,
        drift_records,
        observations: output_observations,
        project_state: output_project_state,
        artifacts: output_artifacts,
        verification_evidence: output_verification_evidence,
        unexplained,
        unresolved,
    })
}

/// Returns false when a direct evidence link is anchored (`target_git_commit` or
/// `as_of_commit`) to a commit outside the queried range. History snapshots reuse
/// the same `File`/`Symbol` id across commits, so a citation anchored to an
/// out-of-range version of a reused id is stale context, not an explanation of an
/// in-range change; indexing or traversing it would let stale evidence both
/// surface and mark the new change explained. Unanchored links are unaffected —
/// their per-commit attribution is tracked as a separate follow-up.
fn direct_evidence_link_in_range(link: &EvidenceLink, range_commit_shas: &BTreeSet<&str>) -> bool {
    link.target_git_commit
        .as_deref()
        .or(link.as_of_commit.as_deref())
        .is_none_or(|commit| range_commit_shas.contains(commit))
}

// Internal evidence-traversal helper: every argument is a borrowed slice of the
// caller's traversal context (indexes plus the queried range), so threading them
// individually is clearer than introducing a context struct used in one place.
#[allow(clippy::too_many_arguments)]
fn is_linked_to_evidence<'a>(
    seed_id: &'a str,
    by_id: &BTreeMap<&'a str, &'a GraphRecord>,
    edges_from: &BTreeMap<&'a str, Vec<(EdgeLabel, &'a str)>>,
    edges_to: &BTreeMap<&'a str, Vec<(EdgeLabel, &'a str)>>,
    evidence_links_to: &BTreeMap<&'a str, Vec<&'a str>>,
    tombstoned_ids: &BTreeSet<&'a str>,
    has_any_temporal_version: &BTreeSet<&'a str>,
    range_commit_shas: &BTreeSet<&'a str>,
) -> bool {
    let mut visited = BTreeSet::new();
    let mut frontier = vec![seed_id];
    visited.insert(seed_id);

    // A node counts as explaining evidence only when it classifies into a
    // non-source output section AND has not been retracted by a current-state
    // tombstone — mirroring `classify_and_insert_change` in the output BFS so the
    // `unexplained` verdict never relies on evidence the output never emits.
    let is_evidence = |node_id: &str| -> bool {
        if node_id == seed_id {
            return false;
        }
        if tombstoned_ids.contains(node_id) && !has_any_temporal_version.contains(node_id) {
            return false;
        }
        if let Some(GraphRecord::Node { kind, .. }) = by_id.get(node_id) {
            return classify_node(*kind).is_some_and(|s| s != ContextSection::SourceFact);
        }
        false
    };

    // Only relay nodes (ToolCall/AgentTurn/AgentRun) may bridge to a further hop,
    // matching the output BFS. A non-relay, non-evidence node such as a Commit or
    // Repository must not be traversed through here, otherwise this helper could
    // mark a change explained by an Observation that the output traversal would
    // never reach (and therefore never emit).
    let can_relay = |node_id: &str| -> bool {
        is_bfs_relay_node(node_id, by_id, tombstoned_ids, has_any_temporal_version)
    };

    for _hop in 0..3 {
        let mut next_frontier = Vec::new();
        for current in frontier {
            if let Some(outs) = edges_from.get(current) {
                for (label, target) in outs {
                    let target = *target;
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if visited.insert(target) {
                        if is_evidence(target) {
                            return true;
                        }
                        if can_relay(target) {
                            next_frontier.push(target);
                        }
                    }
                }
            }

            if let Some(ins) = edges_to.get(current) {
                for (label, source) in ins {
                    let source = *source;
                    if !is_cross_domain_label(*label) {
                        continue;
                    }
                    if is_forward_only_label(*label) {
                        continue;
                    }
                    if visited.insert(source) {
                        if is_evidence(source) {
                            return true;
                        }
                        if can_relay(source) {
                            next_frontier.push(source);
                        }
                    }
                }
            }

            if let Some(GraphRecord::Node {
                evidence_links: Some(links),
                ..
            }) = by_id.get(current)
            {
                for link in links {
                    if let Some(target_id) = &link.target_record_id {
                        if !direct_evidence_link_in_range(link, range_commit_shas) {
                            continue;
                        }
                        if visited.insert(target_id.as_str()) {
                            if is_evidence(target_id.as_str()) {
                                return true;
                            }
                            if can_relay(target_id.as_str()) {
                                next_frontier.push(target_id.as_str());
                            }
                        }
                    }
                }
            }

            if let Some(sources) = evidence_links_to.get(current) {
                for source in sources {
                    let source = *source;
                    if visited.insert(source) {
                        if is_evidence(source) {
                            return true;
                        }
                        if can_relay(source) {
                            next_frontier.push(source);
                        }
                    }
                }
            }
        }
        if next_frontier.is_empty() {
            break;
        }
        frontier = next_frontier;
    }
    false
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
    // A record/edge that still has a temporal (history) version is not deleted
    // for history-bearing reads: a current-state tombstone only retires the
    // current state, so failure history for moved/deleted code stays reachable
    // (mirrors `symbol_context`).
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
        if deleted(handle) {
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
                // The project graph also represents AC ownership with
                // `AcceptanceCriterion --OWNED_BY_TASK--> Task` edges (as consumed
                // by `task_evidence_context`); include ACs connected only by the
                // edge, without the denormalized `parent_task_id` field.
                for r in records {
                    if let GraphRecord::Edge {
                        id: edge_id,
                        label: EdgeLabel::OwnedByTask,
                        source,
                        target,
                        ..
                    } = r
                        && live.contains(target)
                        && !tombstoned.contains(edge_id.as_str())
                        && !tombstoned.contains(source.as_str())
                    {
                        anchor_ids.insert(source.clone());
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
            if deleted(id.as_str()) {
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
            if deleted(id.as_str()) {
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

    // 5) Source / provenance handle naming failures directly. A handle that is
    //    itself a tombstoned record ID (e.g. a retracted AgentSession) is stale —
    //    its live child evidence must not resurrect it as a source target.
    let mut seeds: BTreeSet<String> = BTreeSet::new();
    if deleted(handle) {
        saw_tombstoned = true;
    } else {
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
                && (source_handle.as_deref() == Some(handle)
                    || source_artifact_path.as_deref() == Some(handle)
                    || source_artifact_hash.as_deref() == Some(handle)
                    || session_id.as_deref() == Some(handle))
            {
                if deleted(id.as_str()) {
                    saw_tombstoned = true;
                } else {
                    seeds.insert(id.clone());
                }
            }
        }
        // If the handle is an `AgentSession` record ID, resolve the failures
        // authored in that session even when provenance lives only in
        // `AUTHORED_BY` edges or the session_id value differs from the record ID
        // (the command emits these record IDs as citable provenance).
        let session_key = records.iter().find_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::AgentSession,
                session_id,
                name,
                ..
            } if id == handle => Some(session_id.clone().or_else(|| name.clone())),
            _ => None,
        });
        if let Some(key) = session_key {
            if let Some(k) = key.as_deref() {
                for r in records {
                    if let GraphRecord::Node {
                        id,
                        kind,
                        session_id: Some(sid),
                        ..
                    } = r
                        && (matches!(kind, NodeKind::Failure) || is_verification_kind(*kind))
                        && sid == k
                        && !deleted(id.as_str())
                    {
                        seeds.insert(id.clone());
                    }
                }
            }
            let authored_sources: BTreeSet<&str> = records
                .iter()
                .filter_map(|r| match r {
                    GraphRecord::Edge {
                        id: eid,
                        label: EdgeLabel::AuthoredBy,
                        source,
                        target,
                        ..
                    } if target == handle && !deleted(eid.as_str()) => Some(source.as_str()),
                    _ => None,
                })
                .collect();
            if !authored_sources.is_empty() {
                for r in records {
                    if let GraphRecord::Node { id, kind, .. } = r
                        && authored_sources.contains(id.as_str())
                        && (matches!(kind, NodeKind::Failure) || is_verification_kind(*kind))
                        && !deleted(id.as_str())
                    {
                        seeds.insert(id.clone());
                    }
                }
            }
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
        saw_tombstoned || deleted(handle),
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
    // History-bearing reads keep records/edges that have a temporal version even
    // when a current-state tombstone shares their ID, so failure links for
    // moved/deleted code remain traversable (mirrors `symbol_context`).
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
    let present = |id: &str| -> Option<&'a GraphRecord> {
        if deleted(id) {
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
                // current-state reads, matching `symbol_context`'s convention —
                // unless the edge has a temporal version (history read).
                if deleted(id.as_str()) {
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
            } else if deleted(a.as_str()) {
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
            // `resolved_by` is the pass that *first* resolved the failure — the
            // earliest later success — not the most recent run. Tie-break by ID.
            let better = match best {
                None => true,
                Some((bt, bid)) => (*instant, *sid) < (bt, bid),
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
        let Some(t) = present(id) else {
            return;
        };
        match record_node_kind(t) {
            Some(k) if is_codegraph_kind(k) || is_project_kind(k) => {
                out.insert(id);
            }
            // Relay through a patch artifact to the file(s) it touched, the same
            // `Failure --FAILED_ON--> PatchArtifact --TOUCHED_FILE--> File` shape a
            // file query relays in reverse, so a source query on a patch-invalid
            // failure still anchors on the touched file.
            Some(NodeKind::PatchArtifact) => {
                if let Some(patch_edges) = edges_from.get(id) {
                    for (plabel, pt) in patch_edges {
                        if matches!(plabel, EdgeLabel::TouchedFile)
                            && present(pt).is_some_and(|n| {
                                matches!(record_node_kind(n), Some(NodeKind::File))
                            })
                        {
                            out.insert(pt);
                        }
                    }
                }
            }
            _ => {}
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
    // (relation, target_id) from both graph edges and denormalized links. A
    // failure cites its patch/runtime evidence via PRODUCED_PATCH or FAILED_ON,
    // or via the verification-evidence relations PRODUCED_EVIDENCE / HAS_EVIDENCE
    // / VALIDATED_BY, so all are followed.
    let mut links: Vec<(&'a str, &'a str)> = Vec::new();
    if let Some(edges) = edges_from.get(failure_id) {
        for (label, target) in edges {
            if matches!(
                label,
                EdgeLabel::ProducedPatch
                    | EdgeLabel::FailedOn
                    | EdgeLabel::ProducedEvidence
                    | EdgeLabel::HasEvidence
                    | EdgeLabel::ValidatedBy
            ) {
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
            if matches!(
                link.relation.as_str(),
                "PRODUCED_PATCH"
                    | "FAILED_ON"
                    | "PRODUCED_EVIDENCE"
                    | "HAS_EVIDENCE"
                    | "VALIDATED_BY"
            ) && let Some(t) = link.target_record_id.as_deref()
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
            // A tombstoned (deleted) intermediate provenance node must not relay
            // through to a live session/agent: stop the walk at it rather than
            // enqueueing and continuing along its outgoing edges.
            let Some(node) = present(target) else {
                continue;
            };
            if !visited.contains(*target) {
                frontier.push(target);
            }
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

// ============================================================================
// Change-impact query (issue #76)
// ============================================================================

/// Direction of traversal for one impact lead.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ImpactDirection {
    /// The reached node points *into* the anchor (e.g. a caller of the anchor).
    Inbound,
    /// The anchor points *out* to the reached node (e.g. a callee).
    Outbound,
}

impl ImpactDirection {
    /// Stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "inbound",
            Self::Outbound => "outbound",
        }
    }
}

/// One graph-derived impact lead.
///
/// A node reachable from the anchor via a code-topology edge, tagged with the
/// relation, direction, and hop count. Every row is a LEAD to inspect before
/// editing — not proof of breakage.
#[derive(Debug, Clone)]
pub struct ImpactLead<'a> {
    /// The reached code-graph node (Symbol, File, or Module).
    pub record: &'a GraphRecord,
    /// The connecting edge record (for its stable ID and temporal metadata).
    pub edge: &'a GraphRecord,
    /// The EdgeLabel wire string (e.g. "CALLS", "REFERENCES").
    pub relation: &'static str,
    /// Whether the edge is inbound or outbound relative to the anchor.
    pub direction: ImpactDirection,
    /// The anchor record ID that was used as the traversal seed.
    pub anchor_id: &'a str,
    /// Hop distance from the seed anchor (1-based).
    pub hop: usize,
}

/// Truncation metadata emitted when a per-group cap is hit (AC6).
#[derive(Debug, Clone)]
pub struct ImpactTruncation {
    /// Group label (e.g. "direct_callers").
    pub group: &'static str,
    /// Number of leads returned (after cap).
    pub returned: usize,
    /// Total candidates seen before capping.
    pub total: usize,
    /// The depth parameter in effect when the cap fired.
    pub depth: usize,
}

/// Structured change-impact context returned by [`change_impact_context`].
///
/// Every lead vector is canonically ordered by (record_id, edge_id) for
/// determinism (AC7). Absent sections are empty vecs, never omitted, so a
/// consumer can distinguish "checked, none found" from "class was dropped".
#[derive(Debug, Default)]
pub struct ChangeImpactContext<'a> {
    /// Resolved handle type ("symbol" / "file").
    pub target_kind: &'static str,
    /// Resolved anchor record IDs, canonically sorted.
    pub target_ids: Vec<String>,
    /// Symbols that directly call the anchor (inbound `CALLS` edges).
    pub direct_callers: Vec<ImpactLead<'a>>,
    /// Symbols the anchor calls directly (outbound `CALLS` edges).
    pub direct_callees: Vec<ImpactLead<'a>>,
    /// Symbols or files that reference or import the anchor (inbound
    /// `REFERENCES` edges; outbound handled for completeness).
    pub referencing_files: Vec<ImpactLead<'a>>,
    /// Symbols related through `IMPLEMENTS` edges (both directions).
    pub implementation_symbols: Vec<ImpactLead<'a>>,
    /// The containing file/module (inbound `DEFINES`/`CONTAINS` edges).
    pub containing_context: Vec<ImpactLead<'a>>,
    /// Stable machine-readable diagnostics (unresolved edges, unsupported
    /// relations, truncation notices).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// Depth parameter used.
    pub depth: usize,
    /// Per-group truncation records when the lead cap was hit.
    pub truncations: Vec<ImpactTruncation>,
}

/// Maximum impact leads per group before the truncation diagnostic fires.
const MAX_LEADS_PER_GROUP: usize = 200;

/// Code-topology edge labels that the change-impact traversal classifies.
const IMPACT_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Calls,
    EdgeLabel::References,
    EdgeLabel::Imports,
    EdgeLabel::Implements,
    EdgeLabel::Defines,
    EdgeLabel::Contains,
];

/// First resolved change-impact anchor whose node kind is **not** a code
/// `Symbol` or `File`, if any.
///
/// `change-impact` accepts only symbol and file handles. A canonical codegraph
/// ID that resolves to a `Repository`, `Module`, `Import`, `Commit`, `Change`,
/// or any other node kind maps to [`FailureTargetKind::Symbol`] during handle
/// resolution, so it must be rejected here rather than traversed as an empty
/// `symbol` result. Returns `None` when every anchor is a `Symbol` or `File`.
#[must_use]
pub fn change_impact_unsupported_anchor_kind(
    records: &[GraphRecord],
    target: &ResolvedFailureTarget,
) -> Option<NodeKind> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    target
        .anchor_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .filter_map(record_node_kind)
        .find(|kind| !matches!(kind, NodeKind::Symbol | NodeKind::File))
}

/// Compute graph-derived change-impact leads for a resolved code handle.
///
/// `target` is produced by [`resolve_failure_handle`] (which implements the
/// full handle resolution contract for AC2). The traversal is bounded by
/// `depth` hops from the anchor set. Every result group is canonically sorted
/// for determinism (AC7); missing edge targets produce diagnostics rather than
/// silently dropping relationship classes (AC6/AC8). When `repo_scope` is set
/// (the caller passed `--repo`), `repo_index` constrains the name-based import
/// resolution to the queried anchors' repositories; an unscoped query does not
/// repo-filter imports.
#[must_use]
#[allow(clippy::too_many_lines, clippy::similar_names)]
pub fn change_impact_context<'a>(
    records: &'a [GraphRecord],
    target: &ResolvedFailureTarget,
    depth: usize,
    repo_index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> ChangeImpactContext<'a> {
    fn drain_sorted<'a>(
        map: BTreeMap<(&'a str, &'a str), ImpactLead<'a>>,
        group: &'static str,
        cap: usize,
        depth: usize,
        truncations: &mut Vec<ImpactTruncation>,
    ) -> Vec<ImpactLead<'a>> {
        let total = map.len();
        let mut leads: Vec<ImpactLead<'a>> = map.into_values().collect();
        // Order by hop distance first so that, when a group exceeds the cap, the
        // nearest (most immediate) impact leads are preserved and a large
        // further-out neighborhood cannot evict first-hop callers/callees.
        // `(record_id, edge_id)` breaks ties for byte-stable output.
        leads.sort_by(|a, b| {
            a.hop
                .cmp(&b.hop)
                .then_with(|| a.record.id().cmp(b.record.id()))
                .then_with(|| a.edge.id().cmp(b.edge.id()))
        });
        let returned = leads.len().min(cap);
        if total > cap {
            truncations.push(ImpactTruncation {
                group,
                returned,
                total,
                depth,
            });
        }
        leads.into_iter().take(cap).collect()
    }

    // ── tombstone / temporal filtering (mirrors resolve_failure_handle) ────────
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
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| {
            let id = r.id();
            if deleted(id) { None } else { Some((id, r)) }
        })
        .collect();

    // ── edge indexes (inbound and outbound, code-topology labels only) ─────────
    // outbound_edges[source_id] = Vec<(edge_record_id, label, target_id)>
    let mut outbound_edges: BTreeMap<&str, Vec<(&str, &EdgeLabel, &str)>> = BTreeMap::new();
    // inbound_edges[target_id] = Vec<(edge_record_id, label, source_id)>
    let mut inbound_edges: BTreeMap<&str, Vec<(&str, &EdgeLabel, &str)>> = BTreeMap::new();

    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = r
        {
            if deleted(id.as_str()) {
                continue;
            }
            if !IMPACT_LABELS.contains(label) {
                continue;
            }
            outbound_edges.entry(source.as_str()).or_default().push((
                id.as_str(),
                label,
                target.as_str(),
            ));
            inbound_edges.entry(target.as_str()).or_default().push((
                id.as_str(),
                label,
                source.as_str(),
            ));
        }
    }

    let target_kind = match target.kind {
        FailureTargetKind::File => "file",
        FailureTargetKind::Task | FailureTargetKind::Source | FailureTargetKind::Symbol => "symbol",
    };
    let target_ids: Vec<String> = target.anchor_ids.iter().cloned().collect();

    let mut direct_callers: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut direct_callees: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut referencing_files: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut implementation_symbols: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut containing_context: BTreeMap<(&str, &str), ImpactLead<'_>> = BTreeMap::new();
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    // The queried target's own resolved anchor(s). These are never reported as
    // their own impact leads — a back-edge such as a `caller → anchor` CALLS
    // edge, traversed at depth ≥ 2, would otherwise surface the anchor under
    // `direct_callees`, which is not a lead to inspect.
    let original_targets: BTreeSet<&str> = target
        .anchor_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).map(|r| r.id()))
        .collect();

    // ── BFS frontier ──────────────────────────────────────────────────────────
    // For a File handle, seed the symbols it defines/contains so that callers of
    // those symbols are reachable at hop 1 (mirrors subsystem/semantic-context).
    // Follow nested containers transitively — modules (File CONTAINS Module
    // DEFINES fn) and impl-block Symbols whose methods are emitted beneath them —
    // so every symbol declared in the file is seeded. A Symbol handle seeds only
    // itself: its owned children (e.g. an impl block's methods) are not the
    // queried symbol, so their callers/callees must not be reported as direct
    // leads.
    let seed_descendants = matches!(target.kind, FailureTargetKind::File);
    let mut frontier: BTreeSet<&str> = BTreeSet::new();
    for anchor_id in &target.anchor_ids {
        if let Some(id_ref) = by_id.get(anchor_id.as_str()).map(|r| r.id()) {
            frontier.insert(id_ref);
            if !seed_descendants {
                continue;
            }
            let mut containers: Vec<&str> = vec![id_ref];
            let mut expanded: BTreeSet<&str> = BTreeSet::new();
            while let Some(container) = containers.pop() {
                if !expanded.insert(container) {
                    continue;
                }
                #[allow(clippy::map_unwrap_or)]
                for &(_, label, child_id) in outbound_edges
                    .get(container)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    if !matches!(label, EdgeLabel::Defines | EdgeLabel::Contains) {
                        continue;
                    }
                    // Seed the child and recurse into anything it owns. Modules
                    // own nested symbols; impl-block Symbols own their method
                    // Symbols (emitted via `owner_id()`), so a file handle reaches
                    // methods defined in the file.
                    if matches!(
                        by_id.get(child_id).copied().and_then(record_node_kind),
                        Some(NodeKind::Symbol | NodeKind::Module)
                    ) {
                        frontier.insert(child_id);
                        containers.push(child_id);
                    }
                }
            }
        }
    }

    // The target's own anchors and seeded symbols. `containing_context` is
    // reported only for these so the group reflects the target's container,
    // never an intermediate caller/callee file reached at depth ≥ 2.
    let seed_set: BTreeSet<&str> = frontier.iter().copied().collect();

    let mut visited: BTreeSet<&str> = BTreeSet::new();

    for hop in 1..=depth {
        // Stop as soon as the frontier is exhausted so a very large `--depth`
        // does not spin through empty iterations after traversal is complete.
        if frontier.is_empty() {
            break;
        }
        let current_frontier: Vec<&str> = frontier.iter().copied().collect();
        let mut next_frontier: BTreeSet<&str> = BTreeSet::new();

        for &anchor_id in &current_frontier {
            if visited.contains(anchor_id) {
                continue;
            }
            visited.insert(anchor_id);

            // ── Inbound edges ─────────────────────────────────────────────────
            #[allow(clippy::map_unwrap_or)]
            for &(edge_id, label, source_id) in inbound_edges
                .get(anchor_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(&edge_record) = by_id.get(edge_id) else {
                    continue;
                };
                match label {
                    EdgeLabel::Calls => {
                        // caller → anchor: the source is a direct caller
                        match by_id.get(source_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                direct_callers
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "CALLS",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand callers at next hop (only symbols)
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            // The queried target itself is not a lead about itself.
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "CALLS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::References => {
                        // referencing symbol → anchor
                        match by_id.get(source_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                referencing_files.entry((node.id(), edge_id)).or_insert(
                                    ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "REFERENCES",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    },
                                );
                                // Expand referencing symbols at the next hop so a
                                // wider `--depth` reaches their callers/referrers
                                // (reached symbol nodes expand; file/module owners
                                // do not).
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "REFERENCES".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::Imports => {
                        // file → import: if a file node imports something referencing this anchor
                        match by_id.get(source_id) {
                            Some(&node)
                                if matches!(
                                    record_node_kind(node),
                                    Some(NodeKind::File | NodeKind::Module)
                                ) =>
                            {
                                referencing_files.entry((node.id(), edge_id)).or_insert(
                                    ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "IMPORTS",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    },
                                );
                            }
                            Some(_) => {
                                // Imports from non-file/module source: emit unsupported diagnostic
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unsupported_relation".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "IMPORTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "IMPORTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::Implements => {
                        // impl_sym → anchor (anchor is the trait)
                        match by_id.get(source_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                implementation_symbols
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "IMPLEMENTS",
                                        direction: ImpactDirection::Inbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand implementation symbols at the next hop so
                                // a wider `--depth` reaches their callers/callees.
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: source_id.to_owned(),
                                    relation: "IMPLEMENTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    EdgeLabel::Defines | EdgeLabel::Contains => {
                        // owner → anchor: containing file/module context.
                        //
                        // Only report the container of the *queried target*
                        // (its own anchors/seeded symbols), never of an
                        // intermediate caller/callee reached at depth ≥ 2, and
                        // only when the owner is a File or Module (a
                        // `Repository CONTAINS File` owner is not containing
                        // code context). Key by owner record id (not edge id)
                        // so a file handle that seeds every defined symbol
                        // reports each owner once.
                        if seed_set.contains(anchor_id) {
                            match by_id.get(source_id) {
                                Some(&node) => {
                                    // Resolve the owner to its File/Module context,
                                    // climbing an impl-block Symbol owner up to the
                                    // file that contains it (a method's container is
                                    // its file, not the impl). A Repository owner
                                    // resolves to nothing and is not reported.
                                    if let Some((ctx, ctx_edge)) = containing_file_or_module(
                                        node,
                                        edge_record,
                                        &by_id,
                                        &inbound_edges,
                                    ) {
                                        let relation = match ctx_edge {
                                            GraphRecord::Edge { label: l, .. } => l.as_str(),
                                            _ => label.as_str(),
                                        };
                                        containing_context.entry((ctx.id(), ctx.id())).or_insert(
                                            ImpactLead {
                                                record: ctx,
                                                edge: ctx_edge,
                                                relation,
                                                direction: ImpactDirection::Inbound,
                                                anchor_id,
                                                hop,
                                            },
                                        );
                                    }
                                }
                                None => {
                                    diagnostics.push(MemoryAuditDiagnostic {
                                        code: "unresolved_edge_target".to_owned(),
                                        source_record_id: edge_id.to_owned(),
                                        target_handle: source_id.to_owned(),
                                        relation: label.as_str().to_owned(),
                                        target_domain: "codegraph".to_owned(),
                                    });
                                }
                            }
                        }
                    }
                    _ => {
                        // Unexpected in-scope label — emit diagnostic
                        diagnostics.push(MemoryAuditDiagnostic {
                            code: "unsupported_relation".to_owned(),
                            source_record_id: edge_id.to_owned(),
                            target_handle: source_id.to_owned(),
                            relation: label.as_str().to_owned(),
                            target_domain: "codegraph".to_owned(),
                        });
                    }
                }
            }

            // ── Outbound edges ────────────────────────────────────────────────
            #[allow(clippy::map_unwrap_or)]
            for &(edge_id, label, target_id) in outbound_edges
                .get(anchor_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                let Some(&edge_record) = by_id.get(edge_id) else {
                    continue;
                };
                match label {
                    EdgeLabel::Calls => {
                        // anchor → callee
                        match by_id.get(target_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                direct_callees
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "CALLS",
                                        direction: ImpactDirection::Outbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand callees at next hop (only symbols)
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            // The queried target itself is not a lead about itself.
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: target_id.to_owned(),
                                    relation: "CALLS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    // Outbound References are the anchor's own dependencies, not
                    // code that points at it. referencing_files is documented as
                    // inbound-only, so outbound references are intentionally not
                    // emitted there (they fall through to the `_` arm below).
                    EdgeLabel::Implements => {
                        // anchor → trait (anchor is an impl block)
                        match by_id.get(target_id) {
                            Some(&node) if !original_targets.contains(node.id()) => {
                                implementation_symbols
                                    .entry((node.id(), edge_id))
                                    .or_insert(ImpactLead {
                                        record: node,
                                        edge: edge_record,
                                        relation: "IMPLEMENTS",
                                        direction: ImpactDirection::Outbound,
                                        anchor_id,
                                        hop,
                                    });
                                // Expand implementation/trait symbols at the next
                                // hop so a wider `--depth` reaches their neighbors.
                                if hop < depth
                                    && matches!(
                                        record_node_kind(node),
                                        Some(NodeKind::Symbol | NodeKind::Module)
                                    )
                                {
                                    next_frontier.insert(node.id());
                                }
                            }
                            Some(_) => {}
                            None => {
                                diagnostics.push(MemoryAuditDiagnostic {
                                    code: "unresolved_edge_target".to_owned(),
                                    source_record_id: edge_id.to_owned(),
                                    target_handle: target_id.to_owned(),
                                    relation: "IMPLEMENTS".to_owned(),
                                    target_domain: "codegraph".to_owned(),
                                });
                            }
                        }
                    }
                    // Defines/Contains/Imports outbound = children or import targets,
                    // not inbound leads from the anchor's perspective.
                    _ => {}
                }
            }
        }

        frontier = next_frontier;
    }

    // ── Import resolution (name-based) ─────────────────────────────────────────
    // The Rust extractor records `use` imports as `File/Module --IMPORTS--> Import`
    // nodes (and `Symbol --IMPORTS--> Import` for imports local to an impl), whose
    // name is the imported path; there is no structural edge from the Import node
    // to the symbol it imports. Connect them by matching each imported final path
    // segment — grouped (`a::{X, Y}`) and aliased (`X as Y`) imports expanded — to
    // a seeded anchor symbol's name, then report the importing file/module/symbol
    // as a `referencing_files` lead. Owners are constrained to the queried
    // anchors' repositories so a `--repo`-scoped query never reports a same-named
    // import from another repository. Name-based, so same-name collisions can
    // surface extra leads — consistent with the "leads, not proof" contract.
    let mut anchor_names: BTreeMap<&str, &str> = BTreeMap::new();
    for id in &seed_set {
        if let Some(&node) = by_id.get(*id)
            && matches!(record_node_kind(node), Some(NodeKind::Symbol))
            && let GraphRecord::Node {
                name: Some(name), ..
            } = node
        {
            anchor_names.entry(last_path_segment(name)).or_insert(*id);
        }
    }
    // Import leads are hop-1 neighbours, so they are only produced when at least
    // one hop is requested (a `--depth 0` query reports no impact leads at all).
    if depth >= 1 && !anchor_names.is_empty() {
        // Repositories of the queried anchors. Empty when the store has no
        // repository attribution, in which case import owners are not filtered.
        let anchor_repos: BTreeSet<&str> = original_targets
            .iter()
            .filter_map(|id| repo_index.owner_of(id))
            .collect();
        for r in records {
            let GraphRecord::Node {
                id: import_id,
                kind: NodeKind::Import,
                name: Some(import_name),
                ..
            } = r
            else {
                continue;
            };
            if deleted(import_id.as_str()) {
                continue;
            }
            let Some(&anchor) = imported_symbol_names(import_name)
                .into_iter()
                .find_map(|seg| anchor_names.get(seg))
            else {
                continue;
            };
            #[allow(clippy::map_unwrap_or)]
            for &(edge_id, label, owner_id) in inbound_edges
                .get(import_id.as_str())
                .map(Vec::as_slice)
                .unwrap_or(&[])
            {
                if !matches!(label, EdgeLabel::Imports) {
                    continue;
                }
                let (Some(&owner), Some(&edge_record)) = (by_id.get(owner_id), by_id.get(edge_id))
                else {
                    continue;
                };
                // The owner is the importing file/module, or the owning Symbol
                // for an import local to an impl method. Never report the queried
                // target itself.
                if !matches!(
                    record_node_kind(owner),
                    Some(NodeKind::File | NodeKind::Module | NodeKind::Symbol)
                ) || original_targets.contains(owner.id())
                {
                    continue;
                }
                // Repo scope applies only when the caller passed `--repo`. For a
                // scoped query the owner must resolve to one of the anchors'
                // repositories — an owner with no repository attribution is out of
                // scope and skipped, so an unattributed legacy/generated file
                // cannot leak a cross-repo lead. An unscoped query does not filter,
                // so legitimate cross-repo importers are still reported.
                if repo_scope.is_some()
                    && !anchor_repos.is_empty()
                    && !repo_index
                        .owner_of(owner.id())
                        .is_some_and(|repo| anchor_repos.contains(repo))
                {
                    continue;
                }
                referencing_files
                    .entry((owner.id(), edge_id))
                    .or_insert(ImpactLead {
                        record: owner,
                        edge: edge_record,
                        relation: "IMPORTS",
                        direction: ImpactDirection::Inbound,
                        anchor_id: anchor,
                        hop: 1,
                    });
            }
        }
    }

    // ── Sort all groups canonically and apply per-group cap (AC6/AC7) ──────────
    let mut truncations: Vec<ImpactTruncation> = Vec::new();

    let direct_callers = drain_sorted(
        direct_callers,
        "direct_callers",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let direct_callees = drain_sorted(
        direct_callees,
        "direct_callees",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let referencing_files = drain_sorted(
        referencing_files,
        "referencing_files",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let implementation_symbols = drain_sorted(
        implementation_symbols,
        "implementation_symbols",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );
    let containing_context = drain_sorted(
        containing_context,
        "containing_context",
        MAX_LEADS_PER_GROUP,
        depth,
        &mut truncations,
    );

    // ── Sort and dedup diagnostics ────────────────────────────────────────────
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    // Emit neighborhood_truncated diagnostics for each truncation
    for t in &truncations {
        diagnostics.push(MemoryAuditDiagnostic {
            code: "neighborhood_truncated".to_owned(),
            source_record_id: String::new(),
            target_handle: t.group.to_owned(),
            relation: format!(
                "returned={} total={} depth={}",
                t.returned, t.total, t.depth
            ),
            target_domain: String::new(),
        });
    }
    // Re-sort after appending truncation diagnostics
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    ChangeImpactContext {
        target_kind,
        target_ids,
        direct_callers,
        direct_callees,
        referencing_files,
        implementation_symbols,
        containing_context,
        diagnostics,
        depth,
        truncations,
    }
}

// ---------------------------------------------------------------------------
// Transitive inbound reachability — `eg query transitive-callers` (issue #139)
// ---------------------------------------------------------------------------

/// Edge labels the transitive-callers walk traverses inbound. `MENTIONS` and
/// other weak/topology labels are excluded so the depth-1 result is exactly
/// the direct inbound `CALLS`/`REFERENCES` neighbor set of #122/#76.
const TRANSITIVE_CALLER_LABELS: &[EdgeLabel] = &[EdgeLabel::Calls, EdgeLabel::References];

/// Borrowed 4-tuple used by the transitive walk's indexes: an inbound edge
/// `(source_id, edge_id, label, resolution)` or a shortest-path discovery
/// pointer `(edge_id, parent_id, label, resolution)`.
type TransitiveEdgeRef<'a> = (&'a str, &'a str, &'static str, Option<CallResolution>);

/// One hop of a concrete connecting call path: `source` calls/references
/// `target`, moving one step from a reachable node toward the queried symbol.
#[derive(Debug, Clone, Copy)]
pub struct TransitivePathStep<'a> {
    /// Record ID of the caller/referencer side of this hop.
    pub source_record_id: &'a str,
    /// Stable record ID of the connecting edge.
    pub edge_record_id: &'a str,
    /// Edge label (`CALLS` / `REFERENCES`).
    pub edge_label: &'static str,
    /// Call resolution status carried by the edge (issues #152/#134), when
    /// the edge is inside the resolution contract.
    pub resolution: Option<CallResolution>,
    /// Record ID of the callee/referenced side of this hop.
    pub target_record_id: &'a str,
}

/// One symbol (or file) that can reach the queried target, with its shortest
/// discovered connecting path.
#[derive(Debug, Clone)]
pub struct TransitiveCallerRow<'a> {
    /// The reachable node record.
    pub record: &'a GraphRecord,
    /// Shortest hop distance from the queried target (>= 1).
    pub hop: usize,
    /// Ordered connecting chain from this node down to the target: the first
    /// step's source is this node, the last step's target is the queried
    /// symbol, and consecutive steps share their middle record ID.
    pub path: Vec<TransitivePathStep<'a>>,
    /// Weakest call-resolution status along the path (`unresolved` >
    /// `ambiguous` > `resolved`), or `None` when no step on the path carries
    /// the resolution contract (e.g. a pure `REFERENCES` chain).
    pub path_resolution: Option<CallResolution>,
}

/// Count of reachable-but-dropped frontier nodes at one depth beyond the
/// `--max-depth` bound.
#[derive(Debug, Clone, Copy)]
pub struct TransitiveDroppedDepth {
    /// Depth (hop distance) at which these nodes would have been discovered.
    pub depth: usize,
    /// Number of distinct nodes first reachable at that depth.
    pub count: usize,
}

/// Truncation diagnostic emitted when reachable nodes exist beyond the depth
/// bound: nothing is silently omitted, the dropped frontier is counted per
/// depth (AC4).
#[derive(Debug, Clone)]
pub struct TransitiveTruncation {
    /// The bound in effect.
    pub max_depth: usize,
    /// Dropped frontier counts per depth beyond the bound, ascending.
    pub dropped_frontier: Vec<TransitiveDroppedDepth>,
    /// Total dropped nodes across all depths beyond the bound.
    pub dropped_total: usize,
}

/// Structured transitive-callers result returned by [`transitive_callers`].
#[derive(Debug)]
pub struct TransitiveCallersContext<'a> {
    /// The resolved anchor (queried symbol) record.
    pub anchor: &'a GraphRecord,
    /// Reachable rows, canonically ordered by `(hop, record_id)` ascending.
    pub rows: Vec<TransitiveCallerRow<'a>>,
    /// Depth-bound truncation diagnostic, when reachable nodes were dropped.
    pub truncation: Option<TransitiveTruncation>,
    /// Stable machine-readable diagnostics (dangling edge sources).
    pub diagnostics: Vec<MemoryAuditDiagnostic>,
    /// The depth bound used for the walk.
    pub max_depth: usize,
}

/// First resolved anchor whose node kind is **not** a code `Symbol`, if any.
///
/// `transitive-callers` accepts only symbol handles: a canonical codegraph ID
/// resolving to a `Module`, `Import`, `Commit`, `Change`, or other node kind
/// maps to [`FailureTargetKind::Symbol`] during handle resolution and must be
/// rejected rather than walked as an empty symbol result.
#[must_use]
pub fn transitive_callers_non_symbol_anchor_kind(
    records: &[GraphRecord],
    target: &ResolvedFailureTarget,
) -> Option<NodeKind> {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    target
        .anchor_ids
        .iter()
        .filter_map(|id| by_id.get(id.as_str()).copied())
        .filter_map(record_node_kind)
        .find(|kind| !matches!(kind, NodeKind::Symbol))
}

/// Walks the transitive inbound `CALLS`/`REFERENCES` closure of one resolved
/// symbol, bounded by `max_depth` hops (issue #139).
///
/// The walk is a level-synchronized BFS over inbound edges, so every reported
/// node carries its **shortest** hop distance and one concrete shortest
/// connecting path chosen deterministically (minimum `(parent record ID,
/// edge record ID)` at the discovering depth). A visited set guarantees each
/// node is reported at most once and that cycles (mutual recursion)
/// terminate. The anchor itself is never reported as its own caller.
///
/// When reachable nodes exist beyond `max_depth` the walk keeps counting
/// (without materializing rows or paths) and reports the dropped frontier per
/// depth in [`TransitiveCallersContext::truncation`] rather than silently
/// omitting them.
///
/// Rows are reachability leads: a path existing in the graph is never proof
/// that a change breaks the caller. Returns `None` when `anchor_id` names no
/// live node in `records`.
///
/// # Panics
///
/// Panics only on violated internal invariants: every discovered node is a
/// live record with a parent pointer chaining back to the anchor by
/// construction of the BFS.
#[must_use]
pub fn transitive_callers<'a>(
    records: &'a [GraphRecord],
    anchor_id: &str,
    max_depth: usize,
) -> Option<TransitiveCallersContext<'a>> {
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
    let by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter_map(|r| {
            let id = r.id();
            if deleted(id) { None } else { Some((id, r)) }
        })
        .collect();

    let anchor = by_id.get(anchor_id).copied()?;
    let anchor_id: &str = anchor.id();

    // ── inbound edge index: target -> [(source, edge, label, resolution)] ─────
    let mut inbound: BTreeMap<&str, Vec<TransitiveEdgeRef<'a>>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            resolution,
            ..
        } = r
        {
            if deleted(id.as_str()) || !TRANSITIVE_CALLER_LABELS.contains(label) {
                continue;
            }
            inbound.entry(target.as_str()).or_default().push((
                source.as_str(),
                id.as_str(),
                label.as_str(),
                *resolution,
            ));
        }
    }
    // Deterministic edge visit order; drop exact duplicates from history views
    // where the same stable edge ID recurs across commit snapshots.
    for edges in inbound.values_mut() {
        edges.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
        edges.dedup_by_key(|e| (e.0, e.1));
    }

    // ── level-synchronized BFS from the anchor ────────────────────────────────
    // parent[node] = (edge_id, parent_id, edge_label, resolution): the
    // deterministic shortest-path discovery pointer toward the anchor.
    let mut parent: BTreeMap<&str, TransitiveEdgeRef<'a>> = BTreeMap::new();
    let mut hop_of: BTreeMap<&str, usize> = BTreeMap::new();
    let mut visited: BTreeSet<&str> = BTreeSet::new();
    visited.insert(anchor_id);
    let mut frontier: Vec<&str> = vec![anchor_id];
    let mut diagnostics: Vec<MemoryAuditDiagnostic> = Vec::new();

    // Discovers the next BFS level from `frontier`. For every newly reachable
    // node the minimum `(parent_id, edge_id)` discovery is kept so the
    // reported path is deterministic. Dangling edge sources produce a
    // diagnostic instead of a row when `report` is set (inside the bound).
    let discover_level = |frontier: &[&'a str],
                          visited: &BTreeSet<&str>,
                          diagnostics: &mut Vec<MemoryAuditDiagnostic>,
                          report: bool|
     -> BTreeMap<&'a str, TransitiveEdgeRef<'a>> {
        let mut discoveries: BTreeMap<&str, TransitiveEdgeRef<'_>> = BTreeMap::new();
        for &node in frontier {
            #[allow(clippy::map_unwrap_or)]
            for &(source_id, edge_id, label, resolution) in
                inbound.get(node).map(Vec::as_slice).unwrap_or(&[])
            {
                if visited.contains(source_id) {
                    continue;
                }
                let Some(&source_record) = by_id.get(source_id) else {
                    if report && !deleted(source_id) {
                        diagnostics.push(MemoryAuditDiagnostic {
                            code: "unresolved_edge_source".to_owned(),
                            source_record_id: edge_id.to_owned(),
                            target_handle: source_id.to_owned(),
                            relation: label.to_owned(),
                            target_domain: "codegraph".to_owned(),
                        });
                    }
                    continue;
                };
                let candidate = (source_record.id(), (edge_id, node, label, resolution));
                match discoveries.entry(candidate.0) {
                    std::collections::btree_map::Entry::Vacant(e) => {
                        e.insert(candidate.1);
                    }
                    std::collections::btree_map::Entry::Occupied(mut e) => {
                        // Keep the minimum (parent_id, edge_id) discovery.
                        let (prev_edge, prev_parent, ..) = *e.get();
                        if (candidate.1.1, candidate.1.0) < (prev_parent, prev_edge) {
                            e.insert(candidate.1);
                        }
                    }
                }
            }
        }
        discoveries
    };

    let mut depth = 0usize;
    while depth < max_depth {
        if frontier.is_empty() {
            break;
        }
        depth += 1;
        let discoveries = discover_level(&frontier, &visited, &mut diagnostics, true);
        frontier = discoveries.keys().copied().collect();
        for (node, discovery) in discoveries {
            visited.insert(node);
            hop_of.insert(node, depth);
            parent.insert(node, discovery);
        }
    }

    // ── dropped-frontier counting beyond the bound (AC4) ─────────────────────
    let mut truncation: Option<TransitiveTruncation> = None;
    if !frontier.is_empty() {
        let mut dropped_frontier: Vec<TransitiveDroppedDepth> = Vec::new();
        let mut dropped_total = 0usize;
        let mut count_depth = depth;
        loop {
            let discoveries = discover_level(&frontier, &visited, &mut diagnostics, false);
            if discoveries.is_empty() {
                break;
            }
            count_depth += 1;
            dropped_frontier.push(TransitiveDroppedDepth {
                depth: count_depth,
                count: discoveries.len(),
            });
            dropped_total += discoveries.len();
            frontier = discoveries.keys().copied().collect();
            for node in frontier.iter().copied() {
                visited.insert(node);
            }
        }
        if dropped_total > 0 {
            truncation = Some(TransitiveTruncation {
                max_depth,
                dropped_frontier,
                dropped_total,
            });
        }
    }

    // ── path materialization: shortest chain from each row to the anchor ─────
    let mut rows: Vec<TransitiveCallerRow<'a>> = Vec::with_capacity(hop_of.len());
    for (&node, &hop) in &hop_of {
        let record = by_id
            .get(node)
            .copied()
            .expect("discovered nodes are live records");
        let mut path: Vec<TransitivePathStep<'a>> = Vec::with_capacity(hop);
        let mut cursor = node;
        while cursor != anchor_id {
            let &(edge_id, parent_id, label, resolution) = parent
                .get(cursor)
                .expect("every discovered node has a parent pointer");
            path.push(TransitivePathStep {
                source_record_id: cursor,
                edge_record_id: edge_id,
                edge_label: label,
                resolution,
                target_record_id: parent_id,
            });
            cursor = parent_id;
        }
        // Weakest resolution wins: CallResolution orders resolved < ambiguous
        // < unresolved, so the maximum present status is the weakest link.
        let path_resolution = path.iter().filter_map(|s| s.resolution).max();
        rows.push(TransitiveCallerRow {
            record,
            hop,
            path,
            path_resolution,
        });
    }
    rows.sort_by(|a, b| {
        a.hop
            .cmp(&b.hop)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.source_record_id.cmp(&b.source_record_id))
            .then_with(|| a.target_handle.cmp(&b.target_handle))
            .then_with(|| a.relation.cmp(&b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });

    Some(TransitiveCallersContext {
        anchor,
        rows,
        truncation,
        diagnostics,
        max_depth,
    })
}

/// A crate entry point or binary target.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct EntryPoint {
    /// Stable record ID of the entry point file.
    pub record_id: String,
    /// Repository-relative path to the entry point file.
    pub repo_relative_path: String,
}

/// The kind of a node in the module tree.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Eq, PartialEq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ModuleNodeKind {
    /// A directory directory segment.
    Directory,
    /// A source file target.
    File,
}

/// A node in the repository module and directory/file tree structure.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct ModuleTreeNode {
    /// Directory segment or filename name.
    pub name: String,
    /// Repo-relative path of this directory or file.
    pub path: String,
    /// Kind of the node: directory or file.
    pub kind: ModuleNodeKind,
    /// Transitive count of symbols contained in files under this path.
    pub symbol_count: usize,
    /// Stable record ID of the file node (if file kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Documented absent span reason for directories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_handle_reason: Option<crate::citation_audit::AbsentHandleRule>,
    /// Nested child directories and files.
    pub children: Vec<Self>,
}

/// A symbol ranked by inbound reference degree.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct ReferencedSymbol {
    /// Stable record ID of the symbol.
    pub record_id: String,
    /// Fully qualified name of the symbol.
    pub name: String,
    /// Repository-relative path containing the symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span of the symbol's definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented absent span reason (if span is None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub absent_handle_reason: Option<crate::citation_audit::AbsentHandleRule>,
    /// Total count of inbound reference/calls edges.
    pub inbound_degree: usize,
}

/// Structured orientation map for cold-starting in a repository.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, Debug)]
pub struct OrientationMap {
    /// Entry points (roots/binaries).
    pub entry_points: Vec<EntryPoint>,
    /// Directory/file tree with transitive symbol counts.
    pub module_tree: Vec<ModuleTreeNode>,
    /// Top most-referenced symbols ranked by degree.
    pub top_referenced_symbols: Vec<ReferencedSymbol>,
}

/// Error kinds returned by the orientation map builder.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum OrientationError {
    /// The graph contains zero code-graph nodes.
    #[error("graph has zero code-graph nodes")]
    EmptyGraph,
    /// No entry-point files were found.
    #[error("no entry-point files found in the graph")]
    NoEntryPoints,
}

/// Helper to determine if a path is considered a crate root or binary target.
#[must_use]
pub fn is_entry_point(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let path_ref = std::path::Path::new(&normalized);
    let extension = path_ref.extension();

    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("rs")) {
        return normalized == "src/lib.rs"
            || normalized == "src/main.rs"
            || normalized.ends_with("/src/lib.rs")
            || normalized.ends_with("/src/main.rs")
            || normalized.starts_with("src/bin/")
            || normalized.contains("/src/bin/");
    }

    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("go")) {
        return normalized == "main.go"
            || normalized.ends_with("/main.go")
            || normalized.contains("/cmd/");
    }

    if extension
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ts") || ext.eq_ignore_ascii_case("tsx"))
    {
        return normalized == "index.ts"
            || normalized == "main.ts"
            || normalized == "index.tsx"
            || normalized.ends_with("/index.ts")
            || normalized.ends_with("/main.ts")
            || normalized.ends_with("/index.tsx");
    }

    if extension
        .is_some_and(|ext| ext.eq_ignore_ascii_case("js") || ext.eq_ignore_ascii_case("jsx"))
    {
        return normalized == "index.js"
            || normalized == "main.js"
            || normalized.ends_with("/index.js")
            || normalized.ends_with("/main.js");
    }

    if extension.is_some_and(|ext| ext.eq_ignore_ascii_case("py")) {
        return normalized == "main.py"
            || normalized == "app.py"
            || normalized.ends_with("/main.py")
            || normalized.ends_with("/app.py");
    }

    false
}

struct TrieNode {
    name: String,
    path: String,
    is_file: bool,
    symbol_count: usize,
    record_id: Option<String>,
    children: BTreeMap<String, Self>,
}

impl TrieNode {
    fn compute_transitive_counts(&mut self) -> usize {
        let children_sum: usize = self
            .children
            .values_mut()
            .map(Self::compute_transitive_counts)
            .sum();
        if !self.is_file {
            self.symbol_count = children_sum;
        }
        self.symbol_count
    }
}

fn convert_trie_node(node: TrieNode) -> ModuleTreeNode {
    let children: Vec<ModuleTreeNode> =
        node.children.into_values().map(convert_trie_node).collect();
    ModuleTreeNode {
        name: node.name,
        path: node.path,
        kind: if node.is_file {
            ModuleNodeKind::File
        } else {
            ModuleNodeKind::Directory
        },
        symbol_count: node.symbol_count,
        record_id: node.record_id,
        absent_handle_reason: if node.is_file {
            None
        } else {
            Some(crate::citation_audit::AbsentHandleRule::NoSpanModuleLevel)
        },
        children,
    }
}

/// Returns a repository orientation map (entry points, module tree, top symbols).
///
/// # Errors
///
/// Returns `OrientationError::EmptyGraph` if there are no code nodes, or
/// `OrientationError::NoEntryPoints` if no entry point files exist.
pub fn orientation_map(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
) -> Result<OrientationMap, OrientationError> {
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

    let index = RepositoryIndex::build(records);

    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    let is_code_edge = |label: EdgeLabel| -> bool {
        matches!(
            label,
            EdgeLabel::Contains
                | EdgeLabel::Defines
                | EdgeLabel::Imports
                | EdgeLabel::References
                | EdgeLabel::Calls
                | EdgeLabel::Implements
                | EdgeLabel::Mentions
        )
    };

    // 1. Zero code-graph nodes check
    let code_nodes_count = records
        .iter()
        .filter(|r| {
            if let GraphRecord::Node { id, kind, .. } = r {
                if tombstoned_ids.contains(id.as_str()) {
                    return false;
                }
                if !is_owned(id.as_str()) {
                    return false;
                }
                matches!(
                    kind,
                    NodeKind::Repository
                        | NodeKind::File
                        | NodeKind::Module
                        | NodeKind::Symbol
                        | NodeKind::Import
                        | NodeKind::Diagnostic
                )
            } else {
                false
            }
        })
        .count();

    if code_nodes_count == 0 {
        return Err(OrientationError::EmptyGraph);
    }

    // 2. Entry points extraction
    let mut entry_points = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            if is_entry_point(path) {
                entry_points.push(EntryPoint {
                    record_id: id.clone(),
                    repo_relative_path: path.clone(),
                });
            }
        }
    }
    if entry_points.is_empty() {
        return Err(OrientationError::NoEntryPoints);
    }
    entry_points.sort_by(|a, b| a.repo_relative_path.cmp(&b.repo_relative_path));

    // 3. Module/file tree
    let mut files = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            files.push((id.clone(), path.clone()));
        }
    }

    let mut file_symbol_counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            repo_relative_path: Some(path),
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            let normalized_path = path.replace('\\', "/");
            *file_symbol_counts.entry(normalized_path).or_default() += 1;
        }
    }

    let mut trie_roots: BTreeMap<String, TrieNode> = BTreeMap::new();
    for (file_id, file_path) in &files {
        let normalized = file_path.replace('\\', "/");
        let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
        if segments.is_empty() {
            continue;
        }
        let first_seg = segments[0].to_string();
        let count = file_symbol_counts.get(&normalized).copied().unwrap_or(0);

        let mut curr_node = trie_roots
            .entry(first_seg.clone())
            .or_insert_with(|| TrieNode {
                name: first_seg.clone(),
                path: first_seg.clone(),
                is_file: segments.len() == 1,
                symbol_count: if segments.len() == 1 { count } else { 0 },
                record_id: if segments.len() == 1 {
                    Some(file_id.clone())
                } else {
                    None
                },
                children: BTreeMap::new(),
            });

        for (i, seg) in segments.iter().enumerate().skip(1) {
            let subpath = segments[0..=i].join("/");
            let is_last = i == segments.len() - 1;
            curr_node = curr_node
                .children
                .entry(seg.to_string())
                .or_insert_with(|| TrieNode {
                    name: seg.to_string(),
                    path: subpath,
                    is_file: is_last,
                    symbol_count: if is_last { count } else { 0 },
                    record_id: if is_last { Some(file_id.clone()) } else { None },
                    children: BTreeMap::new(),
                });
        }
    }

    let mut roots: Vec<TrieNode> = trie_roots.into_values().collect();
    for root in &mut roots {
        root.compute_transitive_counts();
    }
    // Sort roots alphabetically
    roots.sort_by(|a, b| a.name.cmp(&b.name));
    let module_tree: Vec<ModuleTreeNode> = roots.into_iter().map(convert_trie_node).collect();

    // 4. Top-referenced symbols
    let mut inbound_degrees: BTreeMap<String, usize> = BTreeMap::new();
    let mut active_symbols = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name,
            repo_relative_path,
            span,
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            active_symbols.insert(
                id.clone(),
                (
                    name.clone().unwrap_or_default(),
                    repo_relative_path.clone(),
                    *span,
                ),
            );
        }
    }

    for r in records {
        if let GraphRecord::Edge {
            id,
            label,
            target,
            source,
            ..
        } = r
        {
            if tombstoned_ids.contains(id.as_str()) {
                continue;
            }
            if !is_code_edge(*label) {
                continue;
            }
            if !is_owned(source.as_str()) {
                continue;
            }
            if active_symbols.contains_key(target) {
                *inbound_degrees.entry(target.clone()).or_default() += 1;
            }
        }
    }

    let mut ranked_symbols: Vec<ReferencedSymbol> = active_symbols
        .into_iter()
        .map(|(id, (name, repo_relative_path, span))| {
            let inbound_degree = inbound_degrees.get(&id).copied().unwrap_or(0);
            ReferencedSymbol {
                record_id: id,
                name,
                repo_relative_path,
                span,
                absent_handle_reason: if span.is_none() {
                    Some(crate::citation_audit::AbsentHandleRule::NoSpanModuleLevel)
                } else {
                    None
                },
                inbound_degree,
            }
        })
        .collect();

    ranked_symbols.sort_by(|a, b| {
        b.inbound_degree
            .cmp(&a.inbound_degree)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    ranked_symbols.truncate(limit);

    Ok(OrientationMap {
        entry_points,
        module_tree,
        top_referenced_symbols: ranked_symbols,
    })
}

/// The lifecycle event kind.
#[derive(serde::Serialize, serde::Deserialize, Copy, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LifelineEventKind {
    /// Symbol was first seen in the commit.
    Introduced,
    /// Symbol's body or drift was changed.
    Modified,
    /// Symbol was absent/removed.
    Removed,
    /// Symbol was reintroduced after being removed.
    Reintroduced,
}

impl std::fmt::Display for LifelineEventKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Introduced => "introduced",
            Self::Modified => "modified",
            Self::Removed => "removed",
            Self::Reintroduced => "reintroduced",
        };
        write!(f, "{s}")
    }
}

/// A temporal event in a symbol's lifecycle.
#[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
pub struct LifelineEvent {
    /// The event kind: "introduced", "modified", "removed", or "reintroduced".
    pub event_type: LifelineEventKind,
    /// The stable record ID associated with this event (symbol node ID, or tombstone ID).
    pub record_id: String,
    /// The Git commit SHA.
    pub commit: String,
    /// The repository-relative path (absent for removal events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// The syntax source span (absent for removal events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// The documented reason the span is absent (e.g. "tombstone" for removal events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<String>,
    /// The SemanticDrift record ID if this is a modifying event with drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_record_id: Option<String>,
    /// The semantic drift score if this is a modifying event with drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drift_score: Option<f64>,
}

/// Errors returned by the symbol lifeline query.
#[derive(thiserror::Error, Debug, Clone, Eq, PartialEq)]
pub enum LifelineError {
    /// The query symbol handle/name was not found in the graph.
    #[error("symbol not found in the graph: {query}")]
    UnknownSymbol {
        /// The query string.
        query: String,
    },
    /// The query name matched multiple symbols.
    #[error("ambiguous symbol name '{query}' matches multiple symbols")]
    AmbiguousSymbol {
        /// The query string.
        query: String,
        /// The unique record IDs matching the query.
        candidates: Vec<String>,
    },
}

/// Traces a single symbol's lifecycle across Git history.
///
/// # Errors
///
/// Returns `LifelineError::UnknownSymbol` if the query matches 0 symbols,
/// or `LifelineError::AmbiguousSymbol` if it matches multiple symbols.
pub fn symbol_lifeline(
    records: &[GraphRecord],
    query: &str,
    repo_id: Option<&str>,
) -> Result<Vec<LifelineEvent>, LifelineError> {
    let index = RepositoryIndex::build(records);

    let is_owned =
        |id: &str| -> bool { repo_id.is_none_or(|r_id| index.owner_of(id) == Some(r_id)) };

    // Pre-build a map from deleted_id to tombstone ID for O(log T) lookup
    let mut tombstone_map: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Tombstone { id, deleted_id, .. } = r {
            tombstone_map.insert(deleted_id.as_str(), id.as_str());
        }
    }

    // Gather all matching Symbol nodes
    let mut matching_symbol_ids = BTreeSet::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name,
            ..
        } = r
        {
            let tombstoned = tombstone_map.contains_key(id.as_str());
            if tombstoned {
                continue;
            }
            if !is_owned(id.as_str()) {
                continue;
            }
            if id == query || name.as_deref() == Some(query) {
                matching_symbol_ids.insert(id.as_str());
            }
        }
    }

    if matching_symbol_ids.is_empty() {
        return Err(LifelineError::UnknownSymbol {
            query: query.to_owned(),
        });
    }
    if matching_symbol_ids.len() > 1 {
        let candidates: Vec<String> = matching_symbol_ids
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        return Err(LifelineError::AmbiguousSymbol {
            query: query.to_owned(),
            candidates,
        });
    }

    let target_symbol_id =
        matching_symbol_ids
            .into_iter()
            .next()
            .ok_or_else(|| LifelineError::UnknownSymbol {
                query: query.to_owned(),
            })?;

    // Find the repository of the target symbol
    let target_repo_id = index.owner_of(target_symbol_id);

    // Build the CommitOrder for repository commits
    let commit_order = CommitOrder::build(records);

    // Filter commits, symbol snapshots, and drift records in a single consolidated loop
    let mut repo_commits = Vec::new();
    let mut parent_map: BTreeMap<&str, &[String]> = BTreeMap::new();
    let mut symbol_snapshots: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut symbol_snapshot_bodies: BTreeMap<&str, &str> = BTreeMap::new();
    let mut drift_map: BTreeMap<&str, &GraphRecord> = BTreeMap::new();

    for r in records {
        match r {
            GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                temporal: Some(t),
                ..
            } => {
                let in_repo = target_repo_id.map_or_else(
                    || index.owner_of(r.id()).is_none(),
                    |r_id| index.owner_of(r.id()) == Some(r_id),
                );
                if in_repo {
                    repo_commits.push((sha.as_str(), r));
                    parent_map.insert(sha.as_str(), &t.git_parent_commits);
                }
            }
            GraphRecord::Node {
                kind: NodeKind::Symbol,
                id,
                temporal: Some(t),
                summary,
                ..
            } if id == target_symbol_id => {
                symbol_snapshots.insert(t.git_commit.as_str(), r);
                symbol_snapshot_bodies.insert(t.git_commit.as_str(), summary.as_str());
            }
            GraphRecord::Node {
                kind: NodeKind::SemanticDrift,
                semantic_drift: Some(drift),
                ..
            } if drift.target_record_id == target_symbol_id => {
                drift_map.insert(drift.after_git_commit.as_str(), r);
            }
            _ => {}
        }
    }

    // Sort repository commits by topological rank
    repo_commits.sort_by(|a, b| {
        let rank_a = commit_order.rank(a.0);
        let rank_b = commit_order.rank(b.0);
        rank_a.cmp(&rank_b).then_with(|| a.0.cmp(b.0))
    });

    // Helper: did the symbol body change in a commit?
    let symbol_body_changed = |commit: &str, summary: &str| -> bool {
        let Some(parents) = parent_map.get(commit) else {
            return true;
        };
        if parents.is_empty() {
            return true;
        }
        let mut saw_parent_snapshot = false;
        for parent in *parents {
            if let Some(parent_body) = symbol_snapshot_bodies.get(parent.as_str()) {
                saw_parent_snapshot = true;
                if *parent_body != summary {
                    return true;
                }
            }
        }
        !saw_parent_snapshot
    };

    let mut events = Vec::new();
    let mut commit_live: BTreeMap<&str, bool> = BTreeMap::new();
    let mut introduced_commits: BTreeSet<&str> = BTreeSet::new();

    for (commit_sha, _) in &repo_commits {
        let snapshot = symbol_snapshots.get(commit_sha);
        if let Some(node) = snapshot {
            let GraphRecord::Node {
                repo_relative_path,
                span,
                summary,
                ..
            } = node
            else {
                continue;
            };

            let parents = parent_map.get(commit_sha);
            let was_live_at_any_parent = parents.is_some_and(|ps| {
                ps.iter()
                    .any(|p| commit_live.get(p.as_str()).copied().unwrap_or(false))
            });

            if was_live_at_any_parent {
                // If it was already live, check if modified
                let changed = symbol_body_changed(commit_sha, summary);
                let drift_node = drift_map.get(commit_sha);
                if changed || drift_node.is_some() {
                    let drift_record_id = drift_node.map(|r| r.id().to_owned());
                    let drift_score = drift_node.and_then(|r| {
                        if let GraphRecord::Node {
                            semantic_drift: Some(d),
                            ..
                        } = r
                        {
                            Some(d.score)
                        } else {
                            None
                        }
                    });

                    let absent_span_reason = if span.is_none() {
                        Some("no_span_module_level".to_owned())
                    } else {
                        None
                    };

                    events.push(LifelineEvent {
                        event_type: LifelineEventKind::Modified,
                        record_id: target_symbol_id.to_string(),
                        commit: (*commit_sha).to_owned(),
                        repo_relative_path: repo_relative_path.clone(),
                        span: *span,
                        absent_span_reason,
                        drift_record_id,
                        drift_score,
                    });
                }
            } else {
                // Determine introduced vs reintroduced using strict_descendants ancestry check
                let is_reintroduction = introduced_commits.iter().any(|&intro_commit| {
                    commit_order
                        .strict_descendants(intro_commit)
                        .contains(commit_sha)
                });

                let event_type = if is_reintroduction {
                    LifelineEventKind::Reintroduced
                } else {
                    introduced_commits.insert(commit_sha);
                    LifelineEventKind::Introduced
                };

                let absent_span_reason = if span.is_none() {
                    Some("no_span_module_level".to_owned())
                } else {
                    None
                };

                events.push(LifelineEvent {
                    event_type,
                    record_id: target_symbol_id.to_string(),
                    commit: (*commit_sha).to_owned(),
                    repo_relative_path: repo_relative_path.clone(),
                    span: *span,
                    absent_span_reason,
                    drift_record_id: None,
                    drift_score: None,
                });
            }
            commit_live.insert(commit_sha, true);
        } else {
            // Symbol is absent at this commit
            let parents = parent_map.get(commit_sha);
            let was_live_at_any_parent = parents.is_some_and(|ps| {
                ps.iter()
                    .any(|p| commit_live.get(p.as_str()).copied().unwrap_or(false))
            });

            if was_live_at_any_parent {
                // Symbol was live but is now absent -> removal event!
                let tombstone_record_id = tombstone_map.get(target_symbol_id).map_or_else(
                    || crate::ir::stable_id(&["tombstone", target_symbol_id]),
                    |&id| id.to_owned(),
                );

                events.push(LifelineEvent {
                    event_type: LifelineEventKind::Removed,
                    record_id: tombstone_record_id,
                    commit: (*commit_sha).to_owned(),
                    repo_relative_path: None,
                    span: None,
                    absent_span_reason: Some("tombstone".to_owned()),
                    drift_record_id: None,
                    drift_score: None,
                });
            }
            commit_live.insert(commit_sha, false);
        }
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Commit-range symbol/file deltas (issue #118)
// ---------------------------------------------------------------------------

/// Always-present advisory label for [`range_deltas`] responses.
///
/// Rows are observed deltas derived from stored graph snapshots; they never
/// assert anything about behavior, tests, or verification.
pub const RANGE_DELTAS_DISCLAIMER: &str = "Rows are observed structural and semantic deltas \
     between the resolved commits; they are not proof of behavior change, breakage, test \
     failure, or verification, and absence of a delta is not proof a behavior was preserved.";

/// Stable label attached to the semantic-drift section of a range-deltas
/// response, distinguishing semantic movement from structural change.
pub const RANGE_DELTAS_DRIFT_LABEL: &str = "semantic_movement_not_structural_change";

/// One classified structural delta between the two endpoints of a commit range.
///
/// Serialization is deliberately bounded to identity/path/span/commit metadata
/// (never node summaries, which embed normalized source bodies for
/// `scan-history` records).
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct RangeDeltaItem<'a> {
    /// Stable record ID of the delta's code fact (head-side snapshot for
    /// added/modified rows; base-side snapshot for removed rows).
    pub record_id: &'a str,
    /// Schema version stamped on the backing record.
    pub schema_version: u32,
    /// Stable change-class label (documented in `docs/cli/deltas.md`):
    /// `added_symbol` / `removed_symbol` / `modified_symbol` /
    /// `added_file` / `removed_file` / `modified_file`.
    pub change_class: &'static str,
    /// Symbol name; absent for file rows (the path is the handle).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    /// Language-specific symbol category, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<&'a str>,
    /// Repository-relative path of the file or symbol definition.
    pub repo_relative_path: &'a str,
    /// Source span, when available (base-side span for removed rows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented reason a symbol row carries no span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<&'static str>,
    /// The commit within the range that introduced the head-visible state of
    /// this delta (last such commit in topological order).
    pub commit: &'a str,
    /// Valid time (committer date) of the introducing commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<&'a str>,
}

/// One semantic-drift row folded into a range-deltas response.
///
/// Drift is semantic movement measured over embeddings, never structural
/// change; the parent section's `label` states this explicitly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeDriftRow<'a> {
    /// Stable record ID of the drift marker record.
    pub record_id: &'a str,
    /// Schema version stamped on the drift record.
    pub schema_version: u32,
    /// Stable change-class label: always `semantic_drift`.
    pub change_class: &'static str,
    /// Stable record ID of the drifted code fact.
    pub target_record_id: &'a str,
    /// Drift score under the recorded metric.
    pub score: f64,
    /// Commit SHA of the earlier embedding.
    pub before_git_commit: &'a str,
    /// Commit SHA of the later embedding.
    pub after_git_commit: &'a str,
    /// Valid time of the later embedding.
    pub after_valid_time: &'a str,
}

/// Semantic-drift section of a range-deltas response.
///
/// Structural deltas are always returned; when the store carries no drift
/// records (embeddings absent or drift never computed) the section reports
/// `status: "unavailable"` with a reason instead of an indistinguishable empty
/// list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeDriftSection<'a> {
    /// `available` when the store carries drift records, else `unavailable`.
    pub status: &'static str,
    /// Stable reason when `status` is `unavailable`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    /// Always [`RANGE_DELTAS_DRIFT_LABEL`]: drift is semantic movement, not
    /// structural change.
    pub label: &'static str,
    /// Drift rows whose later embedding lands inside the queried range.
    pub rows: Vec<RangeDriftRow<'a>>,
}

/// One row of the unresolved/unsupported diagnostic group of a range-deltas
/// response. These are stable machine-readable markers, never partial output.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct RangeDeltaDiagnostic {
    /// Stable diagnostic code (`unresolved_introducing_commit`,
    /// `missing_repo_relative_path`).
    pub code: &'static str,
    /// Stable record ID of the affected fact.
    pub record_id: String,
    /// Bounded human-readable detail (identity fields only, never payloads).
    pub detail: String,
}

/// Structured symbol- and file-level deltas between two commits, grouped by
/// stable change class. Returned by [`range_deltas`].
///
/// Every group is always present (empty vecs, never omitted) and canonically
/// ordered by `(repo_relative_path, name, record_id)` so repeated queries are
/// byte-equivalent after serialization.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RangeDeltas<'a> {
    /// Resolved full SHA of the base (older) endpoint.
    pub base: &'a str,
    /// Resolved full SHA of the head (newer) endpoint.
    pub head: &'a str,
    /// Number of commits in the range (reachable from head, not from base).
    pub range_commit_count: usize,
    /// Always-present advisory disclaimer ([`RANGE_DELTAS_DISCLAIMER`]).
    pub disclaimer: &'static str,
    /// Symbols present at head but not at base.
    pub added_symbols: Vec<RangeDeltaItem<'a>>,
    /// Symbols present at base but not at head.
    pub removed_symbols: Vec<RangeDeltaItem<'a>>,
    /// Symbols present at both endpoints whose recorded body changed.
    pub modified_symbols: Vec<RangeDeltaItem<'a>>,
    /// Files present at head but not at base.
    pub added_files: Vec<RangeDeltaItem<'a>>,
    /// Files present at base but not at head.
    pub removed_files: Vec<RangeDeltaItem<'a>>,
    /// Files present at both endpoints whose recorded content changed.
    pub modified_files: Vec<RangeDeltaItem<'a>>,
    /// Unresolved/unsupported diagnostic group.
    pub unresolved: Vec<RangeDeltaDiagnostic>,
    /// Semantic drift falling inside the range, or an unavailability marker.
    pub semantic_drift: RangeDriftSection<'a>,
}

/// Errors that can occur while resolving a range-deltas query.
///
/// Each variant serializes to a stable machine-readable diagnostic
/// (`error_type` + snake_case payload) rather than partial or silent output.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum RangeDeltasError {
    /// The specified commit prefix could not be resolved to any commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The specified commit prefix was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// Both endpoints resolved to the same commit; an empty range is reported
    /// as a diagnostic, never as silent empty output.
    IdenticalEndpoints {
        /// The full SHA both endpoints resolved to.
        commit: String,
    },
    /// The range is reversed (base is a descendant of head).
    ReversedRange {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// There is no ancestor path between base and head.
    NoPath {
        /// The base commit input.
        base: String,
        /// The head commit input.
        head: String,
    },
    /// The store history is empty (no commits present).
    EmptyHistory,
}

/// Internal endpoint delta classes used while resolving introducing commits.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RangeDeltaClass {
    Added,
    Removed,
    Modified,
}

/// The recorded snapshot body of a node record (empty for non-node records).
/// Used only for in-process comparison; snapshot bodies are never serialized.
const fn range_delta_node_summary(record: &GraphRecord) -> &str {
    match record {
        GraphRecord::Node { summary, .. } => summary.as_str(),
        _ => "",
    }
}

/// A resolved `<base>..<head>` commit range: endpoints, commit topology, and
/// the range commit set. Shared by the range queries (issues #118 and #157)
/// so both keep identical endpoint resolution and error taxonomy.
struct ResolvedCommitRange<'a> {
    /// Resolved full SHA of the base (older) endpoint.
    base_sha: &'a str,
    /// Resolved full SHA of the head (newer) endpoint.
    head_sha: &'a str,
    /// Commit SHA → deduplicated parent SHAs (temporal parents + PARENT_OF).
    parent_map: BTreeMap<&'a str, Vec<&'a str>>,
    /// Commit SHA → valid time (committer date), when recorded.
    commit_valid_time: BTreeMap<&'a str, &'a str>,
    /// Commits reachable from head but not from base.
    range_commit_shas: BTreeSet<&'a str>,
    /// Range commits, newest first in deterministic topological order.
    range_desc: Vec<&'a str>,
}

impl<'a> ResolvedCommitRange<'a> {
    /// The range commit that established the head-visible state of a delta:
    /// the last (newest topological) range commit where the class transition
    /// is observable against the commit's parents. For
    /// [`RangeDeltaClass::Modified`] two snapshots are compared through
    /// `modified_key` (the recorded body for #118, the signature or
    /// visibility surface for #157).
    fn introducing(
        &self,
        per_commit: &BTreeMap<&str, &'a GraphRecord>,
        class: RangeDeltaClass,
        modified_key: impl Fn(&GraphRecord) -> &str,
    ) -> Option<&'a str> {
        for &sha in &self.range_desc {
            let parents: &[&str] = self.parent_map.get(sha).map_or(&[], Vec::as_slice);
            match class {
                RangeDeltaClass::Added => {
                    if per_commit.contains_key(sha)
                        && parents.iter().all(|p| !per_commit.contains_key(p))
                    {
                        return Some(sha);
                    }
                }
                RangeDeltaClass::Removed => {
                    if !per_commit.contains_key(sha)
                        && parents.iter().any(|p| per_commit.contains_key(p))
                    {
                        return Some(sha);
                    }
                }
                RangeDeltaClass::Modified => {
                    if let Some(snap) = per_commit.get(sha) {
                        let key = modified_key(snap);
                        if parents.iter().any(|p| {
                            per_commit
                                .get(p)
                                .is_some_and(|parent_snap| modified_key(parent_snap) != key)
                        }) {
                            return Some(sha);
                        }
                    }
                }
            }
        }
        None
    }
}

/// Resolves two commit handles (full SHA or unique prefix) against the
/// store's `Commit` nodes into a [`ResolvedCommitRange`], reporting every
/// failure as a stable [`RangeDeltasError`] rather than partial output.
#[allow(clippy::missing_panics_doc)]
fn resolve_commit_range<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    in_scope: &dyn Fn(&str) -> bool,
) -> Result<ResolvedCommitRange<'a>, RangeDeltasError> {
    let has_any_commits = records
        .iter()
        .any(|r| matches!(r.node_kind_name(), Some("Commit")));
    if !has_any_commits {
        return Err(RangeDeltasError::EmptyHistory);
    }

    // ── endpoint resolution (full SHA or unique prefix) ─────────────────────
    let resolve_prefix = |prefix: &str| -> Result<&'a str, RangeDeltasError> {
        let mut matches = Vec::new();
        for r in records {
            if let GraphRecord::Node {
                kind: NodeKind::Commit,
                name: Some(sha),
                ..
            } = r
            {
                if sha.to_lowercase().starts_with(&prefix.to_lowercase()) && in_scope(r.id()) {
                    matches.push(sha.as_str());
                }
            }
        }
        matches.sort_unstable();
        matches.dedup();

        if matches.is_empty() {
            return Err(RangeDeltasError::MissingCommit {
                commit_prefix: prefix.to_owned(),
            });
        }
        if matches.len() > 1 {
            let string_matches = matches.iter().map(|s| (*s).to_owned()).collect();
            return Err(RangeDeltasError::AmbiguousCommitPrefix {
                commit_prefix: prefix.to_owned(),
                matches: string_matches,
            });
        }
        Ok(matches.into_iter().next().unwrap())
    };

    let base_sha = resolve_prefix(base_prefix)?;
    let head_sha = resolve_prefix(head_prefix)?;
    if base_sha == head_sha {
        return Err(RangeDeltasError::IdenticalEndpoints {
            commit: base_sha.to_owned(),
        });
    }

    // ── commit topology (temporal parents + PARENT_OF edges) ────────────────
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let mut parent_map: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut commit_valid_time: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
        {
            if in_scope(r.id()) {
                let entry = parent_map.entry(sha.as_str()).or_default();
                for parent in &t.git_parent_commits {
                    entry.push(parent.as_str());
                }
                commit_valid_time
                    .entry(sha.as_str())
                    .or_insert(t.valid_time.as_str());
            }
        }
        if let GraphRecord::Edge {
            label: EdgeLabel::ParentOf,
            source,
            target,
            ..
        } = r
        {
            if !in_scope(source.as_str()) || !in_scope(target.as_str()) {
                continue;
            }
            if let (Some(parent_node), Some(child_node)) =
                (by_id.get(source.as_str()), by_id.get(target.as_str()))
            {
                if let (
                    GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(psha),
                        ..
                    },
                    GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(csha),
                        ..
                    },
                ) = (parent_node, child_node)
                {
                    let entry = parent_map.entry(csha.as_str()).or_default();
                    entry.push(psha.as_str());
                }
            }
        }
    }
    for parents in parent_map.values_mut() {
        parents.sort_unstable();
        parents.dedup();
    }

    let get_reachable = |start_sha: &'a str| -> BTreeSet<&'a str> {
        let mut reachable = BTreeSet::new();
        let mut queue = vec![start_sha];
        while let Some(current) = queue.pop() {
            if !reachable.insert(current) {
                continue;
            }
            if let Some(parents) = parent_map.get(current) {
                for parent in parents {
                    if !reachable.contains(*parent) {
                        queue.push(*parent);
                    }
                }
            }
        }
        reachable
    };

    let reachable_head = get_reachable(head_sha);
    let reachable_base = get_reachable(base_sha);
    if !reachable_head.contains(base_sha) {
        if reachable_base.contains(head_sha) {
            return Err(RangeDeltasError::ReversedRange {
                base: base_prefix.to_owned(),
                head: head_prefix.to_owned(),
            });
        }
        return Err(RangeDeltasError::NoPath {
            base: base_prefix.to_owned(),
            head: head_prefix.to_owned(),
        });
    }
    let range_commit_shas: BTreeSet<&str> = reachable_head
        .difference(&reachable_base)
        .copied()
        .collect();

    // Range commits, newest first in deterministic topological order, so the
    // introducing-commit search finds the last commit that established the
    // head-visible state.
    let order = CommitOrder::build(records);
    let mut range_desc: Vec<&str> = range_commit_shas.iter().copied().collect();
    range_desc.sort_by(|a, b| order.rank(b).cmp(&order.rank(a)).then_with(|| b.cmp(a)));

    Ok(ResolvedCommitRange {
        base_sha,
        head_sha,
        parent_map,
        commit_valid_time,
        range_commit_shas,
        range_desc,
    })
}

/// Snapshot index for one node kind: stable record ID → (commit SHA →
/// snapshot record). Shared by the range queries (issues #118 and #157).
fn temporal_snapshot_index<'a>(
    records: &'a [GraphRecord],
    kind: NodeKind,
    in_scope: &dyn Fn(&str) -> bool,
) -> BTreeMap<&'a str, BTreeMap<&'a str, &'a GraphRecord>> {
    let mut snaps: BTreeMap<&str, BTreeMap<&str, &'a GraphRecord>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: record_kind,
            temporal: Some(t),
            ..
        } = r
        {
            if *record_kind != kind || !in_scope(id.as_str()) {
                continue;
            }
            snaps
                .entry(id.as_str())
                .or_default()
                .insert(t.git_commit.as_str(), r);
        }
    }
    snaps
}

/// Compute symbol- and file-level deltas between two commit handles, grouped
/// by stable change class (issue #118).
///
/// The two endpoints are full SHAs or unique prefixes resolved against the
/// store's `Commit` nodes. Deltas compare the recorded `File`/`Symbol`
/// snapshots at the base endpoint against the head endpoint; a fact that
/// appears and disappears strictly inside the range is not an endpoint delta.
/// Each row carries the range commit that introduced its head-visible state
/// (the last such commit in deterministic topological order) plus that
/// commit's valid time. A rename surfaces as a `removed_*` row for the old
/// name and an `added_*` row for the new name, because symbol identity is
/// path- and name-based.
///
/// Purely read-time: reads only the provided records, never Git state or the
/// working tree.
///
/// # Errors
///
/// Returns a [`RangeDeltasError`] when the history is empty, a commit handle
/// is missing or ambiguous, the endpoints are identical, the range is
/// reversed, or no ancestor path connects the endpoints.
#[allow(clippy::missing_panics_doc)]
pub fn range_deltas<'a>(
    records: &'a [GraphRecord],
    base_prefix: &str,
    head_prefix: &str,
    repo_scope: Option<&str>,
) -> Result<RangeDeltas<'a>, RangeDeltasError> {
    // Repository scoping mirrors `changes_context`: in a shared store two
    // repositories can carry the same commit SHA, so commit resolution and
    // snapshot selection are gated by owning repository when a scope is set.
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

    let range = resolve_commit_range(records, base_prefix, head_prefix, &in_scope)?;
    let base_sha = range.base_sha;
    let head_sha = range.head_sha;

    // ── snapshot index: record id → (commit sha → snapshot record) ──────────
    let file_snaps = temporal_snapshot_index(records, NodeKind::File, &in_scope);
    let symbol_snaps = temporal_snapshot_index(records, NodeKind::Symbol, &in_scope);

    let introducing =
        |per_commit: &BTreeMap<&str, &'a GraphRecord>, class: RangeDeltaClass| -> Option<&'a str> {
            range.introducing(per_commit, class, range_delta_node_summary)
        };

    let mut unresolved: Vec<RangeDeltaDiagnostic> = Vec::new();

    let build_item = |record: &'a GraphRecord,
                      class_label: &'static str,
                      class: RangeDeltaClass,
                      per_commit: &BTreeMap<&str, &'a GraphRecord>,
                      is_symbol: bool,
                      unresolved: &mut Vec<RangeDeltaDiagnostic>|
     -> Option<RangeDeltaItem<'a>> {
        let GraphRecord::Node {
            id,
            schema_version,
            name,
            symbol_kind,
            repo_relative_path,
            span,
            ..
        } = record
        else {
            return None;
        };
        let Some(path) = repo_relative_path.as_deref() else {
            unresolved.push(RangeDeltaDiagnostic {
                code: "missing_repo_relative_path",
                record_id: id.clone(),
                detail: format!("{class_label} row dropped: snapshot carries no repo path"),
            });
            return None;
        };
        let commit = introducing(per_commit, class).unwrap_or_else(|| {
            unresolved.push(RangeDeltaDiagnostic {
                code: "unresolved_introducing_commit",
                record_id: id.clone(),
                detail: format!(
                    "{class_label} delta confirmed between endpoints but no range commit \
                     shows the transition; falling back to the head commit"
                ),
            });
            head_sha
        });
        let absent_span_reason = if is_symbol && span.is_none() {
            Some("no_span_module_level")
        } else {
            None
        };
        Some(RangeDeltaItem {
            record_id: id,
            schema_version: *schema_version,
            change_class: class_label,
            name: if is_symbol { name.as_deref() } else { None },
            symbol_kind: if is_symbol {
                symbol_kind.as_deref()
            } else {
                None
            },
            repo_relative_path: path,
            span: *span,
            absent_span_reason,
            commit,
            valid_time: range.commit_valid_time.get(commit).copied(),
        })
    };

    let classify = |snaps: &BTreeMap<&str, BTreeMap<&str, &'a GraphRecord>>,
                    is_symbol: bool,
                    labels: [&'static str; 3],
                    unresolved: &mut Vec<RangeDeltaDiagnostic>|
     -> (
        Vec<RangeDeltaItem<'a>>,
        Vec<RangeDeltaItem<'a>>,
        Vec<RangeDeltaItem<'a>>,
    ) {
        let [added_label, removed_label, modified_label] = labels;
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut modified = Vec::new();
        for per_commit in snaps.values() {
            match (per_commit.get(base_sha), per_commit.get(head_sha)) {
                (None, Some(head_snap)) => {
                    added.extend(build_item(
                        head_snap,
                        added_label,
                        RangeDeltaClass::Added,
                        per_commit,
                        is_symbol,
                        unresolved,
                    ));
                }
                (Some(base_snap), None) => {
                    removed.extend(build_item(
                        base_snap,
                        removed_label,
                        RangeDeltaClass::Removed,
                        per_commit,
                        is_symbol,
                        unresolved,
                    ));
                }
                (Some(base_snap), Some(head_snap)) => {
                    if range_delta_node_summary(base_snap) != range_delta_node_summary(head_snap) {
                        modified.extend(build_item(
                            head_snap,
                            modified_label,
                            RangeDeltaClass::Modified,
                            per_commit,
                            is_symbol,
                            unresolved,
                        ));
                    }
                }
                // Present at neither endpoint: the fact appeared and
                // disappeared strictly inside the range, so it is not an
                // endpoint delta.
                (None, None) => {}
            }
        }
        let sort_items = |items: &mut Vec<RangeDeltaItem<'a>>| {
            items.sort_by(|a, b| {
                a.repo_relative_path
                    .cmp(b.repo_relative_path)
                    .then_with(|| a.name.unwrap_or("").cmp(b.name.unwrap_or("")))
                    .then_with(|| a.record_id.cmp(b.record_id))
            });
        };
        sort_items(&mut added);
        sort_items(&mut removed);
        sort_items(&mut modified);
        (added, removed, modified)
    };

    let (added_symbols, removed_symbols, modified_symbols) = classify(
        &symbol_snaps,
        true,
        ["added_symbol", "removed_symbol", "modified_symbol"],
        &mut unresolved,
    );
    let (added_files, removed_files, modified_files) = classify(
        &file_snaps,
        false,
        ["added_file", "removed_file", "modified_file"],
        &mut unresolved,
    );

    unresolved.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    unresolved.dedup();

    // ── semantic drift folding ───────────────────────────────────────────────
    let mut store_has_drift = false;
    let mut drift_rows: Vec<RangeDriftRow<'a>> = Vec::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::SemanticDrift,
            schema_version,
            temporal,
            semantic_drift: Some(drift),
            ..
        } = r
        {
            store_has_drift = true;
            let in_range = temporal
                .as_ref()
                .is_some_and(|t| range.range_commit_shas.contains(t.git_commit.as_str()))
                || range
                    .range_commit_shas
                    .contains(drift.after_git_commit.as_str());
            if in_range && in_scope(r.id()) {
                drift_rows.push(RangeDriftRow {
                    record_id: r.id(),
                    schema_version: *schema_version,
                    change_class: "semantic_drift",
                    target_record_id: &drift.target_record_id,
                    score: drift.score,
                    before_git_commit: &drift.before_git_commit,
                    after_git_commit: &drift.after_git_commit,
                    after_valid_time: &drift.after_valid_time,
                });
            }
        }
    }
    drift_rows.sort_by(|a, b| {
        a.target_record_id
            .cmp(b.target_record_id)
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    let semantic_drift = if store_has_drift {
        RangeDriftSection {
            status: "available",
            reason: None,
            label: RANGE_DELTAS_DRIFT_LABEL,
            rows: drift_rows,
        }
    } else {
        RangeDriftSection {
            status: "unavailable",
            reason: Some("no_drift_records_in_store"),
            label: RANGE_DELTAS_DRIFT_LABEL,
            rows: Vec::new(),
        }
    };

    Ok(RangeDeltas {
        base: base_sha,
        head: head_sha,
        range_commit_count: range.range_commit_shas.len(),
        disclaimer: RANGE_DELTAS_DISCLAIMER,
        added_symbols,
        removed_symbols,
        modified_symbols,
        added_files,
        removed_files,
        modified_files,
        unresolved,
        semantic_drift,
    })
}

// ---------------------------------------------------------------------------
// As-of file symbol listing (issue #158)
// ---------------------------------------------------------------------------

/// The temporal point selector accepted by [`file_symbols_at_point`].
///
/// Mirrors the `eg query symbol` valid-time flags (see
/// `docs/schema/temporal-selectors.md`): `--at` pins the point to a commit
/// handle, `--as-of` to the most recent commit at or before an RFC 3339
/// instant. The two are mutually exclusive, which the type makes structural.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FileAtPointSelector<'a> {
    /// A commit handle: full SHA or unique prefix.
    At(&'a str),
    /// An RFC 3339 valid-time instant.
    AsOf(&'a str),
}

/// One symbol row of a file-at-point response (issue #158).
///
/// Serialization is deliberately bounded to identity/path/span/commit
/// metadata resolved *as-of the selected point* — never node summaries, which
/// embed normalized source bodies for `scan-history` records.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct FileAtPointSymbol<'a> {
    /// Stable record ID of the symbol snapshot at the resolved commit.
    pub record_id: &'a str,
    /// Schema version stamped on the backing record.
    pub schema_version: u32,
    /// Symbol name as recorded at the resolved commit.
    pub name: &'a str,
    /// Always `Symbol`.
    pub kind: &'static str,
    /// Language-specific symbol category, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<&'a str>,
    /// Repository-relative path of the file as recorded at the point.
    pub repo_relative_path: &'a str,
    /// Source span resolved as-of the point (not the current tree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented reason a symbol row carries no span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<&'static str>,
    /// The resolved commit the row's state was computed against.
    pub commit: &'a str,
    /// Valid time (committer date) of the resolved commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<&'a str>,
}

/// One stable, machine-readable diagnostic on a *successful* file-at-point
/// response (e.g. `empty_symbol_set`). Never used for failures, which are
/// [`FileAtPointError`] values.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct FileAtPointDiagnostic {
    /// Stable diagnostic code.
    pub code: &'static str,
    /// Bounded human-readable detail (identity fields only, never payloads).
    pub detail: String,
}

/// A file's defined-symbol set reconstructed at a past commit or instant.
/// Returned by [`file_symbols_at_point`] (issue #158).
///
/// Rows are canonically ordered by `(span.start_line, name, record_id)` so
/// repeated queries against an unchanged store serialize byte-identically.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileSymbolsAtPoint<'a> {
    /// The queried repository-relative path, echoed.
    pub path: &'a str,
    /// The `--at` commit handle input, echoed (`null` for `--as-of` queries).
    pub at: Option<&'a str>,
    /// The `--as-of` instant input, echoed (`null` for `--at` queries).
    pub as_of: Option<&'a str>,
    /// Full SHA of the commit the result was computed against.
    pub resolved_commit: &'a str,
    /// Valid time (committer date) of the resolved commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_valid_time: Option<&'a str>,
    /// Stable record ID of the file snapshot at the resolved commit.
    pub file_record_id: &'a str,
    /// Schema version stamped on the file snapshot record.
    pub file_schema_version: u32,
    /// The symbols the file defined at the resolved point.
    pub symbols: Vec<FileAtPointSymbol<'a>>,
    /// Number of symbol rows returned.
    pub returned: usize,
    /// Stable diagnostics (`empty_symbol_set` when the file existed at the
    /// point but defined zero symbols — explicitly distinguishable from
    /// not-found, which is an error).
    pub diagnostics: Vec<FileAtPointDiagnostic>,
}

/// Errors that can occur while resolving a file-at-point query.
///
/// Each variant serializes to a stable machine-readable diagnostic
/// (`error_type` + snake_case payload) rather than partial, fabricated, or
/// silently empty output.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum FileAtPointError {
    /// The store carries no commit history (`scan-history` graph required).
    EmptyHistory,
    /// The `--at` commit handle resolved to no commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The `--at` commit handle was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// The `--as-of` instant is not a valid RFC 3339 timestamp.
    InvalidInstant {
        /// The malformed input, echoed.
        as_of: String,
        /// Parser detail.
        detail: String,
    },
    /// No commit exists at or before the `--as-of` instant.
    NoCommitAtOrBeforeInstant {
        /// The instant, echoed.
        as_of: String,
    },
    /// The path matches no file or symbol snapshot at any recorded commit.
    UnknownPath {
        /// The queried path, echoed.
        path: String,
    },
    /// The path is known to history but did not exist at the resolved point.
    FileAbsentAtPoint {
        /// The queried path, echoed.
        path: String,
        /// The full SHA of the resolved point.
        resolved_commit: String,
    },
    /// The unscoped query matched file snapshots in more than one repository;
    /// rerun with `--repo <SELECTOR>` (issue #67 contract).
    AmbiguousRepository {
        /// The queried path, echoed.
        path: String,
        /// The stable repository IDs that matched.
        repositories: Vec<String>,
    },
}

/// Reconstruct the deterministic set of symbols a file defined at a chosen
/// commit or valid-time instant (issue #158).
///
/// `scan-history` emits a full `File`/`Symbol` snapshot at every commit, so
/// the file's symbol set at a point is exactly the symbol snapshots recorded
/// at the resolved commit for that path: a symbol tombstoned at or before the
/// point has no snapshot there and can never leak into the result. Spans and
/// names are the recorded state as-of the point, not the current tree.
///
/// Code-facts only: the result reads `File`/`Symbol`/`Commit` history records
/// exclusively — agent observations, project/task, artifact, and verification
/// records are never mixed in. Purely read-time: reads only the provided
/// records, never Git state or the working tree.
///
/// # Errors
///
/// Returns a [`FileAtPointError`] when the history is empty, the commit
/// handle is missing or ambiguous, the instant is malformed or precedes the
/// first commit, the path is unknown, the path did not exist at the point, or
/// an unscoped query collides across repositories.
pub fn file_symbols_at_point<'a>(
    records: &'a [GraphRecord],
    path: &'a str,
    selector: FileAtPointSelector<'a>,
    repo_scope: Option<&str>,
) -> Result<FileSymbolsAtPoint<'a>, FileAtPointError> {
    let index = RepositoryIndex::build(records);
    let in_scope =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // ── commit timeline (scoped) ─────────────────────────────────────────────
    let mut commit_valid_time: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
        {
            if in_scope(r.id()) {
                commit_valid_time
                    .entry(sha.as_str())
                    .or_insert(t.valid_time.as_str());
            }
        }
    }
    if commit_valid_time.is_empty() {
        return Err(FileAtPointError::EmptyHistory);
    }

    // ── point resolution ─────────────────────────────────────────────────────
    let (resolved_sha, at_input, as_of_input) = match selector {
        FileAtPointSelector::At(prefix) => {
            let lowered = prefix.to_lowercase();
            let mut matches: Vec<&str> = commit_valid_time
                .keys()
                .copied()
                .filter(|sha| sha.to_lowercase().starts_with(&lowered))
                .collect();
            matches.sort_unstable();
            matches.dedup();
            if matches.is_empty() {
                return Err(FileAtPointError::MissingCommit {
                    commit_prefix: prefix.to_owned(),
                });
            }
            if matches.len() > 1 {
                return Err(FileAtPointError::AmbiguousCommitPrefix {
                    commit_prefix: prefix.to_owned(),
                    matches: matches.iter().map(|s| (*s).to_owned()).collect(),
                });
            }
            (matches[0], Some(prefix), None)
        }
        FileAtPointSelector::AsOf(instant) => {
            let as_of_dt = DateTime::parse_from_rfc3339(instant).map_err(|e| {
                FileAtPointError::InvalidInstant {
                    as_of: instant.to_owned(),
                    detail: e.to_string(),
                }
            })?;
            // An instant must resolve on the queried path's own repository
            // timeline: in a shared multi-repository store, an unrelated
            // repository's newer commit would otherwise win the at-or-before
            // race and make the file look absent at a commit its repository
            // never had. Narrow the candidate commits to the repository
            // group(s) that actually record the path.
            let mut path_owner_groups: BTreeSet<Option<&str>> = BTreeSet::new();
            for r in records {
                if let GraphRecord::Node {
                    id,
                    kind: NodeKind::File | NodeKind::Symbol,
                    repo_relative_path: Some(p),
                    temporal: Some(_),
                    ..
                } = r
                {
                    if p == path && in_scope(id) {
                        path_owner_groups.insert(index.owner_of(id));
                    }
                }
            }
            if path_owner_groups.is_empty() {
                return Err(FileAtPointError::UnknownPath {
                    path: path.to_owned(),
                });
            }
            // Two repositories recording the same path have two distinct
            // timelines; an unscoped single-answer time view never picks one
            // implicitly (issue #67).
            if path_owner_groups.len() > 1 {
                return Err(FileAtPointError::AmbiguousRepository {
                    path: path.to_owned(),
                    repositories: path_owner_groups
                        .iter()
                        .filter_map(|g| *g)
                        .map(str::to_owned)
                        .collect(),
                });
            }
            // Exactly one group remains; `flatten` keeps the unattributed
            // (`None`) group as `None` without a panicking unwrap.
            let path_owner = path_owner_groups.into_iter().next().flatten();
            let owned_commit_shas: BTreeSet<&str> = records
                .iter()
                .filter_map(|r| {
                    if let GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(sha),
                        ..
                    } = r
                    {
                        (index.owner_of(r.id()) == path_owner).then_some(sha.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            // Most recent owned commit at or before the instant. Git
            // timestamps are second-resolution, so equal valid times are
            // broken by topological rank (a descendant outranks its
            // ancestors), then by SHA for full determinism.
            let order = CommitOrder::build(records);
            let best = commit_valid_time
                .iter()
                .filter(|&(&sha, _)| {
                    // Degenerate mixed-attribution stores (path attributed,
                    // commits not) fall back to the full scoped timeline
                    // rather than an empty one.
                    owned_commit_shas.is_empty() || owned_commit_shas.contains(sha)
                })
                .filter_map(|(&sha, &vt)| {
                    let parsed = DateTime::parse_from_rfc3339(vt).ok()?;
                    (parsed <= as_of_dt).then_some((parsed, order.rank(sha), sha))
                })
                .max();
            let Some((_, _, sha)) = best else {
                return Err(FileAtPointError::NoCommitAtOrBeforeInstant {
                    as_of: instant.to_owned(),
                });
            };
            (sha, None, Some(instant))
        }
    };

    // ── file existence at the point (not-found vs absent-at-point) ──────────
    let mut file_snapshots_at_point: Vec<&GraphRecord> = Vec::new();
    let mut path_known_to_history = false;
    for r in records {
        let GraphRecord::Node {
            kind,
            repo_relative_path,
            temporal: Some(t),
            ..
        } = r
        else {
            continue;
        };
        if repo_relative_path.as_deref() != Some(path) || !in_scope(r.id()) {
            continue;
        }
        match kind {
            NodeKind::File => {
                path_known_to_history = true;
                if t.git_commit == resolved_sha {
                    file_snapshots_at_point.push(r);
                }
            }
            NodeKind::Symbol => path_known_to_history = true,
            _ => {}
        }
    }

    if file_snapshots_at_point.is_empty() {
        if path_known_to_history {
            return Err(FileAtPointError::FileAbsentAtPoint {
                path: path.to_owned(),
                resolved_commit: resolved_sha.to_owned(),
            });
        }
        return Err(FileAtPointError::UnknownPath {
            path: path.to_owned(),
        });
    }

    // A shared store can carry the same path+commit under distinct repository
    // identities; never pick one implicitly (issue #67).
    let owner_groups: BTreeSet<Option<&str>> = file_snapshots_at_point
        .iter()
        .map(|r| index.owner_of(r.id()))
        .collect();
    if owner_groups.len() > 1 {
        return Err(FileAtPointError::AmbiguousRepository {
            path: path.to_owned(),
            repositories: owner_groups
                .iter()
                .filter_map(|g| *g)
                .map(str::to_owned)
                .collect(),
        });
    }

    file_snapshots_at_point.sort_by(|a, b| a.id().cmp(b.id()));
    let file_record = file_snapshots_at_point[0];
    let owner = index.owner_of(file_record.id());
    let (file_record_id, file_schema_version) = match file_record {
        GraphRecord::Node {
            id, schema_version, ..
        } => (id.as_str(), *schema_version),
        _ => unreachable!("file snapshots are node records"),
    };

    // ── symbol snapshots at the resolved commit ──────────────────────────────
    let mut symbols: Vec<FileAtPointSymbol<'a>> = Vec::new();
    for r in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            schema_version,
            name,
            symbol_kind,
            repo_relative_path,
            span,
            temporal: Some(t),
            ..
        } = r
        else {
            continue;
        };
        if repo_relative_path.as_deref() != Some(path)
            || t.git_commit != resolved_sha
            || index.owner_of(id) != owner
        {
            continue;
        }
        symbols.push(FileAtPointSymbol {
            record_id: id,
            schema_version: *schema_version,
            name: name.as_deref().unwrap_or(""),
            kind: "Symbol",
            symbol_kind: symbol_kind.as_deref(),
            repo_relative_path: path,
            span: *span,
            absent_span_reason: span.is_none().then_some("no_span_module_level"),
            commit: resolved_sha,
            valid_time: Some(t.valid_time.as_str()),
        });
    }
    symbols.sort_by(|a, b| {
        a.span
            .map(|s| s.start_line)
            .cmp(&b.span.map(|s| s.start_line))
            .then_with(|| a.name.cmp(b.name))
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    let diagnostics = if symbols.is_empty() {
        vec![FileAtPointDiagnostic {
            code: "empty_symbol_set",
            detail: format!(
                "file {path} existed at commit {resolved_sha} but defined zero symbols"
            ),
        }]
    } else {
        Vec::new()
    };

    Ok(FileSymbolsAtPoint {
        path,
        at: at_input,
        as_of: as_of_input,
        resolved_commit: resolved_sha,
        resolved_valid_time: commit_valid_time.get(resolved_sha).copied(),
        file_record_id,
        file_schema_version,
        returned: symbols.len(),
        symbols,
        diagnostics,
    })
}

// ---------------------------------------------------------------------------
// public-api surface query (issue #213)
// ---------------------------------------------------------------------------

/// Symbol kinds that can appear on the externally-reachable public API
/// surface. Methods, tests, and `impl` blocks are declaration details of
/// their owning items and are never enumerated as surface items.
const PUBLIC_API_SYMBOL_KINDS: &[&str] = &[
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
fn is_library_crate_path(path: &str) -> bool {
    let mut segments = path.split(['/', '\\']).filter(|s| !s.is_empty());
    segments.next() == Some("src") && {
        let second = segments.next();
        second.is_some() && second != Some("bin")
    }
}

/// How a module chain resolved during reachability checking.
enum ChainReachability {
    /// Every ancestor module is recorded `public`.
    Public,
    /// Some ancestor module is recorded with a non-`public` visibility.
    NotPublic,
    /// Some ancestor module has no visibility record in the graph (dead file,
    /// pre-#213 scan, or unscanned crate root). Reported, never guessed.
    Unknown(String),
}

/// Checks that every prefix of `chain` names a module recorded `public`.
fn chain_reachability(
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
/// records are excluded, and when a stable ID appears more than once (history
/// graphs) the latest record wins. Deterministic: output ordering depends
/// only on record content, never on map iteration or wall-clock time. Purely
/// parse-derived — never a build-verified or semver claim.
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

    // Current-state view: keep-last dedupe by stable ID so history graphs
    // resolve to their newest version deterministically.
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
        if tombstoned.contains(id.as_str()) || !is_owned(id) {
            continue;
        }
        saw_rust_code = true;
        if !repo_relative_path
            .as_deref()
            .is_some_and(is_library_crate_path)
        {
            continue;
        }
        nodes.insert(id.as_str(), record);
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
    let repo_index = repo_scope.map(|_| RepositoryIndex::build(records));
    let in_scope = |id: &str| -> bool {
        match (repo_scope, repo_index.as_ref()) {
            (Some(scope), Some(index)) => index.owner_of(id) == Some(scope),
            _ => true,
        }
    };

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

// ---------------------------------------------------------------------------
// undocumented public API lane (issue #257)
// ---------------------------------------------------------------------------

/// One symbol reported by the undocumented-public-API lane: a doc-auditable
/// symbol whose recorded doc-comment fact is absent.
///
/// For declared items the citation fields point at the declaration; for
/// re-exports (`via_reexport` = `true`) they point at the `pub use` site and
/// `target_record_id` cites the resolved declaration whose doc fact was
/// checked.
#[derive(Debug, Clone)]
pub struct UndocumentedItem<'a> {
    /// Stable record ID of the declaring `Symbol` node, or of the `Import`
    /// node at the re-export site.
    pub record_id: &'a str,
    /// Symbol kind (`function`, `struct`, `enum`, `trait`, `type_alias`,
    /// `const`, `static`, or `method` under `--include-private`).
    pub kind: String,
    /// Crate-relative fully-qualified path (alias-aware for re-exports).
    pub path: String,
    /// Recorded visibility class: `public` for externally-reachable rows;
    /// the declared class for `--include-private` rows.
    pub visibility: &'a str,
    /// Repo-relative file of the declaration or re-export site.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration or re-export site.
    pub span: Option<SourceSpan>,
    /// Persisted declaration signature (issue #124), joined when present.
    pub signature: Option<&'a str>,
    /// Concrete evidence asserted for this row, as stable markers:
    /// `doc_comment_absent` always, plus `externally_reachable` when the
    /// symbol is on the issue #213 public surface.
    pub evidence: Vec<&'static str>,
    /// `true` when the symbol reaches the surface through a `pub use`.
    pub via_reexport: bool,
    /// Crate-relative use-path the re-export points at (re-exports only).
    pub target: Option<String>,
    /// Record ID of the resolved re-export target whose doc fact was checked.
    pub target_record_id: Option<&'a str>,
}

/// Deterministic tallies for the undocumented-public-API lane.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct UndocumentedCounts {
    /// Doc-auditable candidates whose doc fact was asserted
    /// (`documented + undocumented`).
    pub considered: usize,
    /// Candidates carrying a recorded doc comment — excluded from `items`.
    pub documented: usize,
    /// Candidates with no recorded doc comment — the returned rows (before
    /// any `--limit` truncation).
    pub undocumented: usize,
    /// Undocumented rows contributed by `pub use` re-exports.
    pub reexports: usize,
    /// Surface `module` rows: modules carry no doc-comment fact and are
    /// excluded from the audit, never guessed.
    pub modules_excluded: usize,
    /// Re-export rows whose target did not resolve in-graph: doc presence
    /// cannot be asserted, so they are diagnosed, never reported.
    pub reexports_unresolved: usize,
    /// Doc-auditable symbol records carrying no issue #124 declaration
    /// surface (pre-#124 scan): their doc fact was never captured.
    pub doc_capture_missing: usize,
}

/// The undocumented-public-API report: rows, tallies, and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct UndocumentedReport<'a> {
    /// `true` when the store carries no doc-capture facts at all (pre-#124
    /// scan): the lane reports this verdict instead of treating every symbol
    /// as undocumented.
    pub capability_absent: bool,
    /// Undocumented symbols, sorted by (path, kind, record ID).
    pub items: Vec<UndocumentedItem<'a>>,
    /// Deterministic tallies.
    pub counts: UndocumentedCounts,
    /// Stable diagnostics, sorted and de-duplicated. Reuses the public-api
    /// diagnostic shape; surface diagnostics pass through (except
    /// `empty_surface`, which this lane replaces with its own verdicts).
    pub diagnostics: Vec<PublicApiDiagnostic>,
}

/// Returns the recorded (`doc`, `visibility`) facts of a symbol record.
fn symbol_doc_facts(record: &GraphRecord) -> (Option<&str>, Option<&str>) {
    if let GraphRecord::Node {
        doc, visibility, ..
    } = record
    {
        (doc.as_deref(), visibility.as_deref())
    } else {
        (None, None)
    }
}

/// Lists externally-reachable public symbols whose captured doc-comment fact
/// is absent (issue #257).
///
/// The result is the issue #213 public surface minus the has-doc set from
/// issue #124 — a graph-native join, never a `pub`-token grep.
///
/// The reachability rule is `public_api_surface`'s, reused verbatim: items
/// not externally reachable are excluded by default. A re-export counts as
/// documented when either the `pub use` site or the resolved target carries
/// a doc fact — rustdoc exposes site docs on the public item.
/// `include_private` widens the audit to every doc-auditable symbol (adding
/// `method` declarations) regardless of visibility, for whole-crate doc
/// audits; such rows carry their declared visibility class and never claim
/// `externally_reachable`.
///
/// Soundness boundary: the lane asserts the **presence or absence of a
/// recorded doc comment** (`///`, `/** */`, or `#[doc = "..."]`) — never doc
/// quality, accuracy, or completeness. When the store predates issue #124
/// doc capture, the report carries `capability_absent = true` and a
/// `doc_capture_unavailable` diagnostic instead of silently treating every
/// symbol as undocumented. Deterministic: output ordering depends only on
/// record content. `limit` truncates the sorted rows and adds a
/// `results_truncated` diagnostic; it never changes row order.
#[must_use]
pub fn undocumented_public_api<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    include_private: bool,
    limit: Option<usize>,
) -> UndocumentedReport<'a> {
    let surface = public_api_surface(records, index, repo_scope);

    // Doc-auditable symbol records (keep-last, current-state view), mirroring
    // the surface's scope: the Rust library crate, minus tombstones, within
    // the repo scope. Reachability itself is *not* re-derived here — it comes
    // from `public_api_surface` above.
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
    let mut symbols: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    // Doc facts recorded at `pub use` sites: rustdoc exposes a doc comment
    // written above the re-export on the public item, so a site doc counts
    // as documentation for the re-exported symbol.
    let mut import_docs: BTreeMap<&str, &'a str> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            language,
            repo_relative_path,
            symbol_kind,
            doc,
            ..
        } = record
        else {
            continue;
        };
        if language.as_deref() != Some("rust")
            || tombstoned.contains(id.as_str())
            || !is_owned(id)
            || !repo_relative_path
                .as_deref()
                .is_some_and(is_library_crate_path)
        {
            continue;
        }
        match kind {
            NodeKind::Symbol => {
                let auditable = symbol_kind
                    .as_deref()
                    .is_some_and(|k| PUBLIC_API_SYMBOL_KINDS.contains(&k) || k == "method");
                if !auditable {
                    continue;
                }
                symbols.insert(id.as_str(), record);
            }
            NodeKind::Import => {
                if let Some(doc) = doc.as_deref() {
                    import_docs.insert(id.as_str(), doc);
                }
            }
            _ => {}
        }
    }

    let mut report = UndocumentedReport::default();

    // Capability check: a store where no doc-auditable symbol carries the
    // issue #124 declaration surface never captured doc facts. Report that
    // verdict — never treat "no doc field" in a pre-#124 store as "no docs".
    let mut facts_recorded = 0usize;
    for record in symbols.values() {
        let (_, visibility) = symbol_doc_facts(record);
        if visibility.is_some() {
            facts_recorded += 1;
        } else {
            report.counts.doc_capture_missing += 1;
        }
    }
    report.capability_absent = facts_recorded == 0 && report.counts.doc_capture_missing > 0;

    // Surface diagnostics pass through so glob re-exports and unknown module
    // visibility stay visible; `empty_surface` is replaced by this lane's own
    // explicit verdicts.
    report.diagnostics.extend(
        surface
            .diagnostics
            .iter()
            .filter(|d| d.code != "empty_surface")
            .cloned(),
    );

    if report.capability_absent {
        report.diagnostics.push(PublicApiDiagnostic {
            code: "doc_capture_unavailable",
            record_id: None,
            detail: format!(
                "{} doc-auditable symbol record(s) carry no issue #124 declaration \
                 surface; doc-comment facts were never captured. Re-scan with a \
                 current build to audit documentation",
                report.counts.doc_capture_missing
            ),
        });
        sort_undocumented(&mut report);
        return report;
    }

    // IDs whose doc fact was already asserted via the surface, so the
    // `--include-private` widening never double-reports a symbol.
    let mut asserted: BTreeSet<&str> = BTreeSet::new();

    for item in surface.items {
        if item.kind == "module" {
            report.counts.modules_excluded += 1;
            continue;
        }
        // A doc comment at the `pub use` site documents the re-exported item
        // (rustdoc attaches it to the public name), regardless of whether the
        // target declaration carries its own doc.
        if item.via_reexport && import_docs.contains_key(item.record_id) {
            report.counts.considered += 1;
            report.counts.documented += 1;
            continue;
        }
        if item.kind == "reexport" {
            report.counts.reexports_unresolved += 1;
            report.diagnostics.push(PublicApiDiagnostic {
                code: "reexport_target_unresolved",
                record_id: Some(item.record_id.to_owned()),
                detail: format!(
                    "pub use target `{}` does not resolve in-graph; doc presence \
                     cannot be asserted for it",
                    item.target.as_deref().unwrap_or("")
                ),
            });
            continue;
        }
        // The record whose doc fact backs this row: the declaration itself,
        // or the resolved target for a re-export row. A re-export row without
        // a target record ID is unresolved; count it, never guess.
        let fact_id = match (item.via_reexport, item.target_record_id) {
            (false, _) => item.record_id,
            (true, Some(target_id)) => target_id,
            (true, None) => {
                report.counts.reexports_unresolved += 1;
                continue;
            }
        };
        let Some(record) = symbols.get(fact_id) else {
            // Resolved to a non-auditable record (e.g. a module alias row is
            // already handled above); never guess a doc fact.
            report.counts.reexports_unresolved += 1;
            continue;
        };
        let (doc, visibility) = symbol_doc_facts(record);
        if visibility.is_none() {
            // Pre-#124 record in a mixed store: already tallied in
            // `doc_capture_missing`; its doc fact cannot be asserted.
            continue;
        }
        asserted.insert(fact_id);
        report.counts.considered += 1;
        if doc.is_some() {
            report.counts.documented += 1;
            continue;
        }
        report.counts.undocumented += 1;
        if item.via_reexport {
            report.counts.reexports += 1;
        }
        report.items.push(UndocumentedItem {
            record_id: item.record_id,
            kind: item.kind,
            path: item.path,
            visibility: "public",
            repo_relative_path: item.repo_relative_path,
            span: item.span,
            signature: item.signature,
            evidence: vec!["externally_reachable", "doc_comment_absent"],
            via_reexport: item.via_reexport,
            target: item.target,
            target_record_id: item.target_record_id,
        });
    }

    if include_private {
        for (id, record) in &symbols {
            if asserted.contains(id) {
                continue;
            }
            let GraphRecord::Node {
                name: Some(name),
                repo_relative_path,
                span,
                symbol_kind: Some(symbol_kind),
                signature,
                ..
            } = record
            else {
                continue;
            };
            let (doc, visibility) = symbol_doc_facts(record);
            let Some(visibility) = visibility else {
                continue; // pre-#124 record: already tallied, never guessed.
            };
            report.counts.considered += 1;
            if doc.is_some() {
                report.counts.documented += 1;
                continue;
            }
            report.counts.undocumented += 1;
            report.items.push(UndocumentedItem {
                record_id: id,
                kind: symbol_kind.clone(),
                path: name.clone(),
                visibility,
                repo_relative_path: repo_relative_path.as_deref(),
                span: *span,
                signature: signature.as_deref(),
                evidence: vec!["doc_comment_absent"],
                via_reexport: false,
                target: None,
                target_record_id: None,
            });
        }
    }

    if report.items.is_empty() {
        // `no_undocumented_items` certifies the audit clean, so it requires
        // an audit with no blind spots. When unresolved re-exports or
        // missing doc capture left symbols unasserted, the empty result gets
        // an honest distinct verdict instead — still exit 0, never an error.
        let blind_spots =
            report.counts.reexports_unresolved > 0 || report.counts.doc_capture_missing > 0;
        report.diagnostics.push(if blind_spots {
            PublicApiDiagnostic {
                code: "empty_result_with_blind_spots",
                record_id: None,
                detail: format!(
                    "no undocumented symbols found, but the audit has blind spots \
                     ({} unresolved re-export(s), {} symbol record(s) without doc \
                     capture); this is not a certified-clean claim",
                    report.counts.reexports_unresolved, report.counts.doc_capture_missing
                ),
            }
        } else {
            PublicApiDiagnostic {
                code: "no_undocumented_items",
                record_id: None,
                detail: format!(
                    "every doc-auditable symbol in scope carries a recorded doc \
                     comment ({} considered)",
                    report.counts.considered
                ),
            }
        });
    }

    sort_undocumented(&mut report);

    if let Some(limit) = limit {
        if report.items.len() > limit {
            let total = report.items.len();
            report.items.truncate(limit);
            report.diagnostics.push(PublicApiDiagnostic {
                code: "results_truncated",
                record_id: None,
                detail: format!(
                    "showing {limit} of {total} undocumented rows; raise --limit \
                     to see the rest"
                ),
            });
            // Re-sort so the appended diagnostic keeps the stable order.
            sort_undocumented_diagnostics(&mut report.diagnostics);
        }
    }

    report
}

/// Sorts an undocumented report's rows and diagnostics deterministically.
fn sort_undocumented(report: &mut UndocumentedReport<'_>) {
    report.items.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.kind.cmp(&b.kind))
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    sort_undocumented_diagnostics(&mut report.diagnostics);
}

/// Sorts and de-duplicates a diagnostics list deterministically.
fn sort_undocumented_diagnostics(diagnostics: &mut Vec<PublicApiDiagnostic>) {
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.record_id.cmp(&b.record_id))
            .then_with(|| a.detail.cmp(&b.detail))
    });
    diagnostics.dedup();
}
