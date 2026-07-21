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
//! [`GraphRecord::Tombstone`] per attributed record (INCLUDING the repository
//! identity node itself, so the evicted repo disappears from the catalog surface,
//! not just its contents) plus exactly ONE auditable eviction event (a reused
//! [`NodeKind::Retraction`] node whose prior handle is the repository identity) —
//! so the bytes stay in the store for bi-temporal history views while every
//! current-state read/serving lane drops the repo. Provenance survives in the
//! eviction event, which references the evicted identity as a string handle.
//!
//! # Scan-history temporal residual
//!
//! On a NON-temporal (`eg scan`) store a base-ID tombstone fully suppresses every
//! evicted record from every current-state read. On a `scan-history` (temporal)
//! store the shared `read_all_records` re-emits every commit-anchored code
//! snapshot with NO tombstone check — the same read path that serves issue #231
//! `forget`'s deliberate `--at`-after-deletion bi-temporal honesty — so a base-ID
//! tombstone CANNOT suppress commit-anchored code snapshots from the HEAD-anchored
//! current-state code lanes. The plan therefore discloses those still-visible
//! snapshots under `temporal_snapshots_retained` (empty on a non-temporal store)
//! rather than silently leaking them. See `docs/cli/forget-repo.md`.
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
    /// True when the repository was already evicted (idempotent no-op): a prior
    /// eviction event exists AND every tombstone-suppressible attributed record
    /// is still actively suppressed, so nothing is written.
    pub already_evicted: bool,
    /// True when this plan REPAIRS a prior partial eviction: an eviction event
    /// already exists, but attributed records are currently live again (a crash
    /// between the event write and the tombstone writes, or records revived by a
    /// later re-scan/re-ingest). The repair re-issues tombstones for the
    /// currently-live records WITHOUT writing a second eviction event; the
    /// original event is preserved verbatim. Mutually exclusive with
    /// `already_evicted`.
    pub repair: bool,
    /// Records to tombstone (nodes + edges), sorted by record ID.
    pub evicted: Vec<EvictedRecord>,
    /// Per-domain evicted-record counts (all seven domains present).
    pub by_domain: BTreeMap<String, usize>,
    /// Records reported but NOT evicted because they have no attribution.
    pub unattributable: Vec<EvictedRecord>,
    /// Records reported but NOT evicted because they are shared across repos.
    pub shared_cross_repo: Vec<EvictedRecord>,
    /// Evicted CODE records carrying commit/temporal metadata that a
    /// `scan-history` store's shared read path still re-emits into current-state
    /// code lanes despite the tombstone (the documented residual; empty on a
    /// non-temporal store).
    pub temporal_snapshots_retained: Vec<EvictedRecord>,
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

/// True when `record` is an eviction tombstone (self-verifying).
///
/// A tombstone is an eviction tombstone iff its own ID equals
/// [`eviction_tombstone_id`] recomputed from the record it deletes — no schema
/// field needed. Used to make an evicted repository identity RESOLVE again for the
/// idempotent no-op: the resolution index is built with these tombstones stripped,
/// so a re-run finds the surviving eviction EVENT (never a second tombstone pass)
/// even though the identity node is now tombstoned for every serving lane.
#[must_use]
fn is_eviction_tombstone(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Tombstone { id, deleted_id, .. }
            if id == &eviction_tombstone_id(deleted_id).0
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
        Constructs, Contains, Contradicts, DecidedOn, Defines, DriftsFrom, DriftsPrior,
        EmittedDuring, ExplainsChange, ExternalHandle, FailedOn, FingerprintedAs, FrameResolvesTo,
        HasEvidence, Implements, Imports, MaterializedAs, MeasuredBy, Mentions, MentionsSymbol,
        MergedAs, Observes, OwnedByTask, ParentOf, ProducedEvidence, ProducedPatch, PromptedFor,
        ProposedBy, References, ReferencesTask, RelatesTo, RequestedReviewFrom, ReviewedBy,
        ReviewsCommit, RevokedBy, ScopedToRepo, SessionOf, Supersedes, TouchedFile, TouchesFile,
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
        | Constructs | ParentOf | DriftsFrom | DriftsPrior | MeasuredBy | SessionOf
        | AuthoredBy => false,
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

/// Extracts the recorded (`evicted_by`, `reason`, `transaction_time`) triple from
/// a stored eviction event node, defaulting each missing field to empty. Shared by
/// the idempotent no-op and the repair path so a re-run always re-uses the
/// original event's actor / reason / transaction time verbatim.
fn event_meta(event_node: &GraphRecord) -> (String, String, String) {
    match event_node {
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
    }
}

/// Builds the idempotent no-op plan from an existing eviction event node.
fn already_evicted_plan(
    repository_id: String,
    repository_display: Option<String>,
    event_node: &GraphRecord,
) -> EvictionPlan {
    let (evicted_by, reason, transaction_time) = event_meta(event_node);
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
        repair: false,
        evicted: Vec::new(),
        by_domain: empty_by_domain(),
        unattributable: Vec::new(),
        shared_cross_repo: Vec::new(),
        temporal_snapshots_retained: Vec::new(),
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
/// `records` is the current-state read (`read_all_records`) — the working set the
/// plan is computed over. `resolution_records` is the history-inclusive read
/// (`read_all_records_including_superseded`) used ONLY to resolve the selector and
/// detect an existing eviction: eviction tombstones the repository identity node,
/// so it is gone from `records`, but its bytes survive in `resolution_records`
/// where — with eviction tombstones stripped — it resolves again for the
/// idempotent no-op. Pass the same slice for both when no eviction has run yet.
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
    resolution_records: &[GraphRecord],
    req: &EvictionRequest,
) -> Result<EvictionPlan, EvictError> {
    validate_request(req)?;

    // Resolve the selector against the history-inclusive view with eviction
    // tombstones stripped, so an already-evicted repository (its identity node
    // tombstoned out of every serving lane) still resolves — the idempotent no-op
    // keys on the surviving eviction EVENT, not on the now-tombstoned identity.
    // Eviction tombstones only exist AFTER a prior eviction, so the common path
    // (first eviction) builds the index directly without cloning the view.
    let stripped_view: Vec<GraphRecord>;
    let resolution_view: &[GraphRecord] = if resolution_records.iter().any(is_eviction_tombstone) {
        stripped_view = resolution_records
            .iter()
            .filter(|record| !is_eviction_tombstone(record))
            .cloned()
            .collect();
        &stripped_view
    } else {
        resolution_records
    };
    let index = RepositoryIndex::build(resolution_view);
    let repository_id = index.resolve_selector(req.selector.trim())?.to_owned();
    let repository_display = index.display_of(&repository_id).map(str::to_owned);

    // Idempotency + repair: once an eviction event names this repository, a
    // re-run never writes a SECOND event. But the no-op is only safe when the
    // prior eviction still actively suppresses every tombstone-suppressible
    // attributed record. A crash between the event write and the tombstone
    // writes, or records revived by a later re-scan/re-ingest, can leave the
    // event present while the repository's records are live again — so detect
    // the event here, then decide no-op vs. REPAIR after recomputing attribution
    // over the current-state view (mirrors #231 `eg forget`'s repair path).
    let existing_event = records.iter().find(|record| {
        matches!(
            record,
            GraphRecord::Node { kind: NodeKind::Retraction, source_handle: Some(h), id, .. }
                if h == &repository_id && id == &eviction_event_id(&repository_id)
        )
    });

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
            // Evict every owned node INCLUDING the repository identity node
            // itself, so the evicted repo drops from the catalog surface (a
            // `--repo <evicted>` selector then resolves as unknown, exit 1), not
            // just from its content lanes. Provenance survives in the eviction
            // event's string handle to the identity.
            if repo == target {
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

    // Evicted edges: an edge is part of repo A's footprint whenever its SOURCE
    // (the owning endpoint) is an evicted node — regardless of whether the target
    // survives. This covers both fully-internal edges (both endpoints evicted,
    // e.g. the identity node's containment edges to evicted content) AND an
    // outbound A->B edge (CALLS/REFERENCES/etc.) into a surviving repository. The
    // adapter's current-state read (`read_all_records` -> `latest_edge_versions`)
    // suppresses an edge only when the edge's OWN id is tombstoned, so leaving a
    // source-evicted edge un-tombstoned would leak part of repo A after eviction.
    // The mirror direction (a SURVIVING source pointing at an evicted target) is a
    // cross-repo citation, handled below.
    let mut evicted_edges: Vec<&str> = Vec::new();
    for (&id, &record) in &edge_records {
        if let GraphRecord::Edge { source, .. } = record
            && evicted_nodes.contains(source.as_str())
        {
            evicted_edges.push(id);
        }
    }

    // Cross-repo citations: an evidence edge whose SOURCE is a surviving node and
    // whose TARGET is an evicted node. The survivor (source) is kept — evicting it
    // would alter the co-resident repository's footprint — and its now-dangling
    // link is reported, never silently dropped. The opposite direction (source
    // evicted) is repo A's own footprint and is tombstoned above, so it never
    // reaches this citation branch.
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

    // Disclose the scan-history temporal residual: evicted CODE nodes carrying
    // commit/temporal metadata are re-emitted by the shared `read_all_records`
    // by-commit loop with no tombstone check, so current-state code lanes still
    // surface them on a `scan-history` store despite the tombstone. Empty on a
    // non-temporal store, where a base-ID tombstone fully suppresses the node.
    let mut temporal_snapshots_retained: Vec<EvictedRecord> = evicted_nodes
        .iter()
        .filter(|&&id| {
            matches!(
                node_records.get(id),
                Some(GraphRecord::Node {
                    temporal: Some(_),
                    ..
                })
            )
        })
        .map(|&id| projection(node_records[id]))
        .collect();
    temporal_snapshots_retained.sort_by(|a, b| a.record_id.cmp(&b.record_id));

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

    // A temporal (scan-history) code snapshot cannot be suppressed by a base-ID
    // tombstone (the shared read path re-emits it regardless), so it is a
    // DOCUMENTED RESIDUAL, never a repair trigger — otherwise a re-run over a
    // temporal store would loop, re-issuing ineffective tombstones forever.
    // Repair triggers only on a currently-live record a tombstone CAN suppress.
    let temporal_ids: BTreeSet<&str> = temporal_snapshots_retained
        .iter()
        .map(|record| record.record_id.as_str())
        .collect();
    let has_suppressible_live = evicted
        .iter()
        .any(|record| !temporal_ids.contains(record.record_id.as_str()));

    if let Some(event_node) = existing_event {
        if !has_suppressible_live {
            // True idempotent no-op: the prior eviction still fully suppresses
            // every tombstone-suppressible attributed record. Nothing is written.
            return Ok(already_evicted_plan(
                repository_id,
                repository_display,
                event_node,
            ));
        }
        // REPAIR: the event exists but attributed records are live again. Re-issue
        // tombstones for the currently-live records WITHOUT writing a second
        // eviction event; the original event's actor / reason / transaction time
        // are preserved verbatim.
        let (evicted_by, reason, transaction_time) = event_meta(event_node);
        let mut tombstone_ids: Vec<String> = evicted
            .iter()
            .map(|record| eviction_tombstone_id(&record.record_id).0)
            .collect();
        tombstone_ids.sort();
        let event = RepoEvictionEvent {
            event_id: eviction_event_id(&repository_id),
            repository_id: repository_id.clone(),
            evicted_by,
            reason,
            transaction_time,
            tombstone_ids,
        };
        return Ok(EvictionPlan {
            repository_id,
            repository_display,
            already_evicted: false,
            repair: true,
            evicted,
            by_domain,
            unattributable,
            shared_cross_repo: shared,
            temporal_snapshots_retained,
            cross_repo_citations,
            event,
        });
    }

    // Fresh eviction: no prior event names this repository.
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
        repair: false,
        evicted,
        by_domain,
        unattributable,
        shared_cross_repo: shared,
        temporal_snapshots_retained,
        cross_repo_citations,
        event,
    })
}

/// Builds the records a confirmed eviction writes: the single eviction event
/// node followed by one tombstone per evicted record.
///
/// The event is written first so the tombstones are the latest writes for their
/// targets and stay active (matching #231's ordering discipline). Returns an
/// empty vector for an already-evicted plan (idempotent no-op). On a REPAIR plan
/// (`repair`) the event node is OMITTED — the original event survives and no
/// second event is written — and only the re-issued tombstones are returned.
#[must_use]
pub fn eviction_records(plan: &EvictionPlan) -> Vec<GraphRecord> {
    if plan.already_evicted {
        return Vec::new();
    }
    let mut out: Vec<GraphRecord> = Vec::with_capacity(plan.evicted.len() + 1);
    if !plan.repair {
        out.push(build_event_node(&plan.event));
    }
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
        let temporal_retained = json!({
            "total": self.temporal_snapshots_retained.len(),
            "representative_ids": representative_ids(&self.temporal_snapshots_retained),
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
            "temporal_snapshots_retained": temporal_retained,
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
            plan_eviction(&[], &[], &req).unwrap_err().code(),
            "missing_reason"
        );
        req.reason = "reason".to_owned();
        req.evicted_by = String::new();
        assert_eq!(
            plan_eviction(&[], &[], &req).unwrap_err().code(),
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
        let err = plan_eviction(&[], &[], &req).unwrap_err();
        assert_eq!(err.code(), "unknown_repository_selector");
        assert_eq!(err.exit_code(), 2);
    }
}
