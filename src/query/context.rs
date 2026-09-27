use std::collections::BTreeSet;

use super::{
    PolicyEntry, evidence_link_triple_handle, is_bfs_relay_node, is_cross_domain_label,
    is_forward_only_label,
};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

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

/// One distinct symbol identity sharing a queried name.
///
/// Surfaced in [`SymbolContext::candidates`] when a name resolves to more
/// than one distinct symbol identity (issue #192). The caller picks one by
/// [`SymbolCandidate::record_id`] or [`SymbolCandidate::file_span_handle`]
/// and re-queries (via [`record_context`] or the `candidate` selector on the
/// `symbol_context` MCP tool / `--candidate` on `eg query context`) for
/// context scoped to exactly that symbol.
///
/// The struct is `Serialize` so the MCP and CLI lanes can render the
/// disambiguation list without a second mapping step.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct SymbolCandidate<'a> {
    /// Stable record ID of the symbol node (ADR 0004).
    pub record_id: &'a str,
    /// The queried symbol name (identical for every candidate).
    pub symbol_name: &'a str,
    /// Repo-relative defining path, when the symbol carries one.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the symbol definition, when recorded.
    pub span: Option<&'a SourceSpan>,
}

impl SymbolCandidate<'_> {
    /// Repo-relative `file:span` handle for this candidate:
    /// `path:start_line-end_line`.
    ///
    /// Falls back to the bare repo-relative path when the symbol carries no
    /// span, and to the record ID when it carries no path either. The
    /// `path:start_line-end_line` form round-trips through
    /// [`resolve_symbol_file_span_handle`]; the fallbacks are display-only.
    #[must_use]
    pub fn file_span_handle(&self) -> String {
        match (self.repo_relative_path, self.span) {
            (Some(path), Some(span)) => {
                format!("{path}:{}-{}", span.start_line, span.end_line)
            }
            (Some(path), None) => path.to_owned(),
            (None, _) => self.record_id.to_owned(),
        }
    }
}

/// Evidence-backed symbol context returned by [`symbol_context`].
///
/// Sections are kept separate so the caller can present source facts
/// (deterministic) differently from observations (subjective) without
/// mixing trust levels. An [`Observation`] node MUST NOT appear in
/// [`SymbolContext::source_facts`].
///
/// When the queried name resolves to more than one distinct symbol identity,
/// the recall is *ambiguous* (issue #192): every section is empty and
/// [`SymbolContext::candidates`] enumerates the identities instead. Merging
/// the sections would attribute one symbol's history to another, so the
/// sections are NEVER populated on an ambiguous result — check
/// [`SymbolContext::is_ambiguous`] before inspecting them.
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
    ///
    /// Section contract (issue #191): this section holds `Observation` and
    /// `Failure` records only. `Decision` records never appear here — they
    /// surface in [`SymbolContext::decisions`].
    pub observations: Vec<&'a GraphRecord>,
    /// Agent-authored `Decision` nodes linked to the symbol, carrying
    /// `decision_text` and `rationale_summary` (issue #191).
    ///
    /// Trust contract: decisions are agent-authored and evidence-backed, but
    /// they are deliberate judgments — never deterministic source truth.
    /// Treat the rationale as a claim to verify before reusing, not as a
    /// fact to cite. A decision never appears in
    /// [`SymbolContext::observations`].
    pub decisions: Vec<&'a GraphRecord>,
    /// `Task` and `AcceptanceCriterion` nodes linked to the symbol.
    pub project_state: Vec<&'a GraphRecord>,
    /// `Artifact` and `PatchArtifact` nodes linked to the symbol.
    pub artifacts: Vec<&'a GraphRecord>,
    /// `Verification`, `TestRun`, `CommandRun`, and `CommandEvidence` nodes
    /// linked to the symbol.
    pub verification_evidence: Vec<&'a GraphRecord>,
    /// `SemanticDrift` records whose resolved target (via a `DriftsFrom` edge,
    /// falling back to `target_record_id`) is one of the matched symbol's own
    /// records (issue #108). Deterministic source-derived evidence — never
    /// mixed into `observations`. Ordered score descending, then record ID,
    /// matching [`super::largest_semantic_drifts`].
    pub drift_history: Vec<&'a GraphRecord>,
    /// Evidence link targets referenced by agent-memory nodes that are absent
    /// from this store slice. Surfaced explicitly per AC5.
    pub unresolved: Vec<UnresolvedRef>,
    /// Disambiguation candidates, populated ONLY when the queried name
    /// resolves to more than one distinct symbol identity (issue #192).
    ///
    /// One entry per distinct identity, deterministically ordered by
    /// (repo-relative path, span start line, record ID). Every section is
    /// empty on an ambiguous result — the sections are never a merge of
    /// several identities. Empty for no-match and single-match results.
    pub candidates: Vec<SymbolCandidate<'a>>,
    /// Active approved user-context policy records whose scope applies to
    /// the anchor (issue #169).
    ///
    /// Folded from the durable policy kinds (`Preference`, `WorkflowRule`,
    /// `NamingDecision`, `Constraint`) via
    /// [`policy_for_anchor`][crate::query::policy_for_anchor]: only records
    /// with a validated audit chain and no `active_to` (superseded/revoked
    /// excluded), whose [`UserContextScope`][crate::ir::UserContextScope]
    /// applies to the scope auto-derived from the anchor's own facts
    /// (owning repository, repo-relative path, language) — no caller-supplied
    /// policy filter. Pending, rejected, and deferred candidates never
    /// surface here; they remain visible via `eg query policy`.
    ///
    /// Trust: rows carry the approval-decision handle that materialized them
    /// and classify as [`TrustClass::Other`][crate::query::TrustClass] under
    /// the #114 closed vocabulary — distinguishable from source-derived code
    /// facts and unverified observations.
    ///
    /// Always present (possibly empty), like every other section: a context
    /// with no applicable policy carries an explicit empty `policy` section,
    /// never a missing field. Empty on ambiguous and no-match results.
    pub policy: Vec<PolicyEntry<'a>>,
}

impl SymbolContext<'_> {
    /// Returns `true` when no symbol with the queried name exists in the store.
    ///
    /// Callers MUST check [`SymbolContext::is_ambiguous`] first: an ambiguous
    /// name has empty sections but is a match, not a no-match.
    #[must_use]
    pub const fn is_no_match(&self) -> bool {
        self.source_facts.is_empty()
            && self.observations.is_empty()
            && self.decisions.is_empty()
            && self.project_state.is_empty()
            && self.artifacts.is_empty()
            && self.verification_evidence.is_empty()
            && self.unresolved.is_empty()
            && self.candidates.is_empty()
    }

    /// Returns `true` when the queried name resolved to more than one
    /// distinct symbol identity (issue #192).
    ///
    /// The sections are empty on an ambiguous result; the identities are
    /// enumerated in [`SymbolContext::candidates`] instead of being merged.
    /// Callers MUST check this before inspecting sections — and before
    /// `is_no_match`, which is `false` for ambiguous results.
    #[must_use]
    pub const fn is_ambiguous(&self) -> bool {
        !self.candidates.is_empty()
    }
}

/// Classifies a [`NodeKind`] into one of the five context sections.
///
/// Returns `None` for kinds that do not belong to any section (e.g. edges,
/// tombstones, infrastructure nodes like `Repository`, `Agent`, `AgentSession`).
pub(super) const fn classify_node(kind: NodeKind) -> Option<ContextSection> {
    match kind {
        // code-graph source facts
        NodeKind::Symbol | NodeKind::File | NodeKind::Module | NodeKind::Import => {
            Some(ContextSection::SourceFact)
        }
        // agent-authored observations and failure records.
        //
        // Section contract (issue #191): `Observation` and `Failure` share
        // the observations section; `Decision` has its own. Failures stay
        // here deliberately — they are post-hoc notes about what went wrong,
        // not deliberate judgments — and this comment is the documented
        // routing the issue requires instead of silent flattening.
        NodeKind::Observation | NodeKind::Failure => Some(ContextSection::Observation),
        // agent-authored decisions with rationale (issue #191): a dedicated
        // section, never mixed into observations.
        NodeKind::Decision => Some(ContextSection::Decision),
        // project / task domain
        NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::LocalTask
        | NodeKind::GitHubIssue
        | NodeKind::PR
        // Reviewer-identity join (issue #335): a Review row surfaces in the
        // project-state section, and the ExternalIdentity reached via
        // REVIEWED_BY / REQUESTED_REVIEW_FROM carries the participant login.
        | NodeKind::Review
        | NodeKind::ExternalIdentity
        // Review-state history (issue #336): a ReviewStateTransition is a
        // project-state fact — the append-only record of a review's dismissal or
        // request transition — so it surfaces alongside the Review it concerns.
        | NodeKind::ReviewStateTransition => Some(ContextSection::ProjectState),
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
pub(super) enum ContextSection {
    SourceFact,
    Observation,
    /// Agent-authored decisions with rationale (issue #191). Trust-separated
    /// from observations: a decision is a deliberate, evidence-backed
    /// judgment — never an ad-hoc note and never source truth.
    Decision,
    ProjectState,
    Artifact,
    VerificationEvidence,
}

/// Builds the ambiguous-recall result for issue #192: one
/// [`SymbolCandidate`] per distinct symbol identity, sections left empty.
///
/// `symbol_ids` is the exact set of identities the current-state recall
/// would otherwise have merged (tombstone and temporal rules already
/// applied), so ambiguity detection counts precisely those. Candidates cite
/// the current-state (non-temporal) record's path/span when one exists,
/// falling back to any version so a historical-only identity still gets a
/// handle. Ordered by (repo-relative path, span start line, record ID) for
/// a deterministic, human-scannable disambiguation list.
fn ambiguous_symbol_context<'a>(
    records: &'a [GraphRecord],
    symbol_name: &'a str,
    symbol_ids: &BTreeSet<&'a str>,
) -> SymbolContext<'a> {
    /// Path/span/name of the current-state record for `id`, falling back to
    /// any version present in the slice.
    fn current_record_parts<'a>(
        records: &'a [GraphRecord],
        id: &str,
    ) -> Option<(Option<&'a str>, Option<&'a SourceSpan>, Option<&'a str>)> {
        let mut fallback: Option<(Option<&'a str>, Option<&'a SourceSpan>, Option<&'a str>)> = None;
        for record in records {
            let GraphRecord::Node {
                id: record_id,
                kind: NodeKind::Symbol,
                name,
                repo_relative_path,
                span,
                temporal,
                ..
            } = record
            else {
                continue;
            };
            if record_id.as_str() != id {
                continue;
            }
            let parts = (
                repo_relative_path.as_deref(),
                span.as_ref(),
                name.as_deref(),
            );
            if temporal.is_none() {
                return Some(parts);
            }
            fallback.get_or_insert(parts);
        }
        fallback
    }

    let mut candidates: Vec<SymbolCandidate<'a>> = symbol_ids
        .iter()
        .copied()
        .filter_map(|id| {
            let (repo_relative_path, span, name) = current_record_parts(records, id)?;
            Some(SymbolCandidate {
                record_id: id,
                symbol_name: name.unwrap_or(symbol_name),
                repo_relative_path,
                span,
            })
        })
        .collect();
    candidates.sort_by(|a, b| {
        (
            a.repo_relative_path,
            a.span.map(|s| s.start_line),
            a.record_id,
        )
            .cmp(&(
                b.repo_relative_path,
                b.span.map(|s| s.start_line),
                b.record_id,
            ))
    });

    SymbolContext {
        symbol_name: symbol_name.to_owned(),
        candidates,
        ..Default::default()
    }
}

/// Resolves a `file:span` handle (`path:start_line-end_line`, as produced by
/// [`SymbolCandidate::file_span_handle`]) back to the symbol record.
///
/// This is the re-query half of the issue #192 disambiguation contract: a
/// caller holding a candidate handle anchors [`record_context`] on the
/// returned record for context scoped to exactly that symbol.
///
/// Returns `None` when the handle is malformed, matches no live symbol, or
/// matches more than one distinct identity — ambiguity is never resolved
/// implicitly. Tombstoned current-state records do not resolve (a historical
/// temporal version survives its current-state tombstone, mirroring the
/// recall's liveness rules).
#[must_use]
pub fn resolve_symbol_file_span_handle<'a>(
    records: &'a [GraphRecord],
    handle: &str,
) -> Option<&'a GraphRecord> {
    // Split at the LAST colon so paths containing colons keep working.
    let (path, span_part) = handle.rsplit_once(':')?;
    let (start_text, end_text) = span_part.split_once('-')?;
    if path.is_empty() {
        return None;
    }
    let start_line: usize = start_text.parse().ok()?;
    let end_line: usize = end_text.parse().ok()?;

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

    let mut found: Option<&'a GraphRecord> = None;
    let mut found_id: Option<&str> = None;
    for record in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            repo_relative_path: Some(record_path),
            span: Some(record_span),
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if record_path.as_str() != path
            || record_span.start_line != start_line
            || record_span.end_line != end_line
        {
            continue;
        }
        // Current-state liveness: a tombstoned record resolves only via a
        // surviving historical version.
        if temporal.is_none() && tombstoned_ids.contains(id.as_str()) {
            continue;
        }
        match found_id {
            // Same identity in two versions (current + temporal): one answer.
            Some(known) if known == id.as_str() => {}
            // Two distinct identities, one handle: fail closed.
            Some(_) => return None,
            None => {
                found_id = Some(id.as_str());
                found = Some(record);
            }
        }
    }
    found
}

/// Returns all known context for a named symbol, separated by trust domain.
///
/// # Algorithm
///
/// 1. Collect all `Symbol` node IDs for `symbol_name`.
/// 2. Issue #192: when the name resolves to two or more distinct symbol
///    identities, the recall is ambiguous — return an empty-sectioned
///    [`SymbolContext`] whose [`SymbolContext::candidates`] enumerates the
///    identities instead of merging them. Distinct identities are distinct
///    stable record IDs (ADR 0004); tombstone and temporal rules from step 1
///    decide which identities count, so the ambiguity set is exactly what
///    the recall would otherwise have blended.
/// 3. Add those symbol nodes to `source_facts`.
/// 4. Scan every other record:
///    a. Edges: if source or target is a known symbol ID, follow the other
///    end and classify the referenced node.
///    b. Nodes with `evidence_links`: for each link whose `target_record_id`
///    is a known symbol ID, classify the linking node.
/// 5. Collect any evidence link targets that are not present in the store
///    slice into `unresolved`.
///
/// Output ordering within each section is sorted by record ID for determinism
/// (AC7).
///
/// An empty [`SymbolContext`] where [`SymbolContext::is_no_match`] returns
/// `true` is returned when the symbol is not found. Callers MUST check
/// [`SymbolContext::is_ambiguous`] before `is_no_match` — an ambiguous name
/// has empty sections but is a match, not a no-match. There is no panic or
/// error path.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn symbol_context<'a>(records: &'a [GraphRecord], symbol_name: &'a str) -> SymbolContext<'a> {
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

    // Issue #472: targets of ACTIVE repository-eviction tombstones are suppressed
    // from this current-state lane, including their temporal snapshots. Ordinary
    // `forget` tombstones keep the temporal exemption below — only eviction
    // tombstones suppress history.
    let evicted_ids = crate::repo_evict::active_eviction_tombstoned_ids(records);

    // Step 1: collect symbol record IDs, excluding tombstoned CURRENT-STATE records.
    //
    // Temporal records (from scan-history, carrying `temporal` metadata) are
    // historical snapshots — they must NOT be suppressed by a tombstone that
    // reflects deletion only in the current state (issue #231). However, they
    // ARE suppressed by an ACTIVE repository-eviction tombstone (issue #472).
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
            // Issue #472: evicted snapshots are suppressed even though temporal.
            if temporal.is_some() && evicted_ids.contains(id.as_str()) {
                return None;
            }
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

    // Issue #192: distinct symbol identities sharing one name are never
    // merged. When the name resolves to two or more distinct identities — the
    // exact set the current-state recall below would otherwise have blended
    // into one answer — report the ambiguity and enumerate the candidates
    // instead of populating the sections.
    if symbol_ids.len() >= 2 {
        return ambiguous_symbol_context(records, symbol_name, &symbol_ids);
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

    context_from_seeds(records, symbol_name, source_facts, &symbol_ids, true)
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
///
/// `compute_drift_history` gates the `drift_history` section's O(N) scan
/// (issue #497 Codex review): only [`symbol_context`] (`eg query context`)
/// renders it, per CLAUDE.md's documented scope — `eg query locate` and
/// `eg query semantic-context` (both routed through [`record_context`], the
/// latter once per retrieval lead) never surface it. `record_context` passes
/// `false` so those callers skip the scan entirely rather than compute and
/// immediately discard it; `symbol_context` passes `true`.
#[must_use]
#[allow(clippy::too_many_lines)]
fn context_from_seeds<'a>(
    records: &'a [GraphRecord],
    symbol_name: &str,
    source_facts: BTreeSet<&'a str>,
    symbol_ids: &BTreeSet<&'a str>,
    compute_drift_history: bool,
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
    // Liveness view for BFS relay-node checks.
    let liveness = crate::query::liveness::Liveness::new(records);
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
    let mut decisions: BTreeSet<&str> = BTreeSet::new();
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
                               decisions: &mut BTreeSet<&'a str>,
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
            Some(ContextSection::Decision) => {
                decisions.insert(record_id);
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
                            &mut decisions,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        // Expand nodes that were classified OR that are valid relay
                        // kinds (e.g. ToolCall) which bridge classifiable sections but
                        // have no output section of their own. Tombstoned and missing
                        // (by_id miss) nodes still must not enter the frontier.
                        if was_classified || is_bfs_relay_node(id, &by_id, &liveness) {
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
                            &mut decisions,
                            &mut project_state,
                            &mut artifacts,
                            &mut verification_evidence,
                        );
                        visited.insert(node_id.as_str());
                        if was_classified || is_bfs_relay_node(node_id.as_str(), &by_id, &liveness)
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
                                        &mut decisions,
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
                            &mut decisions,
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
        .chain(decisions.iter())
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
                        &mut decisions,
                        &mut project_state,
                        &mut artifacts,
                        &mut verification_evidence,
                    );
                    if was_classified || is_bfs_relay_node(id, &by_id, &liveness) {
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
        decisions.remove(sid);
        project_state.remove(sid);
        artifacts.remove(sid);
        verification_evidence.remove(sid);
    }

    // Drift history (issue #108): SemanticDrift records whose resolved target
    // (DriftsFrom edge, falling back to target_record_id) is one of the
    // primary query nodes. Ordered score descending, then record ID — the
    // exact same comparator `largest_semantic_drifts` uses for `eg query
    // drift` (AC3) — but applied only to the (typically tiny) subset that
    // survives filtering, not to every drift record in the store: filtering
    // FIRST and sorting the small result, rather than sorting every drift via
    // `largest_semantic_drifts` and filtering afterward, avoids resorting all
    // D drift records on every one of the many `symbol_context` calls
    // `eg audit citations` makes (one call per distinct symbol name — an
    // O(D log D) resort per call adds up to O(S * D log D) across S symbols).
    // Deliberately NOT run through `classify_node`/the BFS above:
    // `SemanticDrift` is not one of the five trust-separated sections and
    // must never be mixed into `observations` (AC5).
    //
    // A tombstoned non-temporal drift node is excluded — the same liveness
    // exception used everywhere else in this function (the `resolve` closure
    // below) and by `subsystem.rs`'s own `semantic_drift` section: a temporal
    // (scan-history) drift record survives its current-state tombstone, but a
    // retracted current-state one must not reappear as live evidence.
    //
    // The DriftsFrom source->target lookup is built once in a single O(n)
    // pass rather than re-scanning `records` per drift record (which
    // `drift_target_record_id` does, since it also serves `eg query drift`'s
    // once-per-row callers): with D drift records over N total records this
    // keeps the section O(N) instead of O(D * N).
    //
    // First-edge-wins: if a malformed/hand-crafted graph carries more than one
    // DriftsFrom edge for the same drift source, `entry().or_insert()` keeps
    // the first one encountered — matching `drift_target_record_id`'s
    // `find_map` (which returns on the first match) so this map can never
    // resolve a drift to a different target than `eg query drift` does.
    //
    // Skipped entirely when `compute_drift_history` is false (issue #497 Codex
    // review): `record_context`'s callers (`eg query locate`,
    // `eg query semantic-context` — the latter once per retrieval lead — and
    // `error_context`'s once-per-frame-target loop) never read this section,
    // so computing and immediately discarding it would waste an O(N) scan per
    // call/lead with no observable effect.
    let drift_history: Vec<&'a GraphRecord> = if compute_drift_history {
        let mut drifts_from_target: std::collections::BTreeMap<&str, &str> =
            std::collections::BTreeMap::new();
        for r in records {
            if let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = r
            {
                drifts_from_target
                    .entry(source.as_str())
                    .or_insert(target.as_str());
            }
        }
        let mut drift_history: Vec<(&'a GraphRecord, f64)> = records
            .iter()
            .filter_map(|record| {
                let GraphRecord::Node {
                    id,
                    kind,
                    semantic_drift: Some(drift),
                    temporal,
                    ..
                } = record
                else {
                    return None;
                };
                // Require the SemanticDrift kind, matching `semantic_drift()`'s
                // own check: the field is a general optional slot on every Node
                // variant, so a malformed graph could carry it on some other
                // kind. Without this a non-drift node would surface here (and
                // now in the citation audit via drive_context) while
                // `largest_semantic_drifts`/`eg query drift` correctly reject it.
                if *kind != NodeKind::SemanticDrift {
                    return None;
                }
                if temporal.is_none() && tombstoned_ids.contains(id.as_str()) {
                    return None;
                }
                let target_id = drifts_from_target
                    .get(id.as_str())
                    .copied()
                    .unwrap_or(drift.target_record_id.as_str());
                symbol_ids
                    .contains(target_id)
                    .then_some((record, drift.score))
            })
            .collect();
        drift_history.sort_by(|(left_record, left_score), (right_record, right_score)| {
            right_score
                .partial_cmp(left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left_record.id().cmp(right_record.id()))
        });
        drift_history.into_iter().map(|(r, _)| r).collect()
    } else {
        Vec::new()
    };

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

    // Issue #169: fold active approved policy whose scope applies to the
    // anchor's own facts (owning repository, repo-relative path, language).
    // Both lanes reach this constructor with exactly one anchor ID, so the
    // policy section is always attributable to a single identity; ambiguous
    // and no-match results return earlier with an empty (default) section.
    let policy = symbol_ids
        .iter()
        .next()
        .and_then(|anchor_id| records.iter().find(|r| r.id() == *anchor_id))
        .map(|anchor| super::policy_for_anchor(records, anchor))
        .unwrap_or_default();

    SymbolContext {
        symbol_name: symbol_name.to_owned(),
        policy,
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
        decisions: resolve(&decisions),
        project_state: resolve(&project_state),
        artifacts: resolve(&artifacts),
        verification_evidence: resolve(&verification_evidence),
        drift_history,
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
        // Single-match recall: no disambiguation needed (issue #192).
        candidates: Vec::new(),
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
                    // An `IMPORTS` edge is followed only in its containment
                    // shape (`File —IMPORTS→ Import`): the issue-#444
                    // module-target edge (`File —IMPORTS→ Module|File`) is a
                    // dependency edge into ANOTHER file's tree, and following
                    // it would leak the imported module's symbols into this
                    // file's context.
                    if matches!(label, EdgeLabel::Imports)
                        && !matches!(target_kind, Some(NodeKind::Import))
                    {
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

    // `record_context`'s callers (`locate`, `semantic_context_bundle`,
    // `error_context`) never read `drift_history` — see the parameter's
    // doc comment on `context_from_seeds`.
    context_from_seeds(records, &label, source_facts, &primary, false)
}

// ── semantic → context bridge (issue #90) ──────────────────────────────────

#[cfg(test)]
mod reviewer_identity_tests {
    use super::*;
    use crate::ir::{EvidenceLink, PROJECT_SCHEMA_VERSION};

    fn sym(name: &str) -> GraphRecord {
        GraphRecord::node(
            format!("codegraph:v5:{name}"),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn project_node(id: &str, kind: NodeKind, login: Option<&str>) -> GraphRecord {
        let mut rec = GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("{} node", kind.as_str()),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            author,
            identity_system,
            ..
        } = &mut rec
        {
            *schema_version = PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
            if kind == NodeKind::ExternalIdentity {
                *author = login.map(str::to_owned);
                *identity_system = Some("github".to_owned());
            }
        }
        rec
    }

    fn id_for(login: &str) -> String {
        crate::github::records::external_identity_id("github", login)
    }

    fn ev_link_to(target: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: "MENTIONS_SYMBOL".to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    #[test]
    fn symbol_context_surfaces_reviewer_identity_via_reviewed_by_and_requested_review_from() {
        // Issue #335: a symbol's project-state context includes the reviewer
        // identities reachable via REVIEWED_BY (from a Review that cites the
        // symbol) and REQUESTED_REVIEW_FROM (from a Task that mentions it).
        let foo = sym("foo");
        let foo_id = foo.id().to_owned();

        // Task that mentions the symbol; requests review from "alice".
        let task = project_node("project:v1:task", NodeKind::Task, None);
        let id_alice = project_node(&id_for("alice"), NodeKind::ExternalIdentity, Some("alice"));
        let mentions = GraphRecord::project_edge(
            EdgeLabel::MentionsSymbol,
            task.id().to_owned(),
            foo_id.clone(),
            Some("1.0".to_owned()),
            "task mentions foo".to_owned(),
        );
        let requested = GraphRecord::project_edge(
            EdgeLabel::RequestedReviewFrom,
            task.id().to_owned(),
            id_alice.id().to_owned(),
            None,
            "requested review".to_owned(),
        );

        // Review that cites the symbol via evidence_link; authored by "bob".
        let mut review = project_node("project:v1:review", NodeKind::Review, None);
        review = review.with_evidence_links(vec![ev_link_to(&foo_id)]);
        let id_bob = project_node(&id_for("bob"), NodeKind::ExternalIdentity, Some("bob"));
        let reviewed = GraphRecord::project_edge(
            EdgeLabel::ReviewedBy,
            review.id().to_owned(),
            id_bob.id().to_owned(),
            None,
            "review by bob".to_owned(),
        );

        let records = vec![
            foo, task, id_alice, mentions, requested, review, id_bob, reviewed,
        ];
        let ctx = symbol_context(&records, "foo");
        let project_ids: Vec<&str> = ctx.project_state.iter().map(|r| r.id()).collect();

        assert!(
            project_ids.contains(&id_for("alice").as_str()),
            "requested-reviewer identity must surface: {project_ids:?}"
        );
        assert!(
            project_ids.contains(&id_for("bob").as_str()),
            "review-author identity must surface: {project_ids:?}"
        );

        // Byte-stable across repeated runs.
        for _ in 0..5 {
            let again = symbol_context(&records, "foo");
            let ids: Vec<&str> = again.project_state.iter().map(|r| r.id()).collect();
            assert_eq!(ids, project_ids, "project_state must be byte-stable");
        }
    }
}

#[cfg(test)]
mod drift_history_tests {
    use super::*;
    use crate::ir::{EmbeddingModel, MetricKind, SelectionBasis, SemanticDriftMetadata};

    fn sym(name: &str) -> GraphRecord {
        GraphRecord::node(
            format!("codegraph:v5:sym-{name}"),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn drift_metadata(target_id: &str, score: f64) -> SemanticDriftMetadata {
        SemanticDriftMetadata {
            embedding_model: EmbeddingModel {
                provider: "test".to_owned(),
                name: "test-model".to_owned(),
                version: "v1".to_owned(),
                dim: 8,
                content_hash: "unknown".to_owned(),
            },
            target_record_id: target_id.to_owned(),
            prior_record_id: target_id.to_owned(),
            before_git_commit: "aaaaaaa".to_owned(),
            after_git_commit: "bbbbbbb".to_owned(),
            before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
            after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
            metric_kind: MetricKind::CosineDistance,
            score,
            selection_threshold: 0.2,
            selection_basis: SelectionBasis::ThresholdOnly,
        }
    }

    fn drift_node(drift_id: &str, target_id: &str, score: f64) -> GraphRecord {
        GraphRecord::node(
            drift_id.to_owned(),
            NodeKind::SemanticDrift,
            None,
            None,
            None,
            "drift".to_owned(),
        )
        .with_semantic_drift(drift_metadata(target_id, score))
    }

    fn drifts_from_edge(drift_id: &str, target_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::DriftsFrom,
            drift_id.to_owned(),
            target_id.to_owned(),
            Some("1.0".to_owned()),
            "drift edge".to_owned(),
        )
    }

    /// AC1 + AC3: drift records whose resolved target is the matched symbol
    /// surface in `drift_history`, ordered score descending then record ID.
    #[test]
    fn symbol_context_includes_drift_history_ordered_by_score_desc() {
        let target = sym("foo");
        let target_id = target.id().to_owned();

        let small = drift_node("semantic:v1:small", &target_id, 0.2);
        let large = drift_node("semantic:v1:large", &target_id, 0.9);
        let edge_small = drifts_from_edge("semantic:v1:small", &target_id);
        let edge_large = drifts_from_edge("semantic:v1:large", &target_id);

        let records = vec![target, small, large, edge_small, edge_large];
        let ctx = symbol_context(&records, "foo");

        let ids: Vec<&str> = ctx.drift_history.iter().map(|r| r.id()).collect();
        assert_eq!(
            ids,
            vec!["semantic:v1:large", "semantic:v1:small"],
            "drift_history must be ordered score-descending: {ids:?}"
        );
    }

    /// AC4: a symbol with no drift records returns an empty `drift_history`,
    /// not an error, and does not change the no-match verdict.
    #[test]
    fn symbol_context_drift_history_empty_when_no_drift_records() {
        let target = sym("bar");
        let records = vec![target];
        let ctx = symbol_context(&records, "bar");
        assert!(ctx.drift_history.is_empty());
        assert!(!ctx.is_no_match());
    }

    /// AC4: a `SemanticDrift` record whose target references a symbol name
    /// that does not exist in the slice must not fabricate a match — the
    /// query for that name stays a genuine no-match, drift and all.
    #[test]
    fn symbol_context_drift_referencing_nonexistent_symbol_stays_no_match() {
        let drift_rec = drift_node(
            "semantic:v1:orphan-drift",
            "codegraph:v5:sym-nonexistent",
            0.9,
        );
        let records = vec![drift_rec];
        let ctx = symbol_context(&records, "nonexistent");
        assert!(ctx.is_no_match(), "no live Symbol named this must no-match");
        assert!(ctx.drift_history.is_empty());
    }

    /// When a malformed/hand-crafted graph carries more than one `DriftsFrom`
    /// edge for the same drift source, the first one encountered must win —
    /// matching `drift_target_record_id`'s `find_map` semantics (the shared
    /// resolver `eg query drift` uses) — so the precomputed lookup here can
    /// never attach a drift to a different target than `eg query drift` does.
    #[test]
    fn symbol_context_drift_history_first_drifts_from_edge_wins_on_duplicate_source() {
        let first_target = sym("first_target");
        let first_target_id = first_target.id().to_owned();
        let second_target = sym("second_target");
        let second_target_id = second_target.id().to_owned();

        let drift_rec = drift_node("semantic:v1:dup-source-drift", &first_target_id, 0.5);
        let edge_to_first = GraphRecord::edge(
            EdgeLabel::DriftsFrom,
            "semantic:v1:dup-source-drift".to_owned(),
            first_target_id,
            Some("1.0".to_owned()),
            "first edge".to_owned(),
        );
        let edge_to_second = GraphRecord::edge(
            EdgeLabel::DriftsFrom,
            "semantic:v1:dup-source-drift".to_owned(),
            second_target_id,
            Some("1.0".to_owned()),
            "second edge".to_owned(),
        );

        let records = vec![
            first_target,
            second_target,
            drift_rec,
            edge_to_first,
            edge_to_second,
        ];

        let ctx_first = symbol_context(&records, "first_target");
        let ids: Vec<&str> = ctx_first.drift_history.iter().map(|r| r.id()).collect();
        assert_eq!(
            ids,
            vec!["semantic:v1:dup-source-drift"],
            "drift must resolve to the first DriftsFrom edge's target"
        );

        let ctx_second = symbol_context(&records, "second_target");
        assert!(
            ctx_second.drift_history.is_empty(),
            "drift must not also attach to the second edge's target"
        );
    }

    /// A retracted (tombstoned) current-state `SemanticDrift` record must not
    /// reappear as live evidence, mirroring the liveness exception every
    /// other section in this function applies and the one `subsystem.rs`'s
    /// own `semantic_drift` section already applies.
    #[test]
    fn symbol_context_drift_history_excludes_tombstoned_current_state_drift() {
        let target = sym("tombstoned_target");
        let target_id = target.id().to_owned();
        let drift_rec = drift_node("semantic:v1:tombstoned-drift", &target_id, 0.5);
        let tombstone = GraphRecord::Tombstone {
            id: "tombstone:test:1".to_owned(),
            schema_version: 1,
            deleted_id: "semantic:v1:tombstoned-drift".to_owned(),
            summary: "retracted via forget-repo".to_owned(),
            producer: None,
        };
        let records = vec![target, drift_rec, tombstone];
        let ctx = symbol_context(&records, "tombstoned_target");
        let ids: Vec<&str> = ctx.drift_history.iter().map(|r| r.id()).collect();
        assert!(ids.is_empty(), "tombstoned drift must not surface: {ids:?}");
    }

    /// AC1: resolution falls back to `target_record_id` when no `DriftsFrom`
    /// edge is present in the slice.
    #[test]
    fn symbol_context_drift_history_falls_back_to_target_record_id_without_edge() {
        let target = sym("baz");
        let target_id = target.id().to_owned();
        let drift_rec = drift_node("semantic:v1:baz-drift", &target_id, 0.5);
        let records = vec![target, drift_rec];
        let ctx = symbol_context(&records, "baz");
        let ids: Vec<&str> = ctx.drift_history.iter().map(|r| r.id()).collect();
        assert_eq!(ids, vec!["semantic:v1:baz-drift"]);
    }

    /// Drift targeting an unrelated symbol must never bleed into this
    /// symbol's `drift_history`.
    #[test]
    fn symbol_context_drift_history_excludes_drift_targeting_other_symbols() {
        let foo = sym("foo");
        let other = sym("other");
        let other_id = other.id().to_owned();
        let unrelated_drift = drift_node("semantic:v1:unrelated", &other_id, 0.99);
        let records = vec![foo, other, unrelated_drift];
        let ctx = symbol_context(&records, "foo");
        assert!(ctx.drift_history.is_empty());
    }

    /// A node of some OTHER kind that happens to carry a `semantic_drift`
    /// payload (the field is a general optional slot on every `Node`
    /// variant, not exclusive to `NodeKind::SemanticDrift`) must never
    /// surface in `drift_history` — matching `semantic_drift()`'s own
    /// kind check, the same gate `largest_semantic_drifts`/`eg query drift`
    /// apply.
    #[test]
    fn symbol_context_drift_history_requires_semantic_drift_kind() {
        let target = sym("kind_gated_target");
        let target_id = target.id().to_owned();
        let mut mislabeled = sym("not_actually_drift");
        mislabeled = mislabeled.with_semantic_drift(drift_metadata(&target_id, 0.99));
        let records = vec![target, mislabeled];
        let ctx = symbol_context(&records, "kind_gated_target");
        assert!(
            ctx.drift_history.is_empty(),
            "a non-SemanticDrift-kind node must never appear in drift_history: {:?}",
            ctx.drift_history.iter().map(|r| r.id()).collect::<Vec<_>>()
        );
    }

    /// AC5: a `SemanticDrift` record is deterministic source-derived
    /// evidence — it must never be classified into `observations`.
    #[test]
    fn symbol_context_drift_history_never_classified_as_observation() {
        let target = sym("qux");
        let target_id = target.id().to_owned();
        let drift_rec = drift_node("semantic:v1:qux-drift", &target_id, 0.7);
        let records = vec![target, drift_rec];
        let ctx = symbol_context(&records, "qux");
        assert!(!ctx.drift_history.is_empty());
        assert!(
            !ctx.observations
                .iter()
                .any(|r| r.id() == "semantic:v1:qux-drift"),
            "drift record must never appear in observations (AC5)"
        );
    }

    /// AC3 / AC7 parity: repeated calls return byte-identical ordering.
    #[test]
    fn symbol_context_drift_history_stable_ordering_across_repeated_calls() {
        let target = sym("stable");
        let target_id = target.id().to_owned();
        let small = drift_node("semantic:v1:stable-small", &target_id, 0.3);
        let large = drift_node("semantic:v1:stable-large", &target_id, 0.8);
        let edge_small = drifts_from_edge("semantic:v1:stable-small", &target_id);
        let edge_large = drifts_from_edge("semantic:v1:stable-large", &target_id);
        let records = vec![target, small, large, edge_small, edge_large];

        let first: Vec<&str> = symbol_context(&records, "stable")
            .drift_history
            .iter()
            .map(|r| r.id())
            .collect();
        for _ in 0..5 {
            let again: Vec<&str> = symbol_context(&records, "stable")
                .drift_history
                .iter()
                .map(|r| r.id())
                .collect();
            assert_eq!(again, first, "drift_history must be byte-stable");
        }
    }

    /// Issue #497 Codex review: `record_context` (used by `eg query locate`,
    /// `eg query semantic-context`, and `error_context`, none of which render
    /// `drift_history`) must skip the drift scan entirely rather than compute
    /// and discard it — `context_from_seeds`'s `compute_drift_history` gate.
    /// `symbol_context`, over the identical record set, must still populate
    /// the section, proving the gate is scoped to `record_context` only and
    /// doesn't silently disable drift resolution everywhere.
    #[test]
    fn record_context_never_populates_drift_history() {
        let target = sym("gated");
        let target_id = target.id().to_owned();
        let drift_rec = drift_node("semantic:v1:gated-drift", &target_id, 0.6);
        let edge = drifts_from_edge("semantic:v1:gated-drift", &target_id);
        let records = vec![target, drift_rec, edge];

        let via_record_context = record_context(&records, &target_id);
        assert!(
            via_record_context.drift_history.is_empty(),
            "record_context must never populate drift_history, even when a \
             drift record targets the anchor: {:?}",
            via_record_context
                .drift_history
                .iter()
                .map(|r| r.id())
                .collect::<Vec<_>>()
        );

        let via_symbol_context = symbol_context(&records, "gated");
        assert_eq!(
            via_symbol_context
                .drift_history
                .iter()
                .map(|r| r.id())
                .collect::<Vec<_>>(),
            vec!["semantic:v1:gated-drift"],
            "symbol_context must still populate drift_history over the same records"
        );
    }
}
#[cfg(test)]
mod relay_liveness_tests {
    //! Divergence repros for the shared `is_bfs_relay_node` tombstone gate
    //! (issue #469): a ToolCall re-ingested AFTER its own tombstone must relay
    //! the context BFS again (latest-write-wins, matching the coalesced
    //! `--data-dir` read), while a tombstone with no re-ingest still deletes
    //! the relay.

    use super::*;

    fn file_node() -> GraphRecord {
        GraphRecord::node(
            "codegraph:v5:file:src/lib.rs".to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            "file src/lib.rs".to_owned(),
        )
    }

    fn symbol_node() -> GraphRecord {
        GraphRecord::node(
            "codegraph:v5:sym:foo".to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some("foo".to_owned()),
            "symbol foo".to_owned(),
        )
    }

    fn memory_node(id: &str, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            Some(id.to_owned()),
            format!("{} {id}", kind.as_str()),
        )
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v5:tomb:{deleted_id}"),
            schema_version: 5,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    /// Fixture: a Symbol + co-located File seed, a ToolCall relay with a
    /// `TOUCHED_FILE` edge to the file and a `PRODUCED_EVIDENCE` edge to a
    /// CommandRun. `revive` controls whether the ToolCall is re-ingested after
    /// its tombstone.
    fn fixture(revive: bool) -> Vec<GraphRecord> {
        let tool_id = "codegraph:v5:tool:relay";
        let run_id = "codegraph:v5:run:relay";
        let mut records = vec![
            symbol_node(),
            file_node(),
            memory_node(tool_id, NodeKind::ToolCall),
            GraphRecord::edge(
                EdgeLabel::TouchedFile,
                tool_id.to_owned(),
                "codegraph:v5:file:src/lib.rs".to_owned(),
                None,
                "tool touched file".to_owned(),
            ),
            tombstone(tool_id),
        ];
        if revive {
            records.push(memory_node(tool_id, NodeKind::ToolCall));
        }
        records.push(memory_node(run_id, NodeKind::CommandRun));
        records.push(GraphRecord::edge(
            EdgeLabel::ProducedEvidence,
            tool_id.to_owned(),
            run_id.to_owned(),
            None,
            "tool call produced run".to_owned(),
        ));
        records
    }

    #[test]
    fn revived_toolcall_relays_context_bfs_to_command_run() {
        // Divergence repro (issue #469): over `--graph` the revived ToolCall is
        // tombstone-gated out of the BFS relay, so the CommandRun it produced
        // is never reached — while `--data-dir` (embedded latest-write-wins)
        // treats the relay as live and surfaces the run.
        let records = fixture(true);
        let ctx = symbol_context(&records, "foo");
        assert!(
            ctx.verification_evidence
                .iter()
                .any(|r| r.id() == "codegraph:v5:run:relay"),
            "a ToolCall revived after its tombstone must relay the context BFS to its CommandRun"
        );
    }

    #[test]
    fn tombstoned_toolcall_without_reingest_does_not_relay() {
        // Negative control: a tombstone with no later re-ingest still deletes
        // the relay — the conversion must not turn every tombstoned relay live.
        let records = fixture(false);
        let ctx = symbol_context(&records, "foo");
        assert!(
            !ctx.verification_evidence
                .iter()
                .any(|r| r.id() == "codegraph:v5:run:relay"),
            "a tombstoned ToolCall with no re-ingest must not relay the context BFS"
        );
    }
}

#[cfg(test)]
mod decision_section_tests {
    //! Issue #191: `Decision` records must surface in a dedicated `decisions`
    //! section — never flattened into `observations`.
    //!
    //! The section exists: `SymbolContext::decisions` carries `Decision`
    //! records, `SymbolContext::observations` never does.
    use super::*;
    use crate::ir::{AGENT_MEMORY_SCHEMA_VERSION, EvidenceLink};
    use crate::query::{TrustIndex, context_decision};

    const DEC_ID: &str = "agent_memory:v1:dec-191";
    const OBS_ID: &str = "agent_memory:v1:obs-191";
    const FAIL_ID: &str = "agent_memory:v1:fail-191";

    fn sym(name: &str) -> GraphRecord {
        GraphRecord::node(
            format!("codegraph:v5:sym-{name}"),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn evidence_link(target: &str, relation: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: relation.to_owned(),
            confidence: "1.0".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    fn agent_node(id: &str, kind: NodeKind, symbol_id: &str, relation: &str) -> GraphRecord {
        let mut node = GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("{kind:?} summary"),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
        if let GraphRecord::Node {
            agent_id,
            session_id,
            observed_at,
            evidence_links,
            ..
        } = &mut node
        {
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-1".to_owned());
            *observed_at = Some("2026-01-01T00:00:00Z".to_owned());
            *evidence_links = Some(vec![evidence_link(symbol_id, relation)]);
        }
        node
    }

    fn fixture() -> Vec<GraphRecord> {
        let target = sym("foo");
        let target_id = target.id().to_owned();
        vec![
            target,
            agent_node(DEC_ID, NodeKind::Decision, &target_id, "OBSERVES"),
            agent_node(OBS_ID, NodeKind::Observation, &target_id, "OBSERVES"),
            agent_node(FAIL_ID, NodeKind::Failure, &target_id, "FAILED_ON"),
        ]
    }

    #[test]
    fn classify_node_separates_decision_from_observation() {
        // RED: today Decision flattens into the Observation section.
        assert_ne!(
            classify_node(NodeKind::Decision),
            classify_node(NodeKind::Observation),
            "Decision must not share the Observation section (issue #191)"
        );
    }

    #[test]
    fn classify_node_keeps_failure_in_observations() {
        // Section contract (issue #191): Failure stays in observations —
        // documented, not silently flattened. Guards the fix against moving
        // failures along with decisions.
        assert_eq!(
            classify_node(NodeKind::Failure),
            Some(ContextSection::Observation)
        );
        assert_eq!(
            classify_node(NodeKind::Observation),
            Some(ContextSection::Observation)
        );
    }

    #[test]
    fn decision_record_never_surfaces_inside_observations() {
        let records = fixture();
        let ctx = symbol_context(&records, "foo");
        let obs_ids: Vec<&str> = ctx.observations.iter().map(|r| r.id()).collect();
        assert!(
            !obs_ids.contains(&DEC_ID),
            "Decision {DEC_ID} must not appear in observations: {obs_ids:?}"
        );
    }

    #[test]
    fn decision_record_surfaces_in_dedicated_decisions_section() {
        // The dedicated-section half of the contract: the Decision record is
        // not merely kept out of `observations` — it must appear in
        // `decisions` (issue #191 AC1/AC3).
        let records = fixture();
        let ctx = symbol_context(&records, "foo");
        let dec_ids: Vec<&str> = ctx.decisions.iter().map(|r| r.id()).collect();
        assert!(
            dec_ids.contains(&DEC_ID),
            "Decision {DEC_ID} must surface in the decisions section: {dec_ids:?}"
        );
        assert!(
            !ctx.is_no_match(),
            "a symbol with linked decisions is not a no-match"
        );
    }

    #[test]
    fn failure_records_stay_in_observations_per_section_contract() {
        // Companion to the classifier contract: the fix must not move
        // failures out of observations.
        let records = fixture();
        let ctx = symbol_context(&records, "foo");
        let obs_ids: Vec<&str> = ctx.observations.iter().map(|r| r.id()).collect();
        assert!(
            obs_ids.contains(&FAIL_ID),
            "Failure records stay in observations per the section contract: {obs_ids:?}"
        );
        assert!(
            obs_ids.contains(&OBS_ID),
            "Observation records stay in observations: {obs_ids:?}"
        );
        assert!(!ctx.is_no_match());
    }

    #[test]
    fn decision_row_retains_text_rationale_and_resolved_evidence() {
        // Issue #191 AC2/AC6: the dedicated section must carry the decision
        // text, the non-empty rationale, provenance, confidence, and the
        // resolved `EXPLAINS_CHANGE` evidence handle.
        let mut decision = GraphRecord::node(
            DEC_ID.to_owned(),
            NodeKind::Decision,
            None,
            None,
            None,
            "Decision summary".to_owned(),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION)
        .with_decision_text("decide to refactor foo")
        .with_rationale_summary("the symbol is duplicated in three places");
        if let GraphRecord::Node {
            agent_id,
            session_id,
            confidence,
            evidence_links,
            ..
        } = &mut decision
        {
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some("sess-1".to_owned());
            *confidence = Some("0.9".to_owned());
            *evidence_links = Some(vec![EvidenceLink {
                target_record_id: Some("codegraph:v5:sym-foo".to_owned()),
                target_domain: "codegraph".to_owned(),
                relation: "EXPLAINS_CHANGE".to_owned(),
                confidence: "0.9".to_owned(),
                as_of_commit: None,
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            }]);
        }
        let records = vec![sym("foo"), decision];
        let trust = TrustIndex::build(&records);
        let record = records.iter().find(|r| r.id() == DEC_ID).unwrap();
        let row = context_decision(record, &records, &trust)
            .expect("Decision record must build a decision row");

        assert_eq!(row.decision_text, Some("decide to refactor foo"));
        assert!(
            row.rationale_summary.is_some_and(|r| !r.is_empty()),
            "rationale_summary must be non-empty (issue #191 success metric)"
        );
        assert_eq!(row.provenance_handle.as_deref(), Some("agent-1:sess-1"));
        assert_eq!(row.confidence, Some("0.9"));
        assert_eq!(row.record_id, DEC_ID);
        assert_eq!(row.evidence_handles.len(), 1);
        assert_eq!(row.evidence_handles[0].relation, "EXPLAINS_CHANGE");
        assert_eq!(
            row.evidence_handles[0].target_record_id,
            "codegraph:v5:sym-foo"
        );
        assert_eq!(row.evidence_handles[0].target_kind, "Symbol");
    }

    #[test]
    fn decision_row_provenance_falls_back_to_agent_id() {
        // Provenance handle is `agent_id:session_id`, falling back to
        // `agent_id` when no session is recorded.
        let mut decision = GraphRecord::node(
            DEC_ID.to_owned(),
            NodeKind::Decision,
            None,
            None,
            None,
            "Decision summary".to_owned(),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION)
        .with_rationale_summary("some reason");
        if let GraphRecord::Node { agent_id, .. } = &mut decision {
            *agent_id = Some("agent-9".to_owned());
        }
        let records = vec![decision];
        let trust = TrustIndex::build(&records);
        let row = context_decision(&records[0], &records, &trust).unwrap();
        assert_eq!(row.provenance_handle.as_deref(), Some("agent-9"));
    }

    #[test]
    fn decision_row_serializes_with_expected_fields() {
        // The row shape the CLI/MCP/daemon lanes share: every field the
        // issue requires must be present on the wire.
        let mut decision = GraphRecord::node(
            DEC_ID.to_owned(),
            NodeKind::Decision,
            None,
            None,
            None,
            "Decision summary".to_owned(),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION)
        .with_decision_text("decide X")
        .with_rationale_summary("because Y");
        if let GraphRecord::Node { agent_id, .. } = &mut decision {
            *agent_id = Some("agent-1".to_owned());
        }
        let records = vec![decision];
        let trust = TrustIndex::build(&records);
        let row = context_decision(&records[0], &records, &trust).unwrap();
        let value = serde_json::to_value(&row).unwrap();
        // Always-serialized keys (the MCP contract requires them).
        for field in [
            "record_id",
            "kind",
            "trust",
            "summary",
            "decision_text",
            "rationale_summary",
        ] {
            assert!(
                value.get(field).is_some(),
                "decision row must serialize `{field}`"
            );
        }
        // Present here because the fixture sets them; skipped when absent.
        assert_eq!(value["provenance_handle"], "agent-1");
        assert_eq!(value["rationale_summary"], "because Y");
    }
}
