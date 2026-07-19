//! Whole-repository logical eviction (issue #248).
//!
//! `eg forget-repo <selector>` logically evicts EVERY record belonging to ONE
//! repository from a shared multi-repo embedded store, across every domain
//! (code facts, semantic drift, agent memory, project/task, artifact,
//! verification, log), leaving co-resident repositories byte-identical.
//!
//! This is the SANCTIONED BULK EXCEPTION to issue #231's rule that deterministic
//! code facts are never tombstoned: the unit forgotten is the whole repository,
//! not a single fact being corrected. Eviction is *logical* — one
//! [`GraphRecord::Tombstone`] per attributed record plus exactly ONE auditable
//! eviction event (a reused [`NodeKind::Retraction`] node whose prior handle is
//! the repository identity) — so the bytes stay in the store for bi-temporal
//! history views while every current-state read/serving lane drops the repo.
//!
//! # Cross-domain attribution
//!
//! 1. **Seed** the owned set from [`RepositoryIndex`]: code-graph containment
//!    (`owner_of`), `SemanticDrift` (via `DRIFTS_FROM`), and log records (the
//!    `repository_id` payload field, issue #362).
//! 2. **Extend** by walking the cross-domain *evidence* subgraph (an exhaustive
//!    partition of [`EdgeLabel`], mirroring the #247 evidence-path set) undirected
//!    from the seed. A non-seed record reached from exactly ONE repository is
//!    attributed to it; a record reached from TWO OR MORE repositories is SHARED
//!    and is NEVER evicted (reported under `shared_cross_repo`).
//! 3. **Honest-gap**: a record with no derivable attribution (a legacy log with
//!    an empty `repository_id`, an orphan artifact) is REPORTED under
//!    `unattributable` and NEVER evicted. Attribution is never guessed.
//! 4. **Cross-repo citation**: a SURVIVING record that merely cites an evicted
//!    handle over an evidence edge is KEPT; the now-dangling link is REPORTED
//!    under `cross_repo_citations`, never silently dropped or cascade-evicted.
//!
//! The pure core here is deterministic: a pinned `transaction_time` yields a
//! byte-identical plan and envelope across runs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::Utc;
use serde_json::{Value, json};

use crate::{
    ir::{AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, GraphRecord, NodeKind, agent_memory_stable_id},
    query::{RepositoryIndex, RepositorySelectorError},
    redaction::{REDACTION_POLICY_VERSION, is_redacted, redact_value},
    schema_version::record_version,
};

/// The seven cross-domain namespaces a repository's records span, always present
/// in the per-domain plan counts (even at zero) so an agent can tell an empty
/// domain from an unreported one.
const PLAN_DOMAINS: [&str; 7] = [
    "codegraph",
    "semantic",
    "agent_memory",
    "project",
    "artifact",
    "verification",
    "log",
];

/// Request parameters for `eg forget-repo`.
#[derive(Debug, Clone)]
pub struct EvictionRequest {
    /// Repository selector (record ID, `owner/name`, basename, remote URL, root
    /// commit SHA, or canonical path).
    pub selector: String,
    /// Operator eviction reason (redaction policy v1 applies).
    pub reason: String,
    /// Operator handle recorded as the eviction actor (redacted).
    pub evicted_by: String,
    /// Optional fixed RFC 3339 transaction time for deterministic output.
    pub transaction_time: Option<String>,
}

/// Machine-readable eviction failure.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum EvictError {
    /// `--reason` is empty or whitespace-only.
    MissingReason,
    /// `--evicted-by` is empty or whitespace-only.
    MissingActor,
    /// `--transaction-time` is not a valid RFC 3339 instant.
    InvalidTransactionTime {
        /// The rejected value.
        value: String,
        /// Parse failure detail.
        message: String,
    },
    /// No repository in the store matches the selector.
    UnknownSelector {
        /// The selector as supplied.
        selector: String,
    },
    /// More than one repository matches the selector.
    AmbiguousSelector {
        /// The selector as supplied.
        selector: String,
        /// Stable repository record IDs of every match, sorted ascending.
        candidates: Vec<String>,
    },
}

impl EvictError {
    /// Returns the stable machine-readable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingReason => "missing_reason",
            Self::MissingActor => "missing_evicted_by",
            Self::InvalidTransactionTime { .. } => "invalid_transaction_time",
            Self::UnknownSelector { .. } => "unknown_repository_selector",
            Self::AmbiguousSelector { .. } => "ambiguous_repository_selector",
        }
    }

    /// Returns the process exit code: 2 for selector-resolution failures
    /// (unknown / ambiguous), 1 for malformed request fields.
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        match self {
            Self::UnknownSelector { .. } | Self::AmbiguousSelector { .. } => 2,
            _ => 1,
        }
    }

    /// Returns the machine-readable JSON error envelope.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let detail = match self {
            Self::MissingReason => json!({
                "message": "--reason must be a non-empty eviction reason",
            }),
            Self::MissingActor => json!({
                "message": "--evicted-by must be a non-empty operator handle",
            }),
            Self::InvalidTransactionTime { value, message } => json!({
                "value": value,
                "message": format!("invalid --transaction-time '{value}': {message}"),
            }),
            Self::UnknownSelector { selector } => json!({
                "selector": selector,
                "message": format!(
                    "no repository matches selector '{selector}'; \
                     eviction targets one repository identity"
                ),
            }),
            Self::AmbiguousSelector {
                selector,
                candidates,
            } => json!({
                "selector": selector,
                "candidates": candidates,
                "message": format!(
                    "selector '{selector}' matches {} repositories; \
                     disambiguate with a repository record ID or remote URL",
                    candidates.len()
                ),
            }),
        };
        json!({
            "ok": false,
            "error": { "code": self.code(), "detail": detail },
        })
    }
}

impl From<RepositorySelectorError> for EvictError {
    fn from(error: RepositorySelectorError) -> Self {
        match error {
            RepositorySelectorError::Unknown { selector } => Self::UnknownSelector { selector },
            RepositorySelectorError::Ambiguous {
                selector,
                candidates,
            } => Self::AmbiguousSelector {
                selector,
                candidates,
            },
        }
    }
}

/// A redaction-safe projection of one record slated for (or excluded from)
/// eviction: its stable ID, resolved domain, and kind handle.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct EvictedRecord {
    /// Stable record ID (the tombstone target / citation handle).
    pub record_id: String,
    /// Resolved record domain (`codegraph`, `agent_memory`, `log`, …).
    pub domain: String,
    /// Node kind name, or `edge` for edge records.
    pub kind: String,
}

/// A surviving record that cites an evicted handle over an evidence edge.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CrossRepoCitation {
    /// Stable record ID of the surviving citing record.
    pub citing_id: String,
    /// Owning repository of the citing record, when derivable.
    pub citing_repository: Option<String>,
    /// Stable record ID of the evicted record the link now dangles at.
    pub evicted_target: String,
}

/// The auditable repository-eviction event.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct RepoEvictionEvent {
    /// Stable record ID of the eviction event node.
    pub event_id: String,
    /// Stable repository record ID this event evicts (the prior handle).
    pub repository_id: String,
    /// Redacted operator handle recorded as the actor.
    pub evicted_by: String,
    /// Redacted eviction reason.
    pub reason: String,
    /// RFC 3339 transaction time of the eviction.
    pub transaction_time: String,
    /// Stable IDs of every tombstone this eviction writes, sorted ascending.
    pub tombstone_ids: Vec<String>,
}

/// A resolved eviction plan for one repository.
#[derive(Debug, Clone)]
pub struct EvictionPlan {
    /// Resolved (highest-version) repository record ID.
    pub repository_id: String,
    /// Human-usable display handle for the repository.
    pub repository_display: Option<String>,
    /// True when the repository was already evicted (idempotent no-op).
    pub already_evicted: bool,
    /// Records to tombstone (nodes + edges), sorted by record ID.
    pub evicted: Vec<EvictedRecord>,
    /// Per-domain evicted-record counts (all seven domains present).
    pub by_domain: BTreeMap<String, usize>,
    /// Records reported but NOT evicted because they have no attribution.
    pub unattributable: Vec<EvictedRecord>,
    /// Records reported but NOT evicted because they are shared across repos.
    pub shared_cross_repo: Vec<EvictedRecord>,
    /// Surviving cross-repository citations of evicted handles.
    pub cross_repo_citations: Vec<CrossRepoCitation>,
    /// The auditable eviction event.
    pub event: RepoEvictionEvent,
}

/// Builds the deterministic eviction-event record ID for a repository handle.
///
/// A distinct namespace (`repo_eviction`) from #231's `retraction` keeps the two
/// event kinds from ever colliding on a stable ID.
#[must_use]
pub fn eviction_event_id(repository_id: &str) -> String {
    agent_memory_stable_id(&["node", "repo_eviction", repository_id])
}

/// Builds the deterministic eviction-tombstone ID for a target record.
///
/// The tombstone lives in the same domain (and domain schema version) as the
/// target so reader-side version validation resolves it against the domain the
/// deleted record belongs to. The ID is a pure function of the target handle, so
/// an arbitrary tombstone can be recognized as an eviction tombstone by
/// recomputing this and comparing (see [`is_eviction_tombstone`]) — no schema
/// field and no summary marker needed.
#[must_use]
pub fn eviction_tombstone_id(target_id: &str) -> (String, u32) {
    let (domain, version) = target_id
        .split_once(":v")
        .and_then(|(domain, rest)| {
            let (version, _) = rest.split_once(':')?;
            Some((domain, version.parse::<u32>().ok()?))
        })
        .unwrap_or(("agent_memory", AGENT_MEMORY_SCHEMA_VERSION));
    let mut hasher = blake3::Hasher::new();
    for part in ["tombstone", "repo_eviction", target_id] {
        hasher.update(part.as_bytes());
        hasher.update(b"\0");
    }
    (
        format!("{domain}:v{version}:{}", hasher.finalize().to_hex()),
        version,
    )
}

/// True when `record` is a repository-eviction event node (self-verifying).
#[must_use]
fn is_eviction_event(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), id, .. }
            if id == &eviction_event_id(h)
    )
}

/// Classifies an [`EdgeLabel`] as a cross-domain evidence/provenance edge the
/// attribution walk may traverse.
///
/// Exhaustive `match` with NO wildcard arm (the #247 completeness invariant): a
/// newly added label fails to compile until it is consciously classified here.
/// TRAVERSED are the cross-domain grounding edges (evidence links, log topology,
/// project registry); EXCLUDED are code-graph topology (handled by
/// [`RepositoryIndex`] containment) and intra-agent-memory scaffolding.
const fn is_evidence_edge(label: EdgeLabel) -> bool {
    use EdgeLabel::{
        Aggregates, AuthoredBy, Calls, CapturedFrom, ChangedIn, ClosesAcceptanceCriterion,
        Contains, Contradicts, DecidedOn, Defines, DriftsFrom, DriftsPrior, EmittedDuring,
        ExplainsChange, ExternalHandle, FailedOn, FingerprintedAs, FrameResolvesTo, HasEvidence,
        Implements, Imports, MaterializedAs, MeasuredBy, Mentions, MentionsSymbol, MergedAs,
        Observes, OwnedByTask, ParentOf, ProducedEvidence, ProducedPatch, PromptedFor, ProposedBy,
        References, ReferencesTask, RelatesTo, RequestedReviewFrom, ReviewedBy, ReviewsCommit,
        RevokedBy, ScopedToRepo, SessionOf, Supersedes, TouchedFile, TouchesFile,
        TransitionsReview, ValidatedBy,
    };
    match label {
        // TRAVERSED — cross-domain evidence / provenance / grounding edges.
        HasEvidence
        | Observes
        | MentionsSymbol
        | TouchedFile
        | ProducedPatch
        | ProducedEvidence
        | ValidatedBy
        | ClosesAcceptanceCriterion
        | OwnedByTask
        | ExternalHandle
        | TouchesFile
        | MergedAs
        | ReviewsCommit
        | ReviewedBy
        | RequestedReviewFrom
        | TransitionsReview
        | FailedOn
        | ExplainsChange
        | ReferencesTask
        | Contradicts
        | Supersedes
        | RelatesTo
        | FrameResolvesTo
        | EmittedDuring
        | MaterializedAs
        | ProposedBy
        | PromptedFor
        | DecidedOn
        | RevokedBy
        | ScopedToRepo
        | FingerprintedAs
        | CapturedFrom
        | Aggregates => true,
        // EXCLUDED — code-graph topology (RepositoryIndex containment already
        // attributes these) and intra-agent-memory / semantic scaffolding.
        Contains | Defines | Imports | References | Calls | Implements | Mentions | ChangedIn
        | ParentOf | DriftsFrom | DriftsPrior | MeasuredBy | SessionOf | AuthoredBy => false,
    }
}

/// Resolves the domain string for a record via the reader-side version tuple.
fn record_domain(record: &GraphRecord) -> String {
    record_version(record).domain
}

/// Node-kind handle for an [`EvictedRecord`] projection.
fn record_kind(record: &GraphRecord) -> String {
    match record {
        GraphRecord::Node { kind, .. } => kind.as_str().to_owned(),
        GraphRecord::Edge { .. } => "edge".to_owned(),
        GraphRecord::Tombstone { .. } => "tombstone".to_owned(),
    }
}

fn projection(record: &GraphRecord) -> EvictedRecord {
    EvictedRecord {
        record_id: record.id().to_owned(),
        domain: record_domain(record),
        kind: record_kind(record),
    }
}

/// Validates the shared request fields.
fn validate_request(req: &EvictionRequest) -> Result<(), EvictError> {
    if req.reason.trim().is_empty() {
        return Err(EvictError::MissingReason);
    }
    if req.evicted_by.trim().is_empty() {
        return Err(EvictError::MissingActor);
    }
    if let Some(value) = req.transaction_time.as_deref()
        && let Err(error) = chrono::DateTime::parse_from_rfc3339(value)
    {
        return Err(EvictError::InvalidTransactionTime {
            value: value.to_owned(),
            message: error.to_string(),
        });
    }
    Ok(())
}

/// Seeds `by_domain` with every plan domain at zero.
fn empty_by_domain() -> BTreeMap<String, usize> {
    let mut by_domain = BTreeMap::new();
    for domain in PLAN_DOMAINS {
        by_domain.insert(domain.to_owned(), 0);
    }
    by_domain
}

/// Builds the idempotent no-op plan from an existing eviction event node.
fn already_evicted_plan(
    repository_id: String,
    repository_display: Option<String>,
    event_node: &GraphRecord,
) -> EvictionPlan {
    let (evicted_by, reason, transaction_time) = match event_node {
        GraphRecord::Node {
            text,
            agent_id,
            transaction_time,
            ..
        } => (
            agent_id.clone().unwrap_or_default(),
            text.clone().unwrap_or_default(),
            transaction_time.clone().unwrap_or_default(),
        ),
        _ => (String::new(), String::new(), String::new()),
    };
    let event = RepoEvictionEvent {
        event_id: eviction_event_id(&repository_id),
        repository_id: repository_id.clone(),
        evicted_by,
        reason,
        transaction_time,
        tombstone_ids: Vec::new(),
    };
    EvictionPlan {
        repository_id,
        repository_display,
        already_evicted: true,
        evicted: Vec::new(),
        by_domain: empty_by_domain(),
        unattributable: Vec::new(),
        shared_cross_repo: Vec::new(),
        cross_repo_citations: Vec::new(),
        event,
    }
}

/// Builds the undirected adjacency over the cross-domain evidence subgraph.
fn evidence_adjacency<'a>(
    node_records: &BTreeMap<&'a str, &'a GraphRecord>,
    edge_records: &BTreeMap<&'a str, &'a GraphRecord>,
) -> BTreeMap<&'a str, Vec<&'a str>> {
    let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for record in edge_records.values() {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
            && is_evidence_edge(*label)
            && node_records.contains_key(source.as_str())
            && node_records.contains_key(target.as_str())
        {
            adjacency
                .entry(source.as_str())
                .or_default()
                .push(target.as_str());
            adjacency
                .entry(target.as_str())
                .or_default()
                .push(source.as_str());
        }
    }
    adjacency
}

/// Walks the evidence subgraph from each repository's seed set, mapping each
/// reachable non-seed node to the set of repositories that reach it.
///
/// The walk never expands THROUGH another node's seed (owned) node, so ownership
/// never bleeds across repositories; it only propagates to non-seed records.
fn walk_reachability<'a>(
    repository_ids: &[&'a str],
    owner: &BTreeMap<&'a str, &'a str>,
    adjacency: &BTreeMap<&'a str, Vec<&'a str>>,
) -> BTreeMap<&'a str, BTreeSet<&'a str>> {
    let mut reached_by: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for &repo in repository_ids {
        let seeds: Vec<&str> = owner
            .iter()
            .filter(|&(_, &r)| r == repo)
            .map(|(&id, _)| id)
            .collect();
        let mut enqueued: BTreeSet<&str> = seeds.iter().copied().collect();
        let mut queue: VecDeque<&str> = seeds.into_iter().collect();
        while let Some(node) = queue.pop_front() {
            let Some(neighbors) = adjacency.get(node) else {
                continue;
            };
            for &neighbor in neighbors {
                if owner.contains_key(neighbor) {
                    continue; // another repo's seed node — never traverse through it.
                }
                reached_by.entry(neighbor).or_default().insert(repo);
                if enqueued.insert(neighbor) {
                    queue.push_back(neighbor);
                }
            }
        }
    }
    reached_by
}

/// Plans the logical eviction of one repository from `records`.
///
/// `records` is the current-state read (`read_all_records`): the repository
/// identity node survives eviction, so a previously-evicted repository still
/// resolves for the idempotent no-op.
///
/// # Errors
///
/// Returns a machine-readable [`EvictError`] when the request is malformed or the
/// selector fails to resolve (unknown / ambiguous).
// One cohesive pass: resolve, seed, partition, and assemble the plan. The
// evidence walk and idempotent branch are already factored into helpers above.
#[allow(clippy::too_many_lines)]
pub fn plan_eviction(
    records: &[GraphRecord],
    req: &EvictionRequest,
) -> Result<EvictionPlan, EvictError> {
    validate_request(req)?;

    // Build the repository index from the current-state view. The repository
    // IDENTITY node is deliberately never tombstoned by eviction (only its
    // content is), so the selector keeps resolving to the now-empty repository —
    // a scoped query returns a clean no-match rather than an "unknown selector"
    // error, and a re-run resolves for the idempotency check.
    let index = RepositoryIndex::build(records);
    let repository_id = index.resolve_selector(req.selector.trim())?.to_owned();
    let repository_display = index.display_of(&repository_id).map(str::to_owned);

    // Idempotency: once an eviction event names this repository, re-running never
    // writes a second event or duplicate tombstones.
    let existing_event = records.iter().find(|record| {
        matches!(
            record,
            GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), id, .. }
                if h == &repository_id && id == &eviction_event_id(&repository_id)
        )
    });
    if let Some(event_node) = existing_event {
        return Ok(already_evicted_plan(
            repository_id,
            repository_display,
            event_node,
        ));
    }

    // Deduplicate the (possibly superseded-inclusive) view by record ID.
    let mut node_records: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    let mut edge_records: BTreeMap<&str, &GraphRecord> = BTreeMap::new();
    for record in records {
        match record {
            GraphRecord::Node { id, .. } => {
                node_records.insert(id.as_str(), record);
            }
            GraphRecord::Edge { id, .. } => {
                edge_records.insert(id.as_str(), record);
            }
            GraphRecord::Tombstone { .. } => {}
        }
    }

    // Seed: the definitive per-node owner from RepositoryIndex (code-graph
    // containment, semantic drift, log `repository_id`).
    let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
    for &id in node_records.keys() {
        if let Some(repo) = index.owner_of(id) {
            owner.insert(id, repo);
        }
    }

    // Extend: from each repository's seed set, walk the cross-domain evidence
    // subgraph, mapping each reachable non-seed record to the repositories that
    // reach it.
    let adjacency = evidence_adjacency(&node_records, &edge_records);
    let repository_ids: Vec<&str> = index.repository_ids();
    let reached_by = walk_reachability(&repository_ids, &owner, &adjacency);

    let target = repository_id.as_str();

    // Partition nodes into evicted / shared / unattributable.
    let mut evicted_nodes: BTreeSet<&str> = BTreeSet::new();
    let mut shared: Vec<EvictedRecord> = Vec::new();
    let mut unattributable: Vec<EvictedRecord> = Vec::new();
    for (&id, &record) in &node_records {
        if is_eviction_event(record) {
            continue;
        }
        if let Some(&repo) = owner.get(id) {
            // The repository identity node is emptied, not tombstoned: its
            // content is evicted but the identity survives so the selector keeps
            // resolving to an empty, evicted repository.
            if repo == target && id != target {
                evicted_nodes.insert(id);
            }
            continue;
        }
        match reached_by.get(id) {
            None => unattributable.push(projection(record)),
            Some(repos) if repos.len() == 1 && repos.contains(target) => {
                evicted_nodes.insert(id);
            }
            Some(repos) if repos.contains(target) => shared.push(projection(record)),
            Some(_) => {} // reached only by other repositories — untouched.
        }
    }

    // Evicted edges: both endpoints are within the eviction scope — the evicted
    // content nodes plus the surviving repository identity node, so the
    // containment edges from the now-empty repository to its evicted content are
    // tombstoned rather than left dangling.
    let edge_scope = |id: &str| evicted_nodes.contains(id) || id == target;
    let mut evicted_edges: Vec<&str> = Vec::new();
    for (&id, &record) in &edge_records {
        if let GraphRecord::Edge {
            source, target: t, ..
        } = record
            && edge_scope(source.as_str())
            && edge_scope(t.as_str())
        {
            evicted_edges.push(id);
        }
    }

    // Cross-repo citations: an evidence edge with exactly one endpoint evicted
    // and the other a surviving node. The survivor is kept; the link is reported.
    let mut cross_repo_citations: Vec<CrossRepoCitation> = Vec::new();
    for record in edge_records.values() {
        if let GraphRecord::Edge {
            label,
            source,
            target: t,
            ..
        } = record
            && is_evidence_edge(*label)
        {
            let src_evicted = evicted_nodes.contains(source.as_str());
            let tgt_evicted = evicted_nodes.contains(t.as_str());
            let (survivor, evicted_target) = match (src_evicted, tgt_evicted) {
                (true, false) if node_records.contains_key(t.as_str()) => (t.as_str(), source),
                (false, true) if node_records.contains_key(source.as_str()) => (source.as_str(), t),
                _ => continue,
            };
            let citing_repository = owner.get(survivor).map(|r| (*r).to_owned()).or_else(|| {
                reached_by
                    .get(survivor)
                    .and_then(|repos| repos.iter().find(|&&r| r != target).map(|&r| r.to_owned()))
            });
            cross_repo_citations.push(CrossRepoCitation {
                citing_id: survivor.to_owned(),
                citing_repository,
                evicted_target: evicted_target.clone(),
            });
        }
    }

    // Assemble the sorted, deterministic evicted-record list.
    let mut evicted: Vec<EvictedRecord> = evicted_nodes
        .iter()
        .map(|&id| projection(node_records[id]))
        .chain(evicted_edges.iter().map(|&id| projection(edge_records[id])))
        .collect();
    evicted.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    unattributable.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    shared.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    cross_repo_citations.sort_by(|a, b| {
        (a.citing_id.as_str(), a.evicted_target.as_str())
            .cmp(&(b.citing_id.as_str(), b.evicted_target.as_str()))
    });

    let mut by_domain: BTreeMap<String, usize> = BTreeMap::new();
    for domain in PLAN_DOMAINS {
        by_domain.insert(domain.to_owned(), 0);
    }
    for record in &evicted {
        *by_domain.entry(record.domain.clone()).or_insert(0) += 1;
    }

    let transaction_time = req
        .transaction_time
        .clone()
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true));
    let mut tombstone_ids: Vec<String> = evicted
        .iter()
        .map(|record| eviction_tombstone_id(&record.record_id).0)
        .collect();
    tombstone_ids.sort();

    let event = RepoEvictionEvent {
        event_id: eviction_event_id(&repository_id),
        repository_id: repository_id.clone(),
        evicted_by: redact_value(req.evicted_by.trim()),
        reason: redact_value(req.reason.trim()),
        transaction_time,
        tombstone_ids,
    };

    Ok(EvictionPlan {
        repository_id,
        repository_display,
        already_evicted: false,
        evicted,
        by_domain,
        unattributable,
        shared_cross_repo: shared,
        cross_repo_citations,
        event,
    })
}

/// Builds the records a confirmed eviction writes: the single eviction event
/// node followed by one tombstone per evicted record.
///
/// The event is written first so the tombstones are the latest writes for their
/// targets and stay active (matching #231's ordering discipline). Returns an
/// empty vector for an already-evicted plan (idempotent no-op).
#[must_use]
pub fn eviction_records(plan: &EvictionPlan) -> Vec<GraphRecord> {
    if plan.already_evicted {
        return Vec::new();
    }
    let mut out: Vec<GraphRecord> = Vec::with_capacity(plan.evicted.len() + 1);
    out.push(build_event_node(&plan.event));
    for record in &plan.evicted {
        let (tombstone_id, version) = eviction_tombstone_id(&record.record_id);
        out.push(GraphRecord::Tombstone {
            id: tombstone_id,
            schema_version: version,
            deleted_id: record.record_id.clone(),
            summary: format!(
                "Repository eviction of {}; see eviction event {}",
                record.record_id, plan.event.event_id
            ),
            producer: None,
        });
    }
    out
}

/// Builds the citable eviction event node (a reused [`NodeKind::Retraction`]).
fn build_event_node(event: &RepoEvictionEvent) -> GraphRecord {
    let mut node = GraphRecord::node(
        event.event_id.clone(),
        NodeKind::Retraction,
        None,
        None,
        None,
        format!("Repository eviction of {}", event.repository_id),
    );
    if let GraphRecord::Node {
        ref mut schema_version,
        ref mut domain,
        ref mut text,
        ref mut agent_id,
        ref mut transaction_time,
        ref mut source_handle,
        ref mut valid_time,
        ref mut valid_time_source,
        ref mut redaction_policy_version,
        ..
    } = node
    {
        *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
        *domain = Some("agent_memory".to_owned());
        *text = Some(event.reason.clone());
        *agent_id = Some(event.evicted_by.clone());
        *transaction_time = Some(event.transaction_time.clone());
        *source_handle = Some(event.repository_id.clone());
        *valid_time = Some(event.transaction_time.clone());
        *valid_time_source = Some("inferred_from_transaction_time".to_owned());
        if is_redacted(&event.reason) || is_redacted(&event.evicted_by) {
            *redaction_policy_version = Some(REDACTION_POLICY_VERSION.to_owned());
        }
    }
    node
}

impl EvictionPlan {
    /// Renders the deterministic, redaction-safe JSON envelope for one action
    /// (`dry_run`, `evicted`, or `already_evicted`).
    #[must_use]
    pub fn to_envelope(&self, action: &str) -> Value {
        let planned = json!({
            "total": self.evicted.len(),
            "by_domain": self.by_domain,
            "representative_ids": representative_ids(&self.evicted),
        });
        let unattributable = json!({
            "total": self.unattributable.len(),
            "by_domain": count_by_domain(&self.unattributable),
            "representative_ids": representative_ids(&self.unattributable),
        });
        let shared = json!({
            "total": self.shared_cross_repo.len(),
            "representative_ids": representative_ids(&self.shared_cross_repo),
        });
        let citations: Vec<Value> = self
            .cross_repo_citations
            .iter()
            .map(|c| {
                json!({
                    "citing_id": c.citing_id,
                    "citing_repository": c.citing_repository,
                    "evicted_target": c.evicted_target,
                })
            })
            .collect();

        let mut envelope = json!({
            "ok": true,
            "action": action,
            "repository": {
                "id": self.repository_id,
                "display": self.repository_display,
            },
            "evicted_by": self.event.evicted_by,
            "reason": self.event.reason,
            "transaction_time": self.event.transaction_time,
            "planned": planned,
            "unattributable": unattributable,
            "shared_cross_repo": shared,
            "cross_repo_citations": citations,
        });
        if action != "dry_run" {
            envelope["eviction"] = json!({
                "event_id": self.event.event_id,
                "tombstone_count": self.event.tombstone_ids.len(),
            });
        }
        envelope
    }
}

fn representative_ids(records: &[EvictedRecord]) -> Vec<String> {
    records
        .iter()
        .take(5)
        .map(|record| record.record_id.clone())
        .collect()
}

fn count_by_domain(records: &[EvictedRecord]) -> BTreeMap<String, usize> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for record in records {
        *counts.entry(record.domain.clone()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::stable_id;

    #[test]
    fn eviction_ids_are_deterministic_and_namespaced() {
        let repo = "codegraph:v6:abc";
        assert_eq!(eviction_event_id(repo), eviction_event_id(repo));
        assert!(eviction_event_id(repo).starts_with("agent_memory:v1:"));
        // Distinct from the #231 retraction namespace.
        assert_ne!(
            eviction_event_id(repo),
            agent_memory_stable_id(&["node", "retraction", repo])
        );
        let (tid, ver) = eviction_tombstone_id(repo);
        assert!(tid.starts_with("codegraph:v6:"));
        assert_eq!(ver, 6);
    }

    #[test]
    fn eviction_tombstone_id_is_a_pure_function_of_the_target() {
        let target = stable_id(&["node", "symbol", "x"]);
        assert_eq!(
            eviction_tombstone_id(&target),
            eviction_tombstone_id(&target)
        );
        // Distinct from the target's own ID and stably namespaced in-domain.
        assert_ne!(eviction_tombstone_id(&target).0, target);
    }

    #[test]
    fn missing_reason_and_actor_are_rejected() {
        let mut req = EvictionRequest {
            selector: "acme/widget".to_owned(),
            reason: "  ".to_owned(),
            evicted_by: "op-1".to_owned(),
            transaction_time: None,
        };
        assert_eq!(
            plan_eviction(&[], &req).unwrap_err().code(),
            "missing_reason"
        );
        req.reason = "reason".to_owned();
        req.evicted_by = String::new();
        assert_eq!(
            plan_eviction(&[], &req).unwrap_err().code(),
            "missing_evicted_by"
        );
    }

    #[test]
    fn unknown_selector_maps_to_exit_2() {
        let req = EvictionRequest {
            selector: "no-such".to_owned(),
            reason: "reason".to_owned(),
            evicted_by: "op-1".to_owned(),
            transaction_time: Some("2026-07-01T00:00:00Z".to_owned()),
        };
        let err = plan_eviction(&[], &req).unwrap_err();
        assert_eq!(err.code(), "unknown_repository_selector");
        assert_eq!(err.exit_code(), 2);
    }
}
