use std::collections::{BTreeMap, BTreeSet};

use super::RepositoryIndex;
use super::liveness::Liveness;
use super::unreferenced::UNREFERENCED_REFERENCE_LABELS;
use crate::ir::{Domain, EdgeLabel, GraphRecord, NodeKind, SourceSpan};

// ---------------------------------------------------------------------------
// blind-spot triage lane (issue #265)
// ---------------------------------------------------------------------------

/// Edge labels counted as cross-domain **evidence** for a code target
/// (issue #265).
///
/// These are exactly the labels the schema registry
/// (`docs/schema/agent-memory.md` §6a) documents as pointing FROM the
/// agent-memory, verification, or project domains INTO the codegraph domain
/// at a `Symbol` or `File` target:
///
/// - `OBSERVES` — an agent-memory `Observation` observes a code entity.
/// - `MENTIONS_SYMBOL` — an agent-memory / verification / project node names
///   a specific symbol.
/// - `TOUCHED_FILE` — an agent-memory / verification node (`FileEdit`,
///   `ToolCall`, `CommandRun`, `TestRun`, `CIStatus`) cites a file it
///   touched.
/// - `FAILED_ON` — an agent-memory / verification node (`Failure`, `TestRun`,
///   `CIStatus`) records a failure on a symbol or file.
/// - `TOUCHES_FILE` — a project `Task` (or `Review`) intends to touch a
///   code-graph file.
///
/// The lane additionally requires the edge's **source node** to classify
/// into the label's documented FROM domain(s) (see
/// [`evidence_label_source_domains`]): a label alone is not trusted, so a
/// malformed code-internal edge carrying a cross-domain label can never
/// fake evidence for a target.
///
/// Deliberately excluded: the structural reference classes
/// ([`UNREFERENCED_REFERENCE_LABELS`]) — being called is not knowledge;
/// `EXPLAINS_CHANGE` — it targets `Commit`/`Change` nodes, never `Symbol` or
/// `File`; `HAS_EVIDENCE` / `VALIDATED_BY` — they terminate at verification
/// sinks, not at code targets; and the generic `RELATES_TO` / `CONTRADICTS` /
/// `SUPERSEDES` link labels — they are permitted cross-domain links but are
/// not *evidence* in the documented sense, and counting them would let this
/// lane's meaning drift.
pub const BLIND_SPOT_EVIDENCE_LABELS: &[EdgeLabel] = &[
    EdgeLabel::Observes,
    EdgeLabel::MentionsSymbol,
    EdgeLabel::TouchedFile,
    EdgeLabel::FailedOn,
    EdgeLabel::TouchesFile,
];

/// Stable wire strings for [`BLIND_SPOT_EVIDENCE_LABELS`], sorted.
pub const BLIND_SPOT_EVIDENCE_CLASS_NAMES: &[&str] = &[
    "FAILED_ON",
    "MENTIONS_SYMBOL",
    "OBSERVES",
    "TOUCHED_FILE",
    "TOUCHES_FILE",
];

/// Which code-target populations the blind-spot lane evaluates.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum BlindSpotKind {
    /// Only `Symbol` records.
    Symbol,
    /// Only `File` records.
    File,
    /// Both symbols and files.
    Both,
}

/// The documented FROM domain(s) for each evidence edge label, taken from
/// the cross-domain edge registry (`docs/schema/agent-memory.md` §6a).
///
/// A [`BLIND_SPOT_EVIDENCE_LABELS`] edge counts as evidence for its code
/// target only when its source node's [`domain_of_kind`] is one of these.
const fn evidence_label_source_domains(label: EdgeLabel) -> &'static [Domain] {
    match label {
        EdgeLabel::Observes => &[Domain::AgentMemory],
        EdgeLabel::MentionsSymbol => &[Domain::AgentMemory, Domain::Verification, Domain::Project],
        EdgeLabel::TouchedFile | EdgeLabel::FailedOn => {
            &[Domain::AgentMemory, Domain::Verification]
        }
        EdgeLabel::TouchesFile => &[Domain::Project],
        _ => &[],
    }
}

/// Classifies a [`NodeKind`] into its owning [`Domain`], following the
/// domain annotations on the `NodeKind` variants in `crate::ir`.
///
/// [`NodeKind::Retraction`] is operator-authored like the user-context
/// records, so it classifies there; it never qualifies as an evidence
/// source regardless, since no evidence label admits the user-context
/// domain.
const fn domain_of_kind(kind: NodeKind) -> Domain {
    match kind {
        NodeKind::Repository
        | NodeKind::File
        | NodeKind::Module
        | NodeKind::Symbol
        | NodeKind::Import
        | NodeKind::Diagnostic
        | NodeKind::PanicRiskSite
        | NodeKind::DebtMarker
        | NodeKind::UnsafeSite
        | NodeKind::DependencyDeclaration
        | NodeKind::ScanCoverage
        | NodeKind::Commit
        | NodeKind::Change => Domain::CodeGraph,
        NodeKind::SemanticDrift | NodeKind::EmbeddingModel | NodeKind::EmbeddingVector => {
            Domain::Semantic
        }
        NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::Observation
        | NodeKind::AgentRun
        | NodeKind::AgentTurn
        | NodeKind::ToolCall
        | NodeKind::CommandRun
        | NodeKind::FileEdit
        | NodeKind::PatchArtifact
        | NodeKind::Failure
        | NodeKind::Decision
        | NodeKind::CostUsage => Domain::AgentMemory,
        NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::TestRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult => Domain::Verification,
        NodeKind::Artifact => Domain::Artifact,
        NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::ExternalLink
        | NodeKind::Product
        | NodeKind::Project
        | NodeKind::Plan
        | NodeKind::GitHubIssue
        | NodeKind::PR
        | NodeKind::Review
        | NodeKind::ExternalIdentity
        | NodeKind::ReviewStateTransition
        | NodeKind::LocalTask => Domain::Project,
        NodeKind::PromoteCandidate
        | NodeKind::PromotionPrompt
        | NodeKind::PromotionDecision
        | NodeKind::Preference
        | NodeKind::WorkflowRule
        | NodeKind::NamingDecision
        | NodeKind::Constraint
        | NodeKind::Retraction => Domain::UserContext,
        NodeKind::LogSource
        | NodeKind::ErrorSignature
        | NodeKind::LogEvent
        | NodeKind::LogOccurrenceBucket => Domain::Log,
    }
}

/// One zero-evidence code target.
///
/// Every row is a TRIAGE LEAD — "no recorded cross-domain evidence" is not
/// evidence the code is unimportant, untested, or safe. See [`blind_spots`]
/// for the evidence definition and its documented non-inheritances.
#[derive(Debug, Clone)]
pub struct BlindSpot<'a> {
    /// Stable record ID of the `Symbol` / `File` node.
    pub record_id: &'a str,
    /// Record schema version.
    pub schema_version: u32,
    /// Whether the row is a symbol or a file.
    pub target_kind: BlindSpotKind,
    /// Symbol name (qualified where the extractor qualifies it); for file
    /// rows, the repo-relative path.
    pub name: &'a str,
    /// Language-specific symbol kind (`function`, `struct`, …), or `file`.
    pub kind: &'a str,
    /// Repo-relative file of the target (the file itself for file rows).
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration; `None` for file rows.
    pub span: Option<SourceSpan>,
    /// Introducing commit for temporal (history-backed) records.
    pub git_commit: Option<&'a str>,
    /// Inbound structural reference count — the prominence ranking key.
    /// Counts inbound edges of the [`UNREFERENCED_REFERENCE_LABELS`]
    /// classes (callers / mentions); files typically carry 0, since those
    /// edges target symbols.
    pub inbound_reference_count: usize,
}

/// Deterministic tallies for the blind-spot result.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct BlindSpotCounts {
    /// Live, in-scope `Symbol` / `File` records evaluated (impl blocks
    /// excluded, per the `--kind` filter).
    pub targets_evaluated: usize,
    /// Evaluated targets carrying at least one qualifying evidence edge.
    pub targets_with_evidence: usize,
    /// Evaluated targets with zero qualifying evidence edges.
    pub blind_spots: usize,
}

/// A stable machine-readable condition attached to the result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct BlindSpotDiagnostic {
    /// Stable diagnostic code (`no_targets`, `no_blind_spots`,
    /// `unattributed_evidence_edges_present`).
    pub code: &'static str,
    /// Record the diagnostic is about, when one exists.
    pub record_id: Option<String>,
    /// Bounded human-readable detail (counts only — never payload).
    pub detail: String,
}

/// The blind-spot set plus tallies and diagnostics.
#[derive(Debug, Clone, Default)]
pub struct BlindSpots<'a> {
    /// Blind spots sorted by (`inbound_reference_count` descending,
    /// `repo_relative_path`, `span.start_line`, `record_id`) — the
    /// documented deterministic ordering.
    pub blind_spots: Vec<BlindSpot<'a>>,
    /// Deterministic tallies.
    pub counts: BlindSpotCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<BlindSpotDiagnostic>,
}

/// Selects code targets with **zero cross-domain evidence edges** as
/// blind-spot triage leads (issue #265).
///
/// A live, in-scope `Symbol` (impl blocks excluded) or `File` record is a
/// blind spot when no live, current-state edge with a label in
/// [`BLIND_SPOT_EVIDENCE_LABELS`] targets it from a node whose kind
/// classifies into the label's documented FROM domain(s) — i.e. no linked
/// agent-memory observation, no linked verification record, no linked
/// project task.
///
/// Evidence does **not** inherit: a symbol evidenced through
/// `MENTIONS_SYMBOL` does not evidence its file, and a file evidenced
/// through `TOUCHED_FILE` does not evidence the symbols it contains. Each
/// target stands on its own edges.
///
/// Current-state view: tombstoned targets and tombstoned evidence sources
/// are excluded, and when a stable ID appears more than once (history
/// graphs) the latest record wins deterministically. A stale evidence edge —
/// replayed from an older commit and absent at its repository's stamped
/// HEAD — does not disqualify its target. Latest-write-wins applies to edge
/// metadata, mirroring the embedded current-state read (issues #421/#432).
/// An evidence edge whose source node record is absent from the store is
/// not counted (it cannot be attributed to a domain) and raises the
/// `unattributed_evidence_edges_present` diagnostic instead of silently
/// hiding a blind spot.
///
/// Rows are ranked riskiest-first: `inbound_reference_count` (inbound edges
/// of the [`UNREFERENCED_REFERENCE_LABELS`] structural classes —
/// callers/mentions) descending, then (`repo_relative_path`,
/// `span.start_line`, `record_id`). A widely-referenced symbol nobody has
/// ever recorded anything about outranks an isolated one.
///
/// Deterministic: output ordering depends only on record content, never on
/// map iteration or wall-clock time. Strictly read-only.
#[must_use]
pub fn blind_spots<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    kind: BlindSpotKind,
) -> BlindSpots<'a> {
    // Latest-write-wins liveness (issues #421/#432), same contract as the
    // unreferenced lane: over an append-only `--graph` a node OR edge
    // re-ingested AFTER its own tombstone is live again, matching the
    // embedded `--data-dir` current-state read.
    let liveness = Liveness::new(records);
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| {
            if let GraphRecord::Tombstone { deleted_id, .. } = r {
                Some(deleted_id.as_str())
            } else {
                None
            }
        })
        .filter(|&id| liveness.deleted(id))
        .collect();
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // Default current-state view: the shared head-anchor gate (issue #427),
    // as in the unreferenced lane. A symbol/edge absent at HEAD does not
    // shape the answer; snapshot-less stores keep every recorded edge.
    let non_head_current = super::non_head_current_record_ids(records, index);

    // Target population: live, in-scope Symbol (non-impl) and File nodes,
    // keep-last dedupe by stable ID so history graphs resolve to their
    // newest version. Files additionally carry their own row (evidence does
    // not inherit between a file and its symbols).
    let mut targets: BTreeMap<&str, (&'a GraphRecord, BlindSpotKind)> = BTreeMap::new();
    // Live node kinds for evidence-source domain attribution. A tombstoned
    // source is retracted: its edges carry no evidence.
    let mut node_kinds: BTreeMap<&str, NodeKind> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind: node_kind,
            symbol_kind,
            ..
        } = record
        else {
            continue;
        };
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        // Latest node version's kind wins (kinds do not change across
        // re-ingests, but latest-write-wins keeps the contract uniform).
        node_kinds.insert(id.as_str(), *node_kind);
        let target_kind = match node_kind {
            NodeKind::Symbol => {
                if !matches!(kind, BlindSpotKind::Symbol | BlindSpotKind::Both) {
                    continue;
                }
                // impl blocks are unnameable declaration details, never
                // triage targets; their methods are considered directly.
                if symbol_kind.as_deref() == Some("impl") {
                    continue;
                }
                BlindSpotKind::Symbol
            }
            NodeKind::File => {
                if !matches!(kind, BlindSpotKind::File | BlindSpotKind::Both) {
                    continue;
                }
                BlindSpotKind::File
            }
            _ => continue,
        };
        if !is_owned(id) || non_head_current.contains(id.as_str()) {
            continue;
        }
        let replace = targets.get(id.as_str()).is_none_or(|(existing, _)| {
            super::file_at_point::version_recency_key(record)
                >= super::file_at_point::version_recency_key(existing)
        });
        if replace {
            targets.insert(id.as_str(), (record, target_kind));
        }
    }

    // Evidence pass: an edge counts when it is live and current, carries an
    // evidence label, targets a population member, and its source node is a
    // live record whose kind classifies into the label's documented FROM
    // domain(s).
    let mut evidenced: BTreeSet<&str> = BTreeSet::new();
    let mut unattributed_evidence_edges = 0usize;
    // Prominence pass: inbound structural reference counts per target.
    let mut inbound_references: BTreeMap<&str, usize> = BTreeMap::new();
    for (index, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        // Latest-write-wins for edge metadata (#421/#432): only the latest
        // EDGE write for the id is live.
        if !liveness.is_latest_edge_version(id.as_str(), index) {
            continue;
        }
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if non_head_current.contains(id.as_str()) {
            continue;
        }
        if BLIND_SPOT_EVIDENCE_LABELS.contains(label) {
            if targets.contains_key(target.as_str()) {
                match node_kinds.get(source.as_str()) {
                    Some(source_kind)
                        if evidence_label_source_domains(*label)
                            .contains(&domain_of_kind(*source_kind)) =>
                    {
                        evidenced.insert(target.as_str());
                    }
                    _ => {
                        // The source record is absent, tombstoned, or its
                        // kind does not belong to the label's documented FROM
                        // domain: the edge cannot be attributed to cross-domain
                        // evidence. Count it for the honesty diagnostic rather
                        // than silently ignoring it.
                        unattributed_evidence_edges += 1;
                    }
                }
            }
        } else if UNREFERENCED_REFERENCE_LABELS.contains(label)
            && targets.contains_key(target.as_str())
        {
            *inbound_references.entry(target.as_str()).or_default() += 1;
        }
    }

    let mut result = BlindSpots::default();
    result.counts.targets_evaluated = targets.len();
    result.counts.targets_with_evidence = evidenced.len();

    for (id, (record, target_kind)) in &targets {
        if evidenced.contains(id) {
            continue;
        }
        let GraphRecord::Node {
            schema_version,
            repo_relative_path,
            span,
            name,
            symbol_kind,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        let (row_name, row_kind) = match target_kind {
            BlindSpotKind::Symbol => {
                let Some(name) = name.as_deref() else {
                    continue;
                };
                (name, symbol_kind.as_deref().unwrap_or("symbol"))
            }
            BlindSpotKind::File => {
                let Some(path) = repo_relative_path.as_deref() else {
                    continue;
                };
                (path, "file")
            }
            BlindSpotKind::Both => continue,
        };
        result.blind_spots.push(BlindSpot {
            record_id: id,
            schema_version: *schema_version,
            target_kind: *target_kind,
            name: row_name,
            kind: row_kind,
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
            git_commit: temporal.as_ref().map(|t| t.git_commit.as_str()),
            inbound_reference_count: inbound_references.get(id).copied().unwrap_or(0),
        });
    }
    result.counts.blind_spots = result.blind_spots.len();

    // Documented deterministic ordering: riskiest first.
    result.blind_spots.sort_by(|a, b| {
        b.inbound_reference_count
            .cmp(&a.inbound_reference_count)
            .then_with(|| a.repo_relative_path.cmp(&b.repo_relative_path))
            .then_with(|| {
                a.span
                    .map(|s| s.start_line)
                    .cmp(&b.span.map(|s| s.start_line))
            })
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    if result.counts.targets_evaluated == 0 {
        result.diagnostics.push(BlindSpotDiagnostic {
            code: "no_targets",
            record_id: None,
            detail: "graph contains no live code Symbol/File records in scope; there is \
                     nothing to triage (the store may predate code extraction or the \
                     repository scope excludes every target)"
                .to_owned(),
        });
    } else if result.blind_spots.is_empty() {
        result.diagnostics.push(BlindSpotDiagnostic {
            code: "no_blind_spots",
            record_id: None,
            detail: "every evaluated target carries at least one qualifying \
                     cross-domain evidence edge; no blind spots"
                .to_owned(),
        });
    }
    if unattributed_evidence_edges > 0 {
        result.diagnostics.push(BlindSpotDiagnostic {
            code: "unattributed_evidence_edges_present",
            record_id: None,
            detail: format!(
                "{unattributed_evidence_edges} evidence-labeled edge(s) could not be \
                 attributed to a source node in a qualifying domain (absent or \
                 tombstoned source, or a kind outside the label's documented FROM \
                 domain); they were not counted as evidence, so a listed target may \
                 still have an unrecorded link"
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

#[cfg(test)]
mod blind_spot_tests {
    //! Unit tests for the evidence definition and ranking contract
    //! (issue #265). The CLI-level acceptance criteria live in
    //! `tests/integration/query_blind_spots.rs`.
    use super::*;
    use crate::ir::SCHEMA_VERSION;

    fn sym(id: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
                start_column: None,
                end_column: None,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn file(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            None,
            format!("file {path}"),
        )
    }

    fn mem_node(id: &str, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("memory {kind:?}"),
        )
    }

    fn evidence(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            None,
            "evidence edge".to_owned(),
        )
    }

    fn references(source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Calls,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            "calls".to_owned(),
        )
    }

    fn tomb(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v6:tomb_{deleted_id}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    fn names<'a>(result: &'a BlindSpots<'a>) -> Vec<&'a str> {
        result.blind_spots.iter().map(|b| b.name).collect()
    }

    #[test]
    fn zero_evidence_target_is_a_blind_spot() {
        let records = vec![sym("codegraph:v6:lonely", "lonely")];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert_eq!(names(&result), vec!["lonely"]);
        assert_eq!(result.counts.targets_evaluated, 1);
        assert_eq!(result.counts.targets_with_evidence, 0);
        assert_eq!(result.counts.blind_spots, 1);
    }

    #[test]
    fn linked_observation_removes_symbol_from_blind_spots() {
        let records = vec![
            sym("codegraph:v6:noted", "noted"),
            mem_node("agent_memory:v1:obs", NodeKind::Observation),
            evidence(
                EdgeLabel::MentionsSymbol,
                "agent_memory:v1:obs",
                "codegraph:v6:noted",
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert!(names(&result).is_empty());
        assert_eq!(result.counts.targets_with_evidence, 1);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "no_blind_spots")
        );
    }

    #[test]
    fn linked_verification_record_removes_file_from_blind_spots() {
        let records = vec![
            file("codegraph:v6:file", "src/lib.rs"),
            mem_node("verification:v1:run", NodeKind::TestRun),
            evidence(
                EdgeLabel::TouchedFile,
                "verification:v1:run",
                "codegraph:v6:file",
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert!(names(&result).is_empty());
    }

    #[test]
    fn linked_task_removes_symbol_from_blind_spots() {
        let records = vec![
            sym("codegraph:v6:planned", "planned"),
            mem_node("project:v1:task", NodeKind::Task),
            evidence(
                EdgeLabel::MentionsSymbol,
                "project:v1:task",
                "codegraph:v6:planned",
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert!(names(&result).is_empty());
    }

    #[test]
    fn evidence_does_not_inherit_between_file_and_symbols() {
        // A TOUCHED_FILE edge evidences the file, not the symbols inside it;
        // a MENTIONS_SYMBOL edge evidences the symbol, not its file.
        let records = vec![
            sym("codegraph:v6:s", "s"),
            file("codegraph:v6:f", "src/lib.rs"),
            mem_node("verification:v1:run", NodeKind::TestRun),
            mem_node("agent_memory:v1:obs", NodeKind::Observation),
            evidence(
                EdgeLabel::TouchedFile,
                "verification:v1:run",
                "codegraph:v6:f",
            ),
            evidence(
                EdgeLabel::MentionsSymbol,
                "agent_memory:v1:obs",
                "codegraph:v6:s",
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert!(
            names(&result).is_empty(),
            "both targets are evidenced directly; nothing here tests inheritance"
        );

        // Now evidence only the symbol: the file must stay a blind spot.
        let records = vec![
            sym("codegraph:v6:s", "s"),
            file("codegraph:v6:f", "src/lib.rs"),
            mem_node("agent_memory:v1:obs", NodeKind::Observation),
            evidence(
                EdgeLabel::MentionsSymbol,
                "agent_memory:v1:obs",
                "codegraph:v6:s",
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert_eq!(names(&result), vec!["src/lib.rs"]);
    }

    #[test]
    fn code_domain_source_does_not_count_as_evidence() {
        // Same label, wrong source domain: a codegraph Symbol emitting
        // OBSERVES is not cross-domain evidence.
        let records = vec![
            sym("codegraph:v6:a", "a"),
            sym("codegraph:v6:b", "b"),
            evidence(EdgeLabel::Observes, "codegraph:v6:a", "codegraph:v6:b"),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert_eq!(names(&result), vec!["a", "b"]);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "unattributed_evidence_edges_present")
        );
    }

    #[test]
    fn structural_reference_edges_are_not_evidence_but_drive_prominence() {
        let records = vec![
            sym("codegraph:v6:caller", "caller"),
            sym("codegraph:v6:callee", "callee"),
            sym("codegraph:v6:alone", "alone"),
            references("codegraph:v6:caller", "codegraph:v6:callee"),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        // All three lack evidence; callee (1 inbound reference) ranks first.
        assert_eq!(names(&result), vec!["callee", "alone", "caller"]);
        assert_eq!(result.blind_spots[0].inbound_reference_count, 1);
        assert_eq!(result.blind_spots[1].inbound_reference_count, 0);
    }

    #[test]
    fn tombstoned_evidence_source_does_not_hide_a_blind_spot() {
        let records = vec![
            sym("codegraph:v6:noted", "noted"),
            mem_node("agent_memory:v1:obs", NodeKind::Observation),
            evidence(
                EdgeLabel::MentionsSymbol,
                "agent_memory:v1:obs",
                "codegraph:v6:noted",
            ),
            tomb("agent_memory:v1:obs"),
        ];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert_eq!(
            names(&result),
            vec!["noted"],
            "a retracted (tombstoned) observation carries no evidence"
        );
    }

    #[test]
    fn tombstoned_target_is_not_a_blind_spot() {
        let records = vec![sym("codegraph:v6:gone", "gone"), tomb("codegraph:v6:gone")];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert!(names(&result).is_empty());
        assert!(result.diagnostics.iter().any(|d| d.code == "no_targets"));
    }

    #[test]
    fn kind_filter_restricts_the_population() {
        let records = vec![
            sym("codegraph:v6:s", "s"),
            file("codegraph:v6:f", "src/lib.rs"),
        ];
        let index = RepositoryIndex::build(&records);
        let symbols = blind_spots(&records, &index, None, BlindSpotKind::Symbol);
        assert_eq!(names(&symbols), vec!["s"]);
        let files = blind_spots(&records, &index, None, BlindSpotKind::File);
        assert_eq!(names(&files), vec!["src/lib.rs"]);
        let both = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert_eq!(both.counts.targets_evaluated, 2);
    }

    #[test]
    fn impl_block_symbols_are_not_targets() {
        let mut record = sym("codegraph:v6:impl", "Target::method");
        if let GraphRecord::Node { symbol_kind, .. } = &mut record {
            *symbol_kind = Some("impl".to_owned());
        }
        let records = vec![record];
        let index = RepositoryIndex::build(&records);
        let result = blind_spots(&records, &index, None, BlindSpotKind::Both);
        assert!(names(&result).is_empty());
    }

    #[test]
    fn evidence_edge_classes_are_sorted_and_complete() {
        let mut classes = BLIND_SPOT_EVIDENCE_CLASS_NAMES.to_vec();
        classes.sort_unstable();
        assert_eq!(classes, BLIND_SPOT_EVIDENCE_CLASS_NAMES);
        assert_eq!(BLIND_SPOT_EVIDENCE_LABELS.len(), classes.len());
    }
}
