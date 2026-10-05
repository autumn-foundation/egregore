//! Agent-belief timeline for one code target (issue #235).
//!
//! [`resolve_belief_target`] turns a code-target handle (a symbol record ID,
//! a repo-relative `path` / `path:line`, or a symbol name resolved through
//! the existing identity rules shared with `eg query lifeline`) into one
//! stable code record. [`belief_timeline`] then returns every live,
//! agent-authored observation-class record (`Observation`, `Decision`,
//! `Failure`) whose evidence links cite that target, ordered oldest→newest
//! by `observed_at` (falling back to `ingested_at`, record ID tiebreaker),
//! each carrying a machine-readable status (`current`, `superseded`, or
//! `contradicted` — the vocabulary is closed), the forward record that
//! overrode or disputed it, and citation provenance. A supersession cycle
//! does not add a fourth status: each cycle member already carries a live
//! supersession claim against it, so it reads `superseded` with its
//! immediate forward record, and the cycle surfaces as a stable
//! `supersession_cycle` diagnostic instead of hanging or truncating
//! silently.
//!
//! Trust separation: statuses derive solely from author-written
//! `superseded_by` / `SUPERSEDES` / `CONTRADICTS` data already in the store,
//! resolved through the shared [`TemporalResolver`]. The lane computes no new
//! judgment, never rewrites or hides a record, and emits no record bodies —
//! only IDs, handles, timestamps, confidence, statuses, and relations.

use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use crate::ir::{GraphRecord, NodeKind};
use crate::temporal_status::TemporalResolver;

use super::RepositoryIndex;
use super::liveness::Liveness;

/// Observation-class node kinds covered by the belief timeline.
///
/// `Hypothesis` and `Lesson` have no `NodeKind` variants in the agent-memory
/// v1 schema, so this is the complete set of agent-authored
/// observation-class records the store can hold.
const OBSERVATION_CLASS: [NodeKind; 3] =
    [NodeKind::Observation, NodeKind::Decision, NodeKind::Failure];

/// The documented ordering rule, stated in the output rather than implicit.
pub const BELIEF_TIMELINE_ORDERING: &str = "observed_at, falling back to ingested_at, \
    with record ID as the stable tiebreaker (oldest to newest); \
    records with no usable timestamp sort last";

/// The kind of a resolved code target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum BeliefTargetKind {
    /// A `Symbol` node.
    Symbol,
    /// A `File` node.
    File,
}

/// One resolved code target: the anchor of a belief timeline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedBeliefTarget {
    /// Stable record ID of the target code record.
    pub record_id: String,
    /// Whether the target is a symbol or a file.
    pub kind: BeliefTargetKind,
    /// Citable handle: `path#name` for symbols, `path` for files.
    pub target_handle: String,
}

/// How a code-target handle failed to resolve. The CLI maps each variant to a
/// stable diagnostic code and exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeliefTargetError {
    /// The handle is empty or otherwise not a supported shape (exit 1).
    Unsupported {
        /// The handle as supplied.
        handle: String,
        /// What was wrong with it.
        message: String,
    },
    /// Well-formed handle matched no live code target (exit 2).
    Unknown {
        /// The handle as supplied.
        handle: String,
    },
    /// The handle matched only tombstoned (retracted) records (exit 2).
    Stale {
        /// The handle as supplied.
        handle: String,
    },
    /// The handle matched several live targets (exit 6).
    Ambiguous {
        /// The handle as supplied.
        handle: String,
        /// Sorted candidate record IDs.
        candidates: Vec<String>,
    },
    /// `path:line` resolved the file but no symbol encloses the line (exit 2).
    NoSymbolAtLine {
        /// The handle as supplied.
        handle: String,
        /// The resolved file path.
        path: String,
        /// The requested 1-based line number.
        line: usize,
    },
}

/// Machine-readable belief status for one timeline entry.
///
/// The vocabulary is closed: every entry is `current`, `superseded`, or
/// `contradicted`. A supersession cycle does not add a fourth status —
/// each cycle member already carries a live supersession claim against it,
/// so it reads `superseded` with its immediate forward record; the cycle
/// itself is surfaced as a `supersession_cycle` diagnostic, never silent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BeliefStatus {
    /// No live author-written supersession or contradiction touches it.
    Current,
    /// An author-written supersession names a record that overrides it.
    Superseded,
    /// A live `CONTRADICTS` relationship connects it to another live
    /// agent-authored record. Both sides of a mutual contradiction appear.
    Contradicted,
}

/// The relating edge kind, when a supersession/contradiction relationship is
/// present on an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum BeliefRelation {
    /// The entry is superseded (`superseded_by` field, `SUPERSEDES` link/edge).
    #[serde(rename = "SUPERSEDES")]
    Supersedes,
    /// The entry is disputed (`CONTRADICTS` link/edge).
    #[serde(rename = "CONTRADICTS")]
    Contradicts,
}

/// Which timestamp ordered one timeline entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderingBasis {
    /// The entry's `observed_at` parsed as RFC 3339.
    ObservedAt,
    /// `observed_at` was absent/unparseable; `ingested_at` was used.
    IngestedAt,
    /// Neither timestamp was usable; the record ID ordered the entry.
    RecordId,
}

/// One entry of the belief timeline: citation metadata only, never a record
/// body.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BeliefEntry {
    /// Stable record ID.
    pub record_id: String,
    /// Citable handle: `agent_id:session_id` (or `agent_id`), the
    /// `TemporalReference` convention.
    pub handle: String,
    /// Node kind name: `Observation`, `Decision`, or `Failure`.
    pub kind: String,
    /// Machine-readable belief status.
    pub status: BeliefStatus,
    /// The relating edge kind, when a relationship is present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<BeliefRelation>,
    /// The record that overrode or disputed this one (absent for `current`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_record_id: Option<String>,
    /// Citable handle of the overriding/disputing record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_handle: Option<String>,
    /// Authoring agent identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Authoring agent kind (`claude-code`, `codex`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// Agent session that produced the record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Wall-clock time the agent observed the fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    /// Transaction time: when the record was committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at: Option<String>,
    /// Which timestamp ordered this entry.
    pub ordering_basis: OrderingBasis,
    /// The record's confidence string, when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// Citable handle(s) of the timeline target as cited by this entry.
    pub evidence_handles: Vec<String>,
}

/// A stable machine-readable diagnostic attached to a timeline.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BeliefDiagnostic {
    /// Stable code: `supersession_cycle` or `dangling_superseded_by`.
    pub code: String,
    /// One-line human explanation.
    pub message: String,
    /// The timeline record the diagnostic is about, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Sorted member record IDs (`supersession_cycle`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub record_ids: Vec<String>,
    /// The dangling forward name (`dangling_superseded_by`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forward_record_id: Option<String>,
}

/// The belief timeline for one resolved code target.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BeliefTimeline {
    /// The resolved code target.
    pub target: ResolvedBeliefTarget,
    /// The documented ordering rule (stated, never implicit).
    pub ordering: String,
    /// Number of entries.
    pub entry_count: usize,
    /// Entries oldest→newest.
    pub entries: Vec<BeliefEntry>,
    /// Stable diagnostics (cycles, dangling forward pointers).
    pub diagnostics: Vec<BeliefDiagnostic>,
}

/// Resolve a code-target handle to one stable code record.
///
/// Precedence: canonical codegraph record ID, then `path:line`, then an exact
/// repo-relative file path, then an exact symbol name (the identity rule
/// shared with `eg query lifeline`: one match resolves, several are
/// ambiguous). `repo_scope` restricts matching to one repository. Only live
/// (not tombstoned) `Symbol`/`File` nodes resolve; a handle matching only
/// tombstoned code records is [`BeliefTargetError::Stale`].
///
/// # Errors
///
/// Returns [`BeliefTargetError`] when the handle is empty (`Unsupported`),
/// matches no live code target (`Unknown`), matches only tombstoned records
/// (`Stale`), matches several live symbols (`Ambiguous`), or a `path:line`
/// handle names a known path whose line no symbol encloses
/// (`NoSymbolAtLine`).
#[must_use = "a resolution error must be surfaced as a lane diagnostic"]
pub fn resolve_belief_target(
    records: &[GraphRecord],
    handle: &str,
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> Result<ResolvedBeliefTarget, BeliefTargetError> {
    if handle.is_empty() {
        return Err(BeliefTargetError::Unsupported {
            handle: handle.to_owned(),
            message: "handle cannot be empty".to_owned(),
        });
    }
    let liveness = Liveness::new(records);
    let live = live_code_nodes(records, &liveness, index, repo_scope);

    // 1. Canonical codegraph record ID naming a live Symbol/File node.
    if let Some(node) = live.get(handle) {
        if let Some(target) = code_target_of(node) {
            return Ok(target);
        }
    }

    // 2. `path:line`: the path must name a known live code path; the target
    //    is the smallest enclosing Symbol at that line (narrowest span wins:
    //    line width, then byte width, then record ID).
    if let Some((path, line)) = split_path_line(handle) {
        if live.values().any(|node| node_path(node) == Some(path)) {
            let mut enclosing: Vec<(&GraphRecord, usize, usize, &str)> = live
                .values()
                .filter_map(|node| {
                    let GraphRecord::Node { kind, span, .. } = node else {
                        return None;
                    };
                    if !matches!(kind, NodeKind::Symbol) || node_path(node) != Some(path) {
                        return None;
                    }
                    let span = span.as_ref()?;
                    if span.start_line <= line && line <= span.end_line {
                        Some((
                            *node,
                            span.end_line.saturating_sub(span.start_line),
                            span.end_byte.saturating_sub(span.start_byte),
                            node_id(node),
                        ))
                    } else {
                        None
                    }
                })
                .collect();
            // Narrowest span wins: line width, then byte width, then record ID.
            // (The record itself carries no `Ord`, so sort on the key tuple.)
            enclosing.sort_by(|a, b| (a.1, a.2, a.3).cmp(&(b.1, b.2, b.3)));
            if let Some((node, _, _, _)) = enclosing.first() {
                if let Some(target) = code_target_of(node) {
                    return Ok(target);
                }
            }
            return Err(BeliefTargetError::NoSymbolAtLine {
                handle: handle.to_owned(),
                path: path.to_owned(),
                line,
            });
        }
    }

    // 3. Repo-relative file path naming a live File node.
    if let Some(node) = live
        .values()
        .find(|node| node_kind(node) == Some(NodeKind::File) && node_path(node) == Some(handle))
    {
        if let Some(target) = code_target_of(node) {
            return Ok(target);
        }
    }

    // 4. Exact symbol name: one live Symbol resolves, several are ambiguous.
    let named: Vec<&GraphRecord> = live
        .values()
        .filter(|node| node_kind(node) == Some(NodeKind::Symbol) && node_name(node) == Some(handle))
        .copied()
        .collect();
    if named.len() > 1 {
        return Err(BeliefTargetError::Ambiguous {
            handle: handle.to_owned(),
            candidates: named.iter().map(|node| node_id(node).to_owned()).collect(),
        });
    }
    if let [only] = named.as_slice() {
        if let Some(target) = code_target_of(only) {
            return Ok(target);
        }
    }

    // No live match: a handle matching only tombstoned code records is stale,
    // otherwise unknown.
    if is_stale_code_handle(
        records,
        &liveness,
        index,
        repo_scope,
        handle,
        split_path_line(handle).map(|(path, _)| path),
    ) {
        Err(BeliefTargetError::Stale {
            handle: handle.to_owned(),
        })
    } else {
        Err(BeliefTargetError::Unknown {
            handle: handle.to_owned(),
        })
    }
}

/// Build the belief timeline for a resolved code target.
///
/// Collects every live, agent-authored observation-class record citing
/// `target.record_id` (via denormalized `evidence_links` or a graph edge),
/// resolves each entry's status transitively through the shared
/// [`TemporalResolver`], and orders entries oldest→newest. Pure and
/// deterministic: reads the record slice only.
#[must_use]
pub fn belief_timeline(records: &[GraphRecord], target: &ResolvedBeliefTarget) -> BeliefTimeline {
    let liveness = Liveness::new(records);
    let resolver = TemporalResolver::build(records);

    // Latest live node per record ID (last physical write wins).
    let mut latest: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node { id, .. } = record {
            if !liveness.deleted(id.as_str()) {
                latest.insert(id.as_str(), record);
            }
        }
    }

    // Citation via a live graph edge into the target (the edge
    // representation of the same citation the denormalized evidence links
    // carry).
    let mut edge_cited: BTreeSet<&str> = BTreeSet::new();
    for (position, record) in records.iter().enumerate() {
        let GraphRecord::Edge {
            id,
            source,
            target: edge_target,
            ..
        } = record
        else {
            continue;
        };
        if edge_target.as_str() == target.record_id
            && !liveness.deleted(id.as_str())
            && liveness.is_latest_edge_version(id.as_str(), position)
        {
            edge_cited.insert(source.as_str());
        }
    }

    // Membership, in record-ID order: live agent beliefs citing the target.
    let members: Vec<&GraphRecord> = latest
        .values()
        .copied()
        .filter(|record| {
            let GraphRecord::Node {
                id, evidence_links, ..
            } = record
            else {
                return false;
            };
            if !is_belief_record(record) {
                return false;
            }
            let via_link = evidence_links.as_ref().is_some_and(|links| {
                links
                    .iter()
                    .any(|link| link.target_record_id.as_deref() == Some(target.record_id.as_str()))
            });
            via_link || edge_cited.contains(id.as_str())
        })
        .collect();

    // Per-entry forward pointers. Only agent beliefs dispute beliefs: the
    // overrider/disputant must itself be a live agent-authored
    // observation-class record. Supersession takes precedence over
    // contradiction. Non-qualifying supersession claims surface as
    // `dangling_superseded_by` diagnostics.
    let mut forward: BTreeMap<&str, (&str, BeliefRelation)> = BTreeMap::new();
    let mut dangling: BTreeSet<(String, String)> = BTreeSet::new();
    for member in &members {
        let GraphRecord::Node { id, .. } = member else {
            continue;
        };
        let member_id = id.as_str();
        let mut superseder: Option<&str> = None;
        for reference in resolver.immediate_superseders(member_id) {
            match live_belief_key(&latest, reference.record_id.as_str()) {
                Some(next) => {
                    superseder = Some(next);
                    break;
                }
                None => {
                    dangling.insert((member_id.to_owned(), reference.record_id.clone()));
                }
            }
        }
        if let Some(next) = superseder {
            forward.insert(member_id, (next, BeliefRelation::Supersedes));
            continue;
        }
        let mut disputant: Option<&str> = None;
        for reference in resolver.contradicting_records(member_id) {
            if let Some(other) = live_belief_key(&latest, reference.record_id.as_str()) {
                disputant = Some(other);
                break;
            }
        }
        if let Some(other) = disputant {
            forward.insert(member_id, (other, BeliefRelation::Contradicts));
        }
    }

    // Supersession cycles: walk each entry's forward supersession edge; a
    // revisit inside the current path is a cycle. Members are collected into
    // one stable diagnostic — the lane never loops or hangs.
    let mut cycle_members: BTreeSet<String> = BTreeSet::new();
    for member in &members {
        let GraphRecord::Node { id, .. } = member else {
            continue;
        };
        let mut path: Vec<&str> = Vec::new();
        let mut in_path: BTreeSet<&str> = BTreeSet::new();
        let mut cursor: &str = id.as_str();
        loop {
            if !in_path.insert(cursor) {
                if let Some(position) = path.iter().position(|seen| *seen == cursor) {
                    cycle_members.extend(path[position..].iter().map(|seen| (*seen).to_owned()));
                }
                break;
            }
            path.push(cursor);
            match forward.get(cursor) {
                Some((next, BeliefRelation::Supersedes)) => cursor = next,
                _ => break,
            }
        }
    }

    // Deterministic temporal order: observed_at, else ingested_at (RFC 3339;
    // unparseable counts as absent), else record ID. Absent timestamps sort
    // last.
    let mut members = members;
    members.sort_by(|a, b| {
        let (rank_a, time_a, _) = ordering_key(a);
        let (rank_b, time_b, _) = ordering_key(b);
        (rank_a, time_a, node_id(a)).cmp(&(rank_b, time_b, node_id(b)))
    });

    let mut entries: Vec<BeliefEntry> = Vec::with_capacity(members.len());
    for member in &members {
        let GraphRecord::Node {
            id,
            kind,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            ..
        } = member
        else {
            continue;
        };
        let member_id = id.as_str();
        let (status, relation) = match forward.get(member_id) {
            Some((_, BeliefRelation::Supersedes)) => {
                (BeliefStatus::Superseded, Some(BeliefRelation::Supersedes))
            }
            Some((_, BeliefRelation::Contradicts)) => (
                BeliefStatus::Contradicted,
                Some(BeliefRelation::Contradicts),
            ),
            None => (BeliefStatus::Current, None),
        };
        let (forward_record_id, forward_handle) = match forward.get(member_id) {
            Some((next, _)) => (Some((*next).to_owned()), Some(resolver.get_handle(next))),
            None => (None, None),
        };
        let (_, _, basis) = ordering_key(member);
        entries.push(BeliefEntry {
            record_id: id.clone(),
            handle: resolver.get_handle(member_id),
            kind: format!("{kind:?}"),
            status,
            relation,
            forward_record_id,
            forward_handle,
            agent_id: agent_id.clone(),
            agent_kind: agent_kind.clone(),
            session_id: session_id.clone(),
            observed_at: observed_at.clone(),
            ingested_at: ingested_at.clone(),
            ordering_basis: basis,
            confidence: confidence.clone(),
            evidence_handles: vec![target.target_handle.clone()],
        });
    }

    let mut diagnostics: Vec<BeliefDiagnostic> = Vec::new();
    if !cycle_members.is_empty() {
        diagnostics.push(BeliefDiagnostic {
            code: "supersession_cycle".to_owned(),
            message: "supersession claims form a cycle; each member reports \
                superseded with its immediate forward record"
                .to_owned(),
            record_id: None,
            record_ids: cycle_members.into_iter().collect(),
            forward_record_id: None,
        });
    }
    for (holder, dangling_name) in dangling {
        diagnostics.push(BeliefDiagnostic {
            code: "dangling_superseded_by".to_owned(),
            message: "author-written supersession claim names a record that is \
                not a live agent-authored observation-class record"
                .to_owned(),
            record_id: Some(holder),
            record_ids: Vec::new(),
            forward_record_id: Some(dangling_name),
        });
    }

    BeliefTimeline {
        target: target.clone(),
        ordering: BELIEF_TIMELINE_ORDERING.to_owned(),
        entry_count: entries.len(),
        entries,
        diagnostics,
    }
}

// ---------- helpers -------------------------------------------------------

/// Latest-write-wins view of live `Symbol`/`File` nodes, keyed by record ID
/// (ascending), repo-scoped.
fn live_code_nodes<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness,
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
) -> BTreeMap<&'a str, &'a GraphRecord> {
    let mut latest: BTreeMap<&'a str, &'a GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node { id, kind, .. } = record else {
            continue;
        };
        if !matches!(kind, NodeKind::Symbol | NodeKind::File) {
            continue;
        }
        if liveness.deleted(id.as_str()) {
            continue;
        }
        if repo_scope.is_some_and(|scope| index.owner_of(id.as_str()) != Some(scope)) {
            continue;
        }
        latest.insert(id.as_str(), record);
    }
    latest
}

/// True for a live agent-authored observation-class record: a belief.
fn is_belief_record(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node {
            kind,
            agent_id: Some(_),
            ..
        } if OBSERVATION_CLASS.contains(kind)
    )
}

/// The borrowed record-ID key for `name` when it is a live agent belief,
/// else `None`.
fn live_belief_key<'a>(latest: &BTreeMap<&'a str, &'a GraphRecord>, name: &str) -> Option<&'a str> {
    let (key, record) = latest.iter().find(|(key, _)| **key == name)?;
    if is_belief_record(record) {
        Some(*key)
    } else {
        None
    }
}

const fn node_id(record: &GraphRecord) -> &str {
    match record {
        GraphRecord::Node { id, .. }
        | GraphRecord::Edge { id, .. }
        | GraphRecord::Tombstone { id, .. } => id.as_str(),
    }
}

const fn node_kind(record: &GraphRecord) -> Option<NodeKind> {
    match record {
        GraphRecord::Node { kind, .. } => Some(*kind),
        _ => None,
    }
}

fn node_path(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            repo_relative_path, ..
        } => repo_relative_path.as_deref(),
        _ => None,
    }
}

fn node_name(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node { name, .. } => name.as_deref(),
        _ => None,
    }
}

/// Builds the citable target for a live `Symbol`/`File` node.
fn code_target_of(record: &GraphRecord) -> Option<ResolvedBeliefTarget> {
    let GraphRecord::Node {
        id,
        kind,
        repo_relative_path,
        name,
        ..
    } = record
    else {
        return None;
    };
    let kind = match kind {
        NodeKind::Symbol => BeliefTargetKind::Symbol,
        NodeKind::File => BeliefTargetKind::File,
        _ => return None,
    };
    let target_handle = match kind {
        BeliefTargetKind::Symbol => match (repo_relative_path.as_deref(), name.as_deref()) {
            (Some(path), Some(name)) => format!("{path}#{name}"),
            (Some(path), None) => format!("{path}#{}", id.as_str()),
            (None, Some(name)) => name.to_owned(),
            (None, None) => id.clone(),
        },
        BeliefTargetKind::File => repo_relative_path.clone().unwrap_or_else(|| id.clone()),
    };
    Some(ResolvedBeliefTarget {
        record_id: id.clone(),
        kind,
        target_handle,
    })
}

/// Splits a `path:line` handle at its last colon.
fn split_path_line(handle: &str) -> Option<(&str, usize)> {
    let (path, line_text) = handle.rsplit_once(':')?;
    if path.is_empty() {
        return None;
    }
    let line = line_text.parse::<usize>().ok()?;
    Some((path, line))
}

/// True when `handle` (or its path part) matches only tombstoned
/// `Symbol`/`File` records: the stale-handle diagnostic, distinct from
/// unknown.
#[allow(clippy::too_many_arguments)]
fn is_stale_code_handle(
    records: &[GraphRecord],
    liveness: &Liveness,
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    handle: &str,
    path_part: Option<&str>,
) -> bool {
    records.iter().any(|record| {
        let GraphRecord::Node {
            id,
            kind,
            repo_relative_path,
            name,
            ..
        } = record
        else {
            return false;
        };
        if !matches!(kind, NodeKind::Symbol | NodeKind::File) {
            return false;
        }
        if !liveness.deleted(id.as_str()) {
            return false;
        }
        if repo_scope.is_some_and(|scope| index.owner_of(id.as_str()) != Some(scope)) {
            return false;
        }
        id.as_str() == handle
            || repo_relative_path.as_deref() == Some(handle)
            || name.as_deref() == Some(handle)
            || path_part.is_some_and(|path| repo_relative_path.as_deref() == Some(path))
    })
}

/// The deterministic temporal ordering key for one member: rank (records with
/// a usable timestamp first), the timestamp, and which field supplied it.
/// Unparseable timestamps count as absent.
fn ordering_key(
    record: &GraphRecord,
) -> (u8, Option<DateTime<chrono::FixedOffset>>, OrderingBasis) {
    let GraphRecord::Node {
        observed_at,
        ingested_at,
        ..
    } = record
    else {
        return (1, None, OrderingBasis::RecordId);
    };
    if let Some(text) = observed_at.as_deref() {
        if let Ok(timestamp) = DateTime::parse_from_rfc3339(text) {
            return (0, Some(timestamp), OrderingBasis::ObservedAt);
        }
    }
    if let Some(text) = ingested_at.as_deref() {
        if let Ok(timestamp) = DateTime::parse_from_rfc3339(text) {
            return (0, Some(timestamp), OrderingBasis::IngestedAt);
        }
    }
    (1, None, OrderingBasis::RecordId)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{EdgeLabel, EvidenceLink, SourceSpan};

    fn symbol(id: &str, name: &str, path: &str, start_line: usize, end_line: usize) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 100,
                start_line,
                end_line,
                start_column: None,
                end_column: None,
            }),
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn file_record(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            None,
            format!("file {path}"),
        )
    }

    fn commit(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Commit,
            None,
            None,
            None,
            "commit".to_owned(),
        )
    }

    fn evidence_link(target_record_id: &str, relation: &str) -> EvidenceLink {
        EvidenceLink {
            target_record_id: Some(target_record_id.to_owned()),
            target_domain: "codegraph".to_owned(),
            relation: relation.to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn observation(
        id: &str,
        kind: NodeKind,
        agent: Option<&str>,
        agent_kind: Option<&str>,
        session: Option<&str>,
        observed_at: Option<&str>,
        ingested_at: Option<&str>,
        confidence: Option<&str>,
        target_id: Option<&str>,
        evidence_relation: Option<&str>,
        superseded_by: Option<&str>,
    ) -> GraphRecord {
        let mut record = GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("{kind:?} {id}"),
        );
        let GraphRecord::Node {
            agent_id,
            agent_kind: ak,
            session_id,
            observed_at: oa,
            ingested_at: ia,
            confidence: conf,
            superseded_by: sb,
            evidence_links,
            text,
            ..
        } = &mut record
        else {
            panic!("expected node record");
        };
        *agent_id = agent.map(str::to_owned);
        *ak = agent_kind.map(str::to_owned);
        *session_id = session.map(str::to_owned);
        *oa = observed_at.map(str::to_owned);
        *ia = ingested_at.map(str::to_owned);
        *conf = confidence.map(str::to_owned);
        *sb = superseded_by.map(str::to_owned);
        *evidence_links = target_id.map(|t| {
            vec![evidence_link(
                t,
                evidence_relation.unwrap_or("MENTIONS_SYMBOL"),
            )]
        });
        *text = Some(format!("SENTINEL_BODY_MUST_NOT_LEAK {id}"));
        record
    }

    fn target(target_id: &str) -> ResolvedBeliefTarget {
        ResolvedBeliefTarget {
            record_id: target_id.to_owned(),
            kind: BeliefTargetKind::Symbol,
            target_handle: "src/a.rs#foo".to_owned(),
        }
    }

    /// A←B←C chain (A superseded_by B, B superseded_by C), two agents,
    /// distinct observed_at values, all citing the same target.
    fn chain_fixture() -> Vec<GraphRecord> {
        vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:a",
                NodeKind::Observation,
                Some("agent_1"),
                Some("claude-code"),
                Some("session_1"),
                Some("2026-09-10T10:00:00Z"),
                Some("2026-09-10T10:05:00Z"),
                Some("high"),
                Some("symbol:foo"),
                None,
                Some("obs:b"),
            ),
            observation(
                "obs:b",
                NodeKind::Observation,
                Some("agent_2"),
                Some("codex"),
                Some("session_9"),
                Some("2026-09-11T10:00:00Z"),
                Some("2026-09-11T10:05:00Z"),
                Some("medium"),
                Some("symbol:foo"),
                None,
                Some("obs:c"),
            ),
            observation(
                "obs:c",
                NodeKind::Observation,
                Some("agent_1"),
                Some("claude-code"),
                Some("session_2"),
                Some("2026-09-12T10:00:00Z"),
                Some("2026-09-12T10:05:00Z"),
                Some("high"),
                Some("symbol:foo"),
                None,
                None,
            ),
        ]
    }

    fn entry_by_id<'a>(timeline: &'a BeliefTimeline, id: &str) -> &'a BeliefEntry {
        timeline
            .entries
            .iter()
            .find(|e| e.record_id == id)
            .unwrap_or_else(|| panic!("no entry for {id}"))
    }

    // ---- target resolution -------------------------------------------------

    #[test]
    fn resolve_symbol_by_canonical_id() {
        let records = vec![symbol("symbol:foo", "foo", "src/a.rs", 10, 20)];
        let index = RepositoryIndex::build(&records);
        let resolved =
            resolve_belief_target(&records, "symbol:foo", &index, None).expect("resolves");
        assert_eq!(resolved.record_id, "symbol:foo");
        assert_eq!(resolved.kind, BeliefTargetKind::Symbol);
        assert_eq!(resolved.target_handle, "src/a.rs#foo");
    }

    #[test]
    fn resolve_file_by_canonical_id() {
        let records = vec![file_record("file:src/a.rs", "src/a.rs")];
        let index = RepositoryIndex::build(&records);
        let resolved =
            resolve_belief_target(&records, "file:src/a.rs", &index, None).expect("resolves");
        assert_eq!(resolved.kind, BeliefTargetKind::File);
        assert_eq!(resolved.target_handle, "src/a.rs");
    }

    #[test]
    fn resolve_canonical_id_of_wrong_kind_is_unknown() {
        let records = vec![commit("commit:1")];
        let index = RepositoryIndex::build(&records);
        let err = resolve_belief_target(&records, "commit:1", &index, None).expect_err("unknown");
        assert_eq!(
            err,
            BeliefTargetError::Unknown {
                handle: "commit:1".to_owned()
            }
        );
    }

    #[test]
    fn resolve_file_by_path() {
        let records = vec![file_record("file:src/a.rs", "src/a.rs")];
        let index = RepositoryIndex::build(&records);
        let resolved = resolve_belief_target(&records, "src/a.rs", &index, None).expect("resolves");
        assert_eq!(resolved.record_id, "file:src/a.rs");
        assert_eq!(resolved.kind, BeliefTargetKind::File);
    }

    #[test]
    fn resolve_symbol_by_path_line_prefers_smallest_enclosing() {
        let records = vec![
            symbol("symbol:outer", "outer", "src/a.rs", 1, 100),
            symbol("symbol:inner", "inner", "src/a.rs", 10, 20),
        ];
        let index = RepositoryIndex::build(&records);
        let resolved =
            resolve_belief_target(&records, "src/a.rs:12", &index, None).expect("resolves");
        assert_eq!(resolved.record_id, "symbol:inner");
        assert_eq!(resolved.target_handle, "src/a.rs#inner");
    }

    #[test]
    fn resolve_path_line_with_no_enclosing_symbol_is_no_symbol_at_line() {
        let records = vec![file_record("file:src/a.rs", "src/a.rs")];
        let index = RepositoryIndex::build(&records);
        let err =
            resolve_belief_target(&records, "src/a.rs:3", &index, None).expect_err("no symbol");
        assert_eq!(
            err,
            BeliefTargetError::NoSymbolAtLine {
                handle: "src/a.rs:3".to_owned(),
                path: "src/a.rs".to_owned(),
                line: 3,
            }
        );
    }

    #[test]
    fn resolve_symbol_by_name() {
        let records = vec![symbol("symbol:foo", "foo", "src/a.rs", 10, 20)];
        let index = RepositoryIndex::build(&records);
        let resolved = resolve_belief_target(&records, "foo", &index, None).expect("resolves");
        assert_eq!(resolved.record_id, "symbol:foo");
    }

    #[test]
    fn resolve_unknown_handle() {
        let records = vec![symbol("symbol:foo", "foo", "src/a.rs", 10, 20)];
        let index = RepositoryIndex::build(&records);
        let err = resolve_belief_target(&records, "nope", &index, None).expect_err("unknown");
        assert_eq!(
            err,
            BeliefTargetError::Unknown {
                handle: "nope".to_owned()
            }
        );
    }

    #[test]
    fn resolve_empty_handle_is_unsupported() {
        let records: Vec<GraphRecord> = vec![];
        let index = RepositoryIndex::build(&records);
        let err = resolve_belief_target(&records, "", &index, None).expect_err("unsupported");
        assert!(matches!(err, BeliefTargetError::Unsupported { .. }));
    }

    #[test]
    fn resolve_ambiguous_name_across_repos_lists_sorted_candidates() {
        let records = vec![
            GraphRecord::node(
                "repo:a".to_owned(),
                NodeKind::Repository,
                None,
                None,
                Some("a".to_owned()),
                "repo a".to_owned(),
            ),
            GraphRecord::node(
                "repo:b".to_owned(),
                NodeKind::Repository,
                None,
                None,
                Some("b".to_owned()),
                "repo b".to_owned(),
            ),
            symbol("symbol:b:dup", "dup", "src/b.rs", 1, 10),
            symbol("symbol:a:dup", "dup", "src/a.rs", 1, 10),
            GraphRecord::edge(
                EdgeLabel::Contains,
                "repo:a".to_owned(),
                "symbol:a:dup".to_owned(),
                None,
                "c".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Contains,
                "repo:b".to_owned(),
                "symbol:b:dup".to_owned(),
                None,
                "c".to_owned(),
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let err = resolve_belief_target(&records, "dup", &index, None).expect_err("ambiguous");
        assert_eq!(
            err,
            BeliefTargetError::Ambiguous {
                handle: "dup".to_owned(),
                candidates: vec!["symbol:a:dup".to_owned(), "symbol:b:dup".to_owned()],
            }
        );
    }

    #[test]
    fn resolve_ambiguous_name_with_repo_scope_picks_scoped_repo() {
        let records = vec![
            GraphRecord::node(
                "repo:a".to_owned(),
                NodeKind::Repository,
                None,
                None,
                Some("a".to_owned()),
                "repo a".to_owned(),
            ),
            GraphRecord::node(
                "repo:b".to_owned(),
                NodeKind::Repository,
                None,
                None,
                Some("b".to_owned()),
                "repo b".to_owned(),
            ),
            symbol("symbol:b:dup", "dup", "src/b.rs", 1, 10),
            symbol("symbol:a:dup", "dup", "src/a.rs", 1, 10),
            GraphRecord::edge(
                EdgeLabel::Contains,
                "repo:a".to_owned(),
                "symbol:a:dup".to_owned(),
                None,
                "c".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::Contains,
                "repo:b".to_owned(),
                "symbol:b:dup".to_owned(),
                None,
                "c".to_owned(),
            ),
        ];
        let index = RepositoryIndex::build(&records);
        let resolved = resolve_belief_target(&records, "dup", &index, Some("repo:b"))
            .expect("scoped resolves");
        assert_eq!(resolved.record_id, "symbol:b:dup");
    }

    #[test]
    fn resolve_tombstoned_symbol_is_stale() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            GraphRecord::Tombstone {
                id: "tombstone:1".to_owned(),
                schema_version: 1,
                deleted_id: "symbol:foo".to_owned(),
                summary: "retracted".to_owned(),
                producer: None,
            },
        ];
        let index = RepositoryIndex::build(&records);
        let err = resolve_belief_target(&records, "foo", &index, None).expect_err("stale");
        assert_eq!(
            err,
            BeliefTargetError::Stale {
                handle: "foo".to_owned()
            }
        );
    }

    // ---- timeline ----------------------------------------------------------

    #[test]
    fn timeline_chain_a_b_c_statuses_and_forward_pointers() {
        let records = chain_fixture();
        let timeline = belief_timeline(&records, &target("symbol:foo"));

        assert_eq!(timeline.entry_count, 3);
        assert_eq!(timeline.ordering, BELIEF_TIMELINE_ORDERING);
        let ids: Vec<&str> = timeline
            .entries
            .iter()
            .map(|e| e.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["obs:a", "obs:b", "obs:c"], "oldest first");

        let a = entry_by_id(&timeline, "obs:a");
        assert_eq!(a.status, BeliefStatus::Superseded);
        assert_eq!(a.relation, Some(BeliefRelation::Supersedes));
        assert_eq!(a.forward_record_id.as_deref(), Some("obs:b"));
        assert_eq!(a.forward_handle.as_deref(), Some("agent_2:session_9"));
        assert_eq!(a.handle, "agent_1:session_1");
        assert_eq!(a.kind, "Observation");
        assert_eq!(a.ordering_basis, OrderingBasis::ObservedAt);
        assert_eq!(a.confidence.as_deref(), Some("high"));
        assert_eq!(a.observed_at.as_deref(), Some("2026-09-10T10:00:00Z"));

        let b = entry_by_id(&timeline, "obs:b");
        assert_eq!(b.status, BeliefStatus::Superseded);
        assert_eq!(b.forward_record_id.as_deref(), Some("obs:c"));

        let c = entry_by_id(&timeline, "obs:c");
        assert_eq!(c.status, BeliefStatus::Current);
        assert_eq!(c.forward_record_id, None);
    }

    #[test]
    fn timeline_includes_contradicted_pair() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:x",
                NodeKind::Observation,
                Some("agent_1"),
                Some("claude-code"),
                Some("session_1"),
                Some("2026-09-10T10:00:00Z"),
                Some("2026-09-10T10:05:00Z"),
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
            observation(
                "obs:y",
                NodeKind::Decision,
                Some("agent_2"),
                Some("codex"),
                Some("session_2"),
                Some("2026-09-11T10:00:00Z"),
                Some("2026-09-11T10:05:00Z"),
                None,
                Some("symbol:foo"),
                Some("CONTRADICTS"),
                None,
            ),
        ];
        // obs:y carries a CONTRADICTS evidence link toward obs:x.
        let mut records = records;
        if let GraphRecord::Node { evidence_links, .. } = &mut records[2] {
            *evidence_links = Some(vec![
                evidence_link("symbol:foo", "MENTIONS_SYMBOL"),
                evidence_link("obs:x", "CONTRADICTS"),
            ]);
        }
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert_eq!(timeline.entry_count, 2);
        let x = entry_by_id(&timeline, "obs:x");
        let y = entry_by_id(&timeline, "obs:y");
        assert_eq!(x.status, BeliefStatus::Contradicted);
        assert_eq!(y.status, BeliefStatus::Contradicted);
        assert_eq!(x.relation, Some(BeliefRelation::Contradicts));
        assert_eq!(y.relation, Some(BeliefRelation::Contradicts));
        assert_eq!(x.forward_record_id.as_deref(), Some("obs:y"));
        assert_eq!(y.forward_record_id.as_deref(), Some("obs:x"));
        assert_eq!(y.kind, "Decision");
    }

    #[test]
    fn timeline_uncontested_observations_are_current() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:one",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
            observation(
                "obs:two",
                NodeKind::Failure,
                Some("agent_2"),
                None,
                None,
                Some("2026-09-11T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert_eq!(timeline.entry_count, 2);
        for entry in &timeline.entries {
            assert_eq!(entry.status, BeliefStatus::Current);
            assert_eq!(entry.forward_record_id, None);
        }
    }

    #[test]
    fn timeline_empty_but_present_for_unrelated_target() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            symbol("symbol:bar", "bar", "src/b.rs", 1, 10),
            observation(
                "obs:other",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:bar"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert_eq!(timeline.entry_count, 0);
        assert!(timeline.entries.is_empty());
        assert!(timeline.diagnostics.is_empty());
        assert_eq!(timeline.target.record_id, "symbol:foo");
    }

    #[test]
    fn timeline_cycle_yields_diagnostic_not_hang() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:p",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                Some("obs:q"),
            ),
            observation(
                "obs:q",
                NodeKind::Observation,
                Some("agent_2"),
                None,
                None,
                Some("2026-09-11T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                Some("obs:p"),
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert_eq!(timeline.entry_count, 2);
        // The status vocabulary stays closed: each cycle member carries a
        // live supersession claim against it, so it reads `superseded` with
        // its immediate forward record. The cycle itself is a diagnostic.
        let p = entry_by_id(&timeline, "obs:p");
        assert_eq!(p.status, BeliefStatus::Superseded);
        assert_eq!(p.forward_record_id.as_deref(), Some("obs:q"));
        let q = entry_by_id(&timeline, "obs:q");
        assert_eq!(q.status, BeliefStatus::Superseded);
        assert_eq!(q.forward_record_id.as_deref(), Some("obs:p"));
        let diagnostic = timeline
            .diagnostics
            .iter()
            .find(|d| d.code == "supersession_cycle")
            .expect("cycle diagnostic present");
        assert_eq!(
            diagnostic.record_ids,
            vec!["obs:p".to_owned(), "obs:q".to_owned()]
        );
    }

    #[test]
    fn timeline_ordering_falls_back_to_ingested_at() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:no_observed",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
            observation(
                "obs:with_observed",
                NodeKind::Observation,
                Some("agent_2"),
                None,
                None,
                Some("2026-09-11T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let ids: Vec<&str> = timeline
            .entries
            .iter()
            .map(|e| e.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["obs:no_observed", "obs:with_observed"]);
        assert_eq!(
            timeline.entries[0].ordering_basis,
            OrderingBasis::IngestedAt
        );
        assert_eq!(
            timeline.entries[1].ordering_basis,
            OrderingBasis::ObservedAt
        );
    }

    #[test]
    fn timeline_unparseable_observed_at_falls_back_to_ingested_at() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:bad_ts",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("not-a-timestamp"),
                Some("2026-09-10T10:00:00Z"),
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert_eq!(timeline.entries.len(), 1);
        assert_eq!(
            timeline.entries[0].ordering_basis,
            OrderingBasis::IngestedAt
        );
    }

    #[test]
    fn timeline_tiebreak_by_record_id() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:b",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
            observation(
                "obs:a",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let ids: Vec<&str> = timeline
            .entries
            .iter()
            .map(|e| e.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["obs:a", "obs:b"]);
    }

    #[test]
    fn timeline_excludes_tombstoned_observations() {
        let mut records = chain_fixture();
        records.push(GraphRecord::Tombstone {
            id: "tombstone:1".to_owned(),
            schema_version: 1,
            deleted_id: "obs:a".to_owned(),
            summary: "retracted".to_owned(),
            producer: None,
        });
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let ids: Vec<&str> = timeline
            .entries
            .iter()
            .map(|e| e.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["obs:b", "obs:c"]);
    }

    #[test]
    fn timeline_excludes_records_without_agent_id() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:human",
                NodeKind::Observation,
                None,
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
            observation(
                "obs:agent",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-11T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let ids: Vec<&str> = timeline
            .entries
            .iter()
            .map(|e| e.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["obs:agent"]);
    }

    #[test]
    fn timeline_excludes_non_observation_class_records() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:toolcall",
                NodeKind::ToolCall,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert!(timeline.entries.is_empty());
    }

    #[test]
    fn timeline_cites_target_via_graph_edge_when_no_evidence_links() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:edge",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                None,
                None,
                None,
            ),
            GraphRecord::edge(
                EdgeLabel::MentionsSymbol,
                "obs:edge".to_owned(),
                "symbol:foo".to_owned(),
                None,
                "mentions".to_owned(),
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        assert_eq!(timeline.entry_count, 1);
        assert_eq!(timeline.entries[0].record_id, "obs:edge");
    }

    #[test]
    fn timeline_dangling_superseded_by_yields_diagnostic() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:dangling",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                Some("obs:missing"),
            ),
        ];
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let entry = entry_by_id(&timeline, "obs:dangling");
        assert_eq!(entry.status, BeliefStatus::Current);
        let diagnostic = timeline
            .diagnostics
            .iter()
            .find(|d| d.code == "dangling_superseded_by")
            .expect("dangling diagnostic present");
        assert_eq!(diagnostic.record_id.as_deref(), Some("obs:dangling"));
        assert_eq!(diagnostic.forward_record_id.as_deref(), Some("obs:missing"));
    }

    #[test]
    fn timeline_contradicts_code_target_does_not_contradict_observer() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:witness",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                None,
            ),
        ];
        let mut records = records;
        if let GraphRecord::Node { evidence_links, .. } = &mut records[1] {
            *evidence_links = Some(vec![
                evidence_link("symbol:foo", "MENTIONS_SYMBOL"),
                evidence_link("symbol:foo", "CONTRADICTS"),
            ]);
        }
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let entry = entry_by_id(&timeline, "obs:witness");
        assert_eq!(entry.status, BeliefStatus::Current);
    }

    #[test]
    fn timeline_superseded_takes_precedence_over_contradicted() {
        let records = vec![
            symbol("symbol:foo", "foo", "src/a.rs", 10, 20),
            observation(
                "obs:first",
                NodeKind::Observation,
                Some("agent_1"),
                None,
                None,
                Some("2026-09-10T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                None,
                Some("obs:second"),
            ),
            observation(
                "obs:second",
                NodeKind::Observation,
                Some("agent_2"),
                None,
                None,
                Some("2026-09-11T10:00:00Z"),
                None,
                None,
                Some("symbol:foo"),
                Some("CONTRADICTS"),
                None,
            ),
        ];
        let mut records = records;
        if let GraphRecord::Node { evidence_links, .. } = &mut records[2] {
            *evidence_links = Some(vec![
                evidence_link("symbol:foo", "MENTIONS_SYMBOL"),
                evidence_link("obs:first", "CONTRADICTS"),
            ]);
        }
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let first = entry_by_id(&timeline, "obs:first");
        assert_eq!(first.status, BeliefStatus::Superseded);
        assert_eq!(first.relation, Some(BeliefRelation::Supersedes));
        assert_eq!(first.forward_record_id.as_deref(), Some("obs:second"));
    }

    #[test]
    fn timeline_entry_carries_no_body_text() {
        let records = chain_fixture();
        let timeline = belief_timeline(&records, &target("symbol:foo"));
        let value = serde_json::to_value(&timeline).expect("serializes");
        let serialized = serde_json::to_string(&value).expect("stringifies");
        assert!(
            !serialized.contains("SENTINEL_BODY_MUST_NOT_LEAK"),
            "no record body leaks into the timeline"
        );
        for entry in value
            .get("entries")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            assert!(entry.get("text").is_none(), "entries expose no text field");
        }
    }

    #[test]
    fn timeline_is_deterministic_across_runs() {
        let records = chain_fixture();
        let first =
            serde_json::to_string(&belief_timeline(&records, &target("symbol:foo"))).expect("json");
        for _ in 0..4 {
            let again = serde_json::to_string(&belief_timeline(&records, &target("symbol:foo")))
                .expect("json");
            assert_eq!(first, again);
        }
    }
}
