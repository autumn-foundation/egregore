//! Repo-scoped, recency-ordered digest of recent agent sessions (issue #112).
//!
//! Answers "what did agents recently do in this repository?" from records that
//! already exist: one row per live `AgentSession` whose members cite code in the
//! selected repository, ordered by last activity descending, carrying the
//! agent/session handles, time bounds, run outcomes, referenced tasks, and
//! per-kind record counts.
//!
//! Everything here is an AGENT CLAIM. A row states what an agent recorded, never
//! that the work happened, that a task completed, or that code works. Task
//! status is a recorded project-domain fact carried through verbatim, not a
//! correctness judgement.
//!
//! # Determinism
//!
//! The pure core is transport-agnostic: it consumes an append-ordered record
//! slice (a `--graph` JSONL or an embedded current-state read) and every
//! intermediate collection is a `BTreeMap`/`BTreeSet`, so output is byte
//! identical across runs and across transports. Membership is EDGE-DERIVED only
//! — a matching `session_id` string is not membership — and all timestamp
//! ordering is by parsed UTC instant, never raw RFC 3339 string order.

use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use chrono::{DateTime, Utc};

use super::{RepositoryIndex, liveness::Liveness};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind};

// ---------------------------------------------------------------------------
// Pinned contract constants
// ---------------------------------------------------------------------------

/// Default number of session rows returned when `--limit` is not supplied.
pub const SESSIONS_DEFAULT_LIMIT: usize = 20;

/// Largest accepted `--limit`; anything outside `1..=SESSIONS_MAX_LIMIT` is
/// rejected with an `invalid_limit` diagnostic before any store is read.
pub const SESSIONS_MAX_LIMIT: usize = 200;

/// Maximum `runs` entries carried on one row before `runs_truncated` fires.
pub const MAX_RUNS_PER_SESSION: usize = 20;

/// Maximum `tasks` entries carried on one row before `tasks_truncated` fires.
pub const MAX_TASKS_PER_SESSION: usize = 20;

/// Maximum session IDs listed on one `unresolved_repository_scope` diagnostic.
///
/// Unlike row truncation, this list is NOT bounded by
/// `--limit`/`budget.max_results` (it describes sessions excluded from every
/// digest, not rows in this one), so a store with many not-yet-linked
/// imported sessions could otherwise serialize an unbounded payload.
pub const MAX_UNRESOLVED_SESSION_IDS: usize = 200;

/// The standing epistemic disclaimer every sessions answer carries verbatim,
/// on both the CLI envelope and the daemon verb result.
pub const SESSIONS_DISCLAIMER: &str = "rows are recorded agent-authored memory and project-state facts; outcomes, observations, decisions, failures, and counts are agent claims, never verification, proof of task completion, or proof that code works; an absent citation is not evidence that no work happened; task status is a recorded project-domain fact, not a correctness claim";

/// Count kinds the digest cannot report because no backing `NodeKind` exists.
///
/// `record_counts.lesson` is therefore always JSON `null` (never `0`, which
/// would claim "we looked and found none"), and the envelope discloses the gap.
pub const SESSIONS_UNSUPPORTED_COUNT_KINDS: &[&str] = &["lesson"];

/// Maximum characters of any single free-text session field that reaches
/// rendered output.
///
/// `agent_id` / `session_id` are importer-supplied strings: nothing in the
/// schema bounds their length, so a crafted (or merely pathological) record can
/// carry megabytes of text. Both the core-computed `summary_label` (which
/// interpolates them) and the CLI text renderer cap at this one constant, so
/// the two transports bound identically. Mirrors
/// `crate::embeddings::IDENTITY_FIELD_MAX_CHARS` (issue #104), which bounds the
/// same class of store-read, operator-controlled value.
pub const SESSIONS_FIELD_MAX_CHARS: usize = 128;

/// Marker appended when [`SESSIONS_FIELD_MAX_CHARS`] truncates a value, so a
/// cap is visible rather than silent.
const TRUNCATION_MARKER: char = '…';

/// Replacement for a control character in rendered text output.
const CONTROL_REPLACEMENT: char = '·';

/// Aggregation scope stamped on every session row (see
/// [`SessionRow::aggregation_scope`]).
const WHOLE_SESSION_AGGREGATION: &str = "whole_session";

/// Trust class stamped on every session row.
const SESSION_TRUST_CLASS: &str = "agent_authored";

/// Trust class stamped on every referenced task row.
const TASK_TRUST_CLASS: &str = "project_state";

/// The closed project-domain task-status vocabulary. Duplicated from
/// `crate::local_project::valid_task_statuses` through a `pub(crate)` accessor
/// so the digest and the importer can never drift.
fn valid_task_status(status: &str) -> bool {
    crate::local_project::valid_task_statuses().contains(&status)
}

/// Maximum hop distance from a session at which a record is still a member:
/// `X -AUTHORED_BY-> U -AUTHORED_BY-> R -SESSION_OF-> S`.
const MEMBERSHIP_MAX_HOPS: usize = 3;

/// Edge relations that make a member a CODE citation.
const CODE_CITATION_RELATIONS: &[EdgeLabel] = &[
    EdgeLabel::MentionsSymbol,
    EdgeLabel::TouchedFile,
    EdgeLabel::Observes,
    EdgeLabel::FailedOn,
];

/// Edge relations that carry a project `Task` to the code it names. Only these
/// are followed from a referenced task — the traversal never continues from one
/// task to another.
const TASK_CODE_RELATIONS: &[EdgeLabel] = &[EdgeLabel::MentionsSymbol, EdgeLabel::TouchesFile];

/// Every relation the citation index is ever asked about: the union of
/// [`CODE_CITATION_RELATIONS`], `REFERENCES_TASK`, and [`TASK_CODE_RELATIONS`].
///
/// The adjacency builder inserts ONLY these, so whole-repository code-graph
/// topology (`CONTAINS` / `DEFINES` / `CALLS` / …) never bloats the map: on a
/// real store that topology is the overwhelming majority of edges and none of
/// it can ever be returned by `cited_targets`.
const CITATION_RELATIONS: &[EdgeLabel] = &[
    EdgeLabel::MentionsSymbol,
    EdgeLabel::TouchedFile,
    EdgeLabel::TouchesFile,
    EdgeLabel::Observes,
    EdgeLabel::FailedOn,
    EdgeLabel::ReferencesTask,
];

// ---------------------------------------------------------------------------
// Output types (serde field order is the wire order)
// ---------------------------------------------------------------------------

/// One recorded agent run belonging to a session.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct RunRow {
    /// Stable `AgentRun` record ID.
    pub run_record_id: String,
    /// Outcome token parsed from the exact producer template; `None` when the
    /// summary does not match it. Never guessed.
    pub outcome: Option<String>,
    /// Exit-reason token parsed from the same template; `None` alongside
    /// `outcome`.
    pub exit_reason: Option<String>,
    /// The run's recorded `observed_at`, PARSED and re-rendered through
    /// [`render_instant`] — never the stored bytes verbatim.
    ///
    /// `None` when the field is absent, empty, or not RFC 3339: an unparseable
    /// value is already counted by the session's `unparseable_timestamp`
    /// diagnostic, so forwarding it would add nothing but a free-text escape
    /// hatch (a crafted value could forge lines in the text renderer) and would
    /// render inconsistently with the session's own re-rendered bounds.
    pub observed_at: Option<String>,
}

/// One project-domain `Task` a session's members referenced.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct TaskRef {
    /// Stable `Task` record ID.
    pub record_id: String,
    /// Recorded status, validated against the closed project vocabulary.
    /// An absent or out-of-vocabulary status normalizes to `unknown`.
    pub status: String,
    /// `true` only when the record carried a status inside the closed set.
    pub status_recorded: bool,
    /// Always `project_state`: task status is a project fact, not an agent claim.
    pub trust_class: &'static str,
}

/// Distinct member record counts by agent-memory node kind.
#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize)]
pub struct SessionCounts {
    /// Distinct `Observation` members.
    pub observation: u64,
    /// Distinct `Decision` members.
    pub decision: u64,
    /// Distinct `Failure` members.
    pub failure: u64,
    /// Always `null`: no `Lesson` node kind exists (see
    /// [`SESSIONS_UNSUPPORTED_COUNT_KINDS`]).
    pub lesson: Option<u64>,
}

/// One session row in the digest.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct SessionRow {
    /// Stable `AgentSession` record ID.
    pub session_record_id: String,
    /// Always `agent_authored`.
    pub trust_class: &'static str,
    /// Stable `Agent` record ID reached by the session's own `SESSION_OF` edge;
    /// `None` when no such edge exists (a stamped `agent_id` alone is not a
    /// citable agent handle).
    pub agent_record_id: Option<String>,
    /// The session node's recorded `agent_id` string.
    pub agent_id: Option<String>,
    /// The session node's recorded `session_id` string.
    pub session_id: Option<String>,
    /// Redaction-safe structured label for the session summary.
    pub summary_label: String,
    /// BLAKE3 handle over the stored summary bytes; the raw summary never
    /// leaves the store through this lane.
    pub summary_hash: Option<String>,
    /// Earliest parseable `observed_at` over the session and its members.
    pub first_activity: Option<String>,
    /// Latest parseable `observed_at` over the session and its members.
    pub last_activity: Option<String>,
    /// Earliest parseable `ingested_at` over the session and its members.
    pub first_ingested_at: Option<String>,
    /// Latest parseable `ingested_at` over the session and its members.
    pub last_ingested_at: Option<String>,
    /// `derived_from_member_observed_at` when any activity time was parseable,
    /// `absent` otherwise.
    pub time_basis: &'static str,
    /// Number of parseable `observed_at` values that fed the bounds.
    pub time_source_count: u64,
    /// Every repository this session resolves to, sorted ascending.
    ///
    /// This is the key set of [`Self::scope_basis_by_repository`].
    pub repository_scope: Vec<String>,
    /// The UNION of every per-repository basis: `code_citation` and/or
    /// `task_reference`, sorted ascending.
    ///
    /// A union answers "how did this session reach code at all"; it does NOT
    /// say how it reached the repository you queried. Read
    /// [`Self::scope_basis_by_repository`] for that.
    pub scope_basis: Vec<&'static str>,
    /// The basis set PER repository, keyed by repository record ID (keys sorted
    /// by the backing `BTreeMap`, values sorted ascending).
    ///
    /// A session attributed to repo A by a direct code citation and to repo B
    /// only through a referenced task carries
    /// `{A: ["code_citation"], B: ["task_reference"]}` — the flat
    /// [`Self::scope_basis`] union would claim `code_citation` in B's digest
    /// too, which is not a fact about B.
    pub scope_basis_by_repository: BTreeMap<String, Vec<&'static str>>,
    /// `run_absent` / `outcome_recorded` / `outcome_unrecorded` / `multiple_runs`.
    pub run_status: &'static str,
    /// The session's runs, ordered by `observed_at` ascending (absent last),
    /// then record ID.
    pub runs: Vec<RunRow>,
    /// Distinct referenced tasks, ordered by record ID.
    pub tasks: Vec<TaskRef>,
    /// Always `whole_session`: the counts, runs, tasks, and time bounds on this
    /// row aggregate over the session's FULL edge-derived membership,
    /// regardless of which repository was queried.
    ///
    /// A cross-repository session therefore reports the same totals in every
    /// digest it appears in; they are not per-repository slices. Consult
    /// [`Self::repository_scope`] together with
    /// [`Self::scope_basis_by_repository`] to spot such a session. This slice
    /// DISCLOSES the aggregation rather than computing per-repository counts.
    pub aggregation_scope: &'static str,
    /// Distinct member record counts by kind.
    pub record_counts: SessionCounts,
}

/// One machine-readable diagnostic on a sessions answer.
///
/// Every variant carries a stable `code`; the optional fields are the payload
/// that code defines. Absent fields are omitted rather than serialized `null`,
/// so a diagnostic never suggests a value it does not carry.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct SessionsDiagnostic {
    /// Stable diagnostic code.
    pub code: &'static str,
    /// Session this diagnostic is about, when it is session-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_record_id: Option<String>,
    /// Run this diagnostic is about, when it is run-scoped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_record_id: Option<String>,
    /// Every session named by a set-valued diagnostic, sorted ascending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_record_ids: Option<Vec<String>>,
    /// Generic count payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// True total before truncation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched: Option<u64>,
    /// Number actually returned after truncation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub returned: Option<u64>,
    /// The cap that produced the truncation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

impl SessionsDiagnostic {
    /// Builds a diagnostic carrying only its stable code.
    const fn bare(code: &'static str) -> Self {
        Self {
            code,
            session_record_id: None,
            run_record_id: None,
            session_record_ids: None,
            count: None,
            matched: None,
            returned: None,
            limit: None,
        }
    }

    /// Deterministic sort key: code, then the session handle, then the run
    /// handle. Every payload variant is distinguished by one of the three.
    fn sort_key(&self) -> (&'static str, &str, &str) {
        (
            self.code,
            self.session_record_id.as_deref().unwrap_or(""),
            self.run_record_id.as_deref().unwrap_or(""),
        )
    }
}

/// The full digest: ordered rows plus sorted diagnostics.
///
/// The repository identity and the standing disclaimer live on the caller's
/// envelope (CLI or daemon), so both transports serialize these two fields
/// from the same value and cannot drift.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct SessionsDigest {
    /// Session rows, ordered by last activity descending (absent last), then
    /// session record ID ascending, truncated to the requested limit.
    pub sessions: Vec<SessionRow>,
    /// Diagnostics, sorted by `(code, session_record_id, run_record_id)`.
    pub diagnostics: Vec<SessionsDiagnostic>,
}

// ---------------------------------------------------------------------------
// Internal working types
// ---------------------------------------------------------------------------

/// A citation carried by a node, from either representation.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct Citation<'a> {
    /// The wire relation string.
    relation: &'a str,
    /// The cited record ID.
    target: &'a str,
}

/// Per-session scope derivation result.
///
/// The basis set is tracked PER repository, never as one flat set: a session
/// code-citing repo A and reaching repo B only through a referenced task must
/// not report `code_citation` in repo B's digest.
#[derive(Debug, Default)]
struct SessionScope {
    /// Repository record ID → the bases (`code_citation` / `task_reference`)
    /// that put THAT repository in scope.
    by_repository: BTreeMap<String, BTreeSet<&'static str>>,
}

impl SessionScope {
    /// Records `basis` as a reason `repository` is in scope.
    fn attribute(&mut self, repository: &str, basis: &'static str) {
        self.by_repository
            .entry(repository.to_owned())
            .or_default()
            .insert(basis);
    }

    /// The union of every per-repository basis, sorted ascending.
    fn union_bases(&self) -> Vec<&'static str> {
        let union: BTreeSet<&'static str> = self
            .by_repository
            .values()
            .flat_map(|bases| bases.iter().copied())
            .collect();
        union.into_iter().collect()
    }
}

/// One built row travelling with its ordering instant and its own row-scoped
/// diagnostics, so both survive (or vanish) exactly with the row.
///
/// Row-scoped diagnostics are flattened out of the SURVIVING rows only, after
/// sort and truncation: a diagnostic naming a session or run that `--limit`
/// dropped would cite a record the answer does not contain.
struct PendingRow {
    row: SessionRow,
    /// The ORIGINAL parsed maximum activity instant. Ordering reads this, never
    /// the rendered `last_activity` string, so no rendering choice can collapse
    /// two distinct instants into an artificial tie.
    order_instant: Option<DateTime<Utc>>,
    diagnostics: Vec<SessionsDiagnostic>,
}

// ---------------------------------------------------------------------------
// Pure core
// ---------------------------------------------------------------------------

/// Builds the repo-scoped session digest.
///
/// `records` is an append-ordered slice; `index` must be built from the SAME
/// slice so `owner_of` attributions line up. `repository_id` is an already
/// resolved repository record ID (selector resolution is the caller's job).
/// `limit` is assumed pre-validated against `1..=SESSIONS_MAX_LIMIT`
/// (see [`SESSIONS_MAX_LIMIT`]) — except that the daemon may pass a SMALLER
/// effective cap after folding in the common `budget.max_results` contract;
/// any value simply truncates the ordered rows, with `results_truncated`
/// reporting the cap that actually applied.
///
/// A session belongs to this digest exactly when `repository_id` is in the set
/// of repositories its members cite. Sessions with no derivable repository at
/// all are excluded from every digest and reported once under
/// `unresolved_repository_scope`, never silently dropped.
#[must_use]
pub fn sessions_for_repo(
    records: &[GraphRecord],
    index: &RepositoryIndex,
    repository_id: &str,
    limit: usize,
) -> SessionsDigest {
    let liveness = Liveness::new(records);
    let nodes = live_nodes(records, &liveness);
    let adjacency = Adjacency::build(records, &liveness, &nodes);

    // Envelope-level diagnostics (never row-scoped): they describe the digest
    // as a whole and survive truncation unchanged.
    let mut diagnostics: Vec<SessionsDiagnostic> = Vec::new();
    // Capped at MAX_UNRESOLVED_SESSION_IDS DURING the loop below (never grows
    // past it), with `unresolved_total` tracking the TRUE count separately —
    // so a store with millions of pre-linking sessions costs O(1) extra
    // memory per session, not O(total unresolved). `nodes` is a `BTreeMap`,
    // so this loop already visits `session_id` in ascending order: the list
    // is sorted BY CONSTRUCTION and needs no separate sort pass.
    let mut unresolved: Vec<String> = Vec::new();
    let mut unresolved_total: u64 = 0;
    // Bounded top-`limit` retention: a max-heap capped at `limit` entries, so
    // the digest never holds more than `limit` fully-materialized rows (each
    // carrying cloned handles, runs, tasks, and diagnostics) at once,
    // regardless of how many sessions this repository actually matches.
    // `matched` tracks the TRUE total separately, so `results_truncated`
    // still reports an honest count even though `kept` never grows past
    // `limit`.
    let mut kept: BinaryHeap<HeapEntry> = BinaryHeap::new();
    let mut matched: u64 = 0;
    // (session record id, session_id string, member ids) for EVERY live
    // AgentSession in the WHOLE STORE, regardless of which repository (if
    // any) it resolves to — feeds the sibling-linkage union below, since a
    // stamp-sharing sibling scoped to a DIFFERENT repository, or to none at
    // all, still has REAL edge-linked members that must count as linked.
    let mut all_session_members: Vec<(&str, &str, BTreeSet<&str>)> = Vec::new();
    // Stamps actually represented among THIS digest's scoped rows — the
    // unlinked-stamped-record diagnostic below is scoped to these, never to
    // every stamp in the store.
    let mut scoped_stamps: BTreeSet<&str> = BTreeSet::new();

    for (&session_id, session_record) in &nodes {
        if node_kind(session_record) != Some(NodeKind::AgentSession) {
            continue;
        }
        let members = members_of(session_id, &adjacency);
        let stamped = node_session_id(session_record).filter(|s| !s.is_empty());
        if let Some(stamped) = stamped {
            all_session_members.push((session_id, stamped, members.clone()));
        }

        let scope = derive_scope(session_id, &members, &adjacency, &nodes, index);

        if scope.by_repository.is_empty() {
            unresolved_total += 1;
            if unresolved.len() < MAX_UNRESOLVED_SESSION_IDS {
                unresolved.push(session_id.to_owned());
            }
            continue;
        }
        if !scope.by_repository.contains_key(repository_id) {
            continue;
        }

        matched += 1;
        if let Some(stamped) = stamped {
            scoped_stamps.insert(stamped);
        }

        // A `limit` of 0 (e.g. a daemon caller's `budget.max_results: 0`)
        // discards every row regardless, so skip building one entirely —
        // no point paying the allocation for a row that can never survive.
        if limit == 0 {
            continue;
        }
        let built = build_row(
            session_id,
            session_record,
            &members,
            &scope,
            &adjacency,
            &nodes,
        );
        let entry = HeapEntry(built);
        if kept.len() < limit {
            kept.push(entry);
        } else if kept.peek().is_some_and(|worst| entry < *worst) {
            kept.pop();
            kept.push(entry);
        }
        // Otherwise `entry` is worse than every currently-kept row and is
        // dropped here, immediately, rather than retained until a final
        // truncation pass.
    }

    if unresolved_total > 0 {
        let mut diagnostic = SessionsDiagnostic::bare("unresolved_repository_scope");
        let returned = unresolved.len() as u64;
        diagnostic.session_record_ids = Some(unresolved);
        diagnostic.count = Some(unresolved_total);
        if returned < unresolved_total {
            diagnostic.returned = Some(returned);
            diagnostic.limit = Some(MAX_UNRESOLVED_SESSION_IDS as u64);
        }
        diagnostics.push(diagnostic);
    }

    // Records stamped with a scoped session's `session_id` but reachable by no
    // edge path: reported once, never counted as members (issue #112 AC:
    // membership is edge-derived only).
    let unlinked = unlinked_stamped_records(&scoped_stamps, &all_session_members, &nodes);
    if unlinked > 0 {
        let mut diagnostic = SessionsDiagnostic::bare("unlinked_session_stamped_records");
        diagnostic.count = Some(unlinked);
        diagnostics.push(diagnostic);
    }

    // `kept` already holds at most `limit` entries (bounded during the loop
    // above), so this sort is over `limit` rows, never `matched` rows.
    let mut pending: Vec<PendingRow> = kept.into_iter().map(|entry| entry.0).collect();
    pending.sort_by(|a, b| pending_order_key(a).cmp(&pending_order_key(b)));

    if matched > pending.len() as u64 {
        let mut diagnostic = SessionsDiagnostic::bare("results_truncated");
        diagnostic.matched = Some(matched);
        diagnostic.returned = Some(pending.len() as u64);
        diagnostic.limit = Some(limit as u64);
        diagnostics.push(diagnostic);
    }

    // Row-scoped diagnostics are harvested from the SURVIVING rows only, so no
    // diagnostic ever names a session or run truncation dropped.
    let mut rows: Vec<SessionRow> = Vec::with_capacity(pending.len());
    for entry in pending {
        diagnostics.extend(entry.diagnostics);
        rows.push(entry.row);
    }

    // `matched == 0`, never `rows.is_empty()`: a zero `limit` (e.g. a daemon
    // caller's `budget.max_results: 0`) truncates every matched row away, and
    // that is a `results_truncated` answer, not "this repository has no
    // sessions" — the two diagnostics must never both describe the same
    // zero-row response for different reasons.
    if matched == 0 {
        diagnostics.push(SessionsDiagnostic::bare("no_sessions"));
    }

    diagnostics.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
    diagnostics.dedup();

    SessionsDigest {
        sessions: rows,
        diagnostics,
    }
}

/// Ordering key: last activity DESCENDING with absent last, then session
/// record ID ascending. `Reverse` on an `Option<DateTime>` would sort `None`
/// FIRST, so the key encodes "has a time" explicitly.
///
/// The instant is the ORIGINAL parsed value carried on the [`PendingRow`],
/// never a re-parse of the rendered `last_activity` string: rendering is a
/// presentation choice and must not be able to introduce ordering ties.
/// Second and sub-second parts are compared separately so no integer
/// conversion can overflow or round.
fn pending_order_key(pending: &PendingRow) -> (bool, std::cmp::Reverse<(i64, u32)>, &str) {
    let instant = pending
        .order_instant
        .map(|dt| (dt.timestamp(), dt.timestamp_subsec_nanos()));
    (
        instant.is_none(),
        std::cmp::Reverse(instant.unwrap_or((i64::MIN, 0))),
        pending.row.session_record_id.as_str(),
    )
}

/// A [`PendingRow`] ordered by [`pending_order_key`], for the bounded
/// top-`limit` retention in [`sessions_for_repo`].
///
/// A max-heap of `HeapEntry` therefore always surfaces the WORST (least
/// recently active) currently-kept row at its peak — the one to evict when a
/// better row arrives — while the best rows sink to the bottom of the heap's
/// internal order. This is the opposite of `pending_order_key`'s own
/// ascending "best first" meaning; `HeapEntry`'s `Ord` is deliberately the
/// same comparison, relied on for `BinaryHeap`'s max-at-top behavior.
struct HeapEntry(PendingRow);

impl HeapEntry {
    fn key(&self) -> (bool, std::cmp::Reverse<(i64, u32)>, &str) {
        pending_order_key(&self.0)
    }
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.key() == other.key()
    }
}

impl Eq for HeapEntry {}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.key().cmp(&other.key())
    }
}

/// Latest live node write per stable record ID.
///
/// Later physical writes of one ID supersede earlier ones (matching the
/// embedded current-state read), and ids whose most recent write is a tombstone
/// are dropped entirely — so a re-ingested duplicate line can never inflate a
/// count and a tombstoned session never appears.
fn live_nodes<'a>(
    records: &'a [GraphRecord],
    liveness: &Liveness<'a>,
) -> BTreeMap<&'a str, &'a GraphRecord> {
    let mut nodes: BTreeMap<&'a str, &'a GraphRecord> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node { id, .. } = record {
            if liveness.deleted(id.as_str()) {
                continue;
            }
            nodes.insert(id.as_str(), record);
        }
    }
    nodes
}

/// Edge-derived adjacency over live records, latest edge version per ID.
struct Adjacency<'a> {
    /// `AUTHORED_BY` target → sources.
    authored_by_predecessors: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// `SESSION_OF` target → sources.
    session_of_predecessors: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// `SESSION_OF` source → targets (a session's own agent handle).
    session_of_successors: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// Node → every citation it carries, from graph edges AND on-node
    /// `evidence_links`. Both representations are authoritative.
    citations: BTreeMap<&'a str, BTreeSet<Citation<'a>>>,
}

impl<'a> Adjacency<'a> {
    /// Builds the adjacency in one pass over the record slice.
    fn build(
        records: &'a [GraphRecord],
        liveness: &Liveness<'a>,
        nodes: &BTreeMap<&'a str, &'a GraphRecord>,
    ) -> Self {
        let mut authored_by_predecessors: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
        let mut session_of_predecessors: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
        let mut session_of_successors: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
        let mut citations: BTreeMap<&'a str, BTreeSet<Citation<'a>>> = BTreeMap::new();

        for (position, record) in records.iter().enumerate() {
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
            if liveness.deleted(id.as_str()) || !liveness.is_latest_edge_version(id, position) {
                continue;
            }
            // A live edge into a dead endpoint is not an adjacency: it would
            // resurrect a tombstoned session through the back door.
            if !nodes.contains_key(source.as_str()) || !nodes.contains_key(target.as_str()) {
                continue;
            }
            match label {
                EdgeLabel::AuthoredBy => {
                    authored_by_predecessors
                        .entry(target.as_str())
                        .or_default()
                        .insert(source.as_str());
                }
                EdgeLabel::SessionOf => {
                    session_of_predecessors
                        .entry(target.as_str())
                        .or_default()
                        .insert(source.as_str());
                    session_of_successors
                        .entry(source.as_str())
                        .or_default()
                        .insert(target.as_str());
                }
                _ => {}
            }
            // Only relations `cited_targets` is ever asked about are indexed:
            // code-graph topology would otherwise dominate the map on any real
            // store while never being returnable.
            if CITATION_RELATIONS.contains(label) {
                citations
                    .entry(source.as_str())
                    .or_default()
                    .insert(Citation {
                        relation: label.as_str(),
                        target: target.as_str(),
                    });
            }
        }

        // On-node evidence links are the second citation representation: a
        // record may carry its only code citation there, with no Edge record.
        for (&id, record) in nodes {
            let Some(links) = record.evidence_links() else {
                continue;
            };
            for link in links {
                let Some(target) = link.target_record_id.as_deref() else {
                    continue;
                };
                if !nodes.contains_key(target) {
                    continue;
                }
                // Same filter as the edge pass, applied through the wire-string
                // mapping `cited_targets` itself uses.
                if !EdgeLabel::from_relation(link.relation.as_str())
                    .is_some_and(|label| CITATION_RELATIONS.contains(&label))
                {
                    continue;
                }
                citations.entry(id).or_default().insert(Citation {
                    relation: link.relation.as_str(),
                    target,
                });
            }
        }

        Self {
            authored_by_predecessors,
            session_of_predecessors,
            session_of_successors,
            citations,
        }
    }

    /// Every node that points at `id` through `AUTHORED_BY` or `SESSION_OF`.
    fn predecessors(&self, id: &str) -> BTreeSet<&'a str> {
        let mut out: BTreeSet<&'a str> = BTreeSet::new();
        if let Some(sources) = self.authored_by_predecessors.get(id) {
            out.extend(sources.iter().copied());
        }
        if let Some(sources) = self.session_of_predecessors.get(id) {
            out.extend(sources.iter().copied());
        }
        out
    }

    /// Citations carried by `id` whose relation is one of `relations`.
    fn cited_targets(&self, id: &str, relations: &[EdgeLabel]) -> BTreeSet<&'a str> {
        self.citations.get(id).map_or_else(BTreeSet::new, |set| {
            set.iter()
                .filter(|citation| {
                    EdgeLabel::from_relation(citation.relation)
                        .is_some_and(|label| relations.contains(&label))
                })
                .map(|citation| citation.target)
                .collect()
        })
    }
}

/// Every record belonging to session `session_id`, edge-derived only.
///
/// Walks backwards from the session over `AUTHORED_BY` / `SESSION_OF` for at
/// most [`MEMBERSHIP_MAX_HOPS`] hops, which covers the deepest recorded chain
/// `X -AUTHORED_BY-> U -AUTHORED_BY-> R -SESSION_OF-> S`. The session itself is
/// never a member of itself.
fn members_of<'a>(session_id: &'a str, adjacency: &Adjacency<'a>) -> BTreeSet<&'a str> {
    let mut members: BTreeSet<&'a str> = BTreeSet::new();
    let mut frontier: BTreeSet<&'a str> = BTreeSet::from([session_id]);
    for _ in 0..MEMBERSHIP_MAX_HOPS {
        let mut next: BTreeSet<&'a str> = BTreeSet::new();
        for node in &frontier {
            for predecessor in adjacency.predecessors(node) {
                if predecessor == session_id || !members.insert(predecessor) {
                    continue;
                }
                next.insert(predecessor);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    members
}

/// Derives the set of repositories a session resolves to, with the basis for
/// each. Never name- or path-matches, and never walks past the first code hop.
fn derive_scope(
    session_id: &str,
    members: &BTreeSet<&str>,
    adjacency: &Adjacency<'_>,
    nodes: &BTreeMap<&str, &GraphRecord>,
    index: &RepositoryIndex,
) -> SessionScope {
    let mut scope = SessionScope::default();
    for citer in std::iter::once(&session_id).chain(members.iter()) {
        for target in adjacency.cited_targets(citer, CODE_CITATION_RELATIONS) {
            if let Some(repository) = index.owner_of(target) {
                scope.attribute(repository, "code_citation");
            }
        }
        // Task-mediated scope: exactly one hop through the referenced task to
        // the code it names. A task referencing another task is NOT followed.
        for task_id in adjacency.cited_targets(citer, &[EdgeLabel::ReferencesTask]) {
            if nodes.get(task_id).and_then(|r| node_kind(r)) != Some(NodeKind::Task) {
                continue;
            }
            for target in adjacency.cited_targets(task_id, TASK_CODE_RELATIONS) {
                if let Some(repository) = index.owner_of(target) {
                    scope.attribute(repository, "task_reference");
                }
            }
        }
    }
    scope
}

/// Builds one row plus its ordering instant and the row-scoped diagnostics it
/// raises (all three travel together as a [`PendingRow`]).
fn build_row(
    session_id: &str,
    session_record: &GraphRecord,
    members: &BTreeSet<&str>,
    scope: &SessionScope,
    adjacency: &Adjacency<'_>,
    nodes: &BTreeMap<&str, &GraphRecord>,
) -> PendingRow {
    let mut diagnostics: Vec<SessionsDiagnostic> = Vec::new();

    // ── Time bounds ─────────────────────────────────────────────────────────
    let mut observed: Vec<DateTime<Utc>> = Vec::new();
    let mut ingested: Vec<DateTime<Utc>> = Vec::new();
    let mut unparseable: u64 = 0;
    for id in std::iter::once(&session_id).chain(members.iter()) {
        let Some(record) = nodes.get(id) else {
            continue;
        };
        let GraphRecord::Node {
            observed_at,
            ingested_at,
            ..
        } = record
        else {
            continue;
        };
        if let Some(raw) = observed_at.as_deref().filter(|s| !s.is_empty()) {
            match parse_instant(raw) {
                Some(instant) => observed.push(instant),
                None => unparseable += 1,
            }
        }
        if let Some(raw) = ingested_at.as_deref().filter(|s| !s.is_empty()) {
            // An unparseable `ingested_at` is counted in the SAME per-session
            // tally as `observed_at`: a timestamp field that could not be read
            // is a reported gap either way, never a silent drop.
            match parse_instant(raw) {
                Some(instant) => ingested.push(instant),
                None => unparseable += 1,
            }
        }
    }
    if unparseable > 0 {
        let mut diagnostic = SessionsDiagnostic::bare("unparseable_timestamp");
        diagnostic.session_record_id = Some(session_id.to_owned());
        diagnostic.count = Some(unparseable);
        diagnostics.push(diagnostic);
    }
    let time_source_count = observed.len() as u64;
    // The ordering instant is the ORIGINAL parsed maximum, kept alongside the
    // rendered bound so ordering never depends on the rendering.
    let order_instant = observed.iter().max().copied();
    let (first_activity, last_activity) = bounds(&observed);
    let (first_ingested_at, last_ingested_at) = bounds(&ingested);

    // ── Runs ────────────────────────────────────────────────────────────────
    // Each run travels with its parsed instant so ordering is by instant, never
    // by the rendered string.
    let mut runs: Vec<(Option<DateTime<Utc>>, RunRow)> = Vec::new();
    for id in members {
        let Some(record) = nodes.get(id) else {
            continue;
        };
        if node_kind(record) != Some(NodeKind::AgentRun) {
            continue;
        }
        let GraphRecord::Node {
            summary,
            observed_at,
            ..
        } = record
        else {
            continue;
        };
        let parsed = parse_run_outcome(summary);
        if parsed.is_none() && claims_outcome(summary) {
            // The summary CLAIMS an outcome but is not enum-shaped: report the
            // defect by run handle. A summary that records no outcome at all is
            // simply `outcome_unrecorded` — there is nothing malformed to name,
            // and the raw bytes never reach the diagnostic either way.
            let mut diagnostic = SessionsDiagnostic::bare("outcome_not_enum_shaped");
            diagnostic.run_record_id = Some((*id).to_owned());
            diagnostics.push(diagnostic);
        }
        let (outcome, exit_reason) = parsed.map_or((None, None), |(o, e)| (Some(o), Some(e)));
        // The stored `observed_at` is never forwarded verbatim: it is parsed
        // and re-rendered, so an unparseable or crafted value cannot leak.
        let instant = observed_at
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(parse_instant);
        runs.push((
            instant,
            RunRow {
                run_record_id: (*id).to_owned(),
                outcome,
                exit_reason,
                observed_at: instant.map(render_instant),
            },
        ));
    }
    runs.sort_by(|a, b| run_order_key(a).cmp(&run_order_key(b)));
    let mut runs: Vec<RunRow> = runs.into_iter().map(|(_, run)| run).collect();
    let run_status = match runs.as_slice() {
        [] => "run_absent",
        [single] => {
            if single.outcome.is_some() {
                "outcome_recorded"
            } else {
                "outcome_unrecorded"
            }
        }
        _ => "multiple_runs",
    };
    if runs.len() > MAX_RUNS_PER_SESSION {
        let matched = runs.len();
        runs.truncate(MAX_RUNS_PER_SESSION);
        let mut diagnostic = SessionsDiagnostic::bare("runs_truncated");
        diagnostic.session_record_id = Some(session_id.to_owned());
        diagnostic.matched = Some(matched as u64);
        diagnostic.returned = Some(runs.len() as u64);
        diagnostic.limit = Some(MAX_RUNS_PER_SESSION as u64);
        diagnostics.push(diagnostic);
    }

    // ── Referenced tasks ────────────────────────────────────────────────────
    let mut task_ids: BTreeSet<&str> = BTreeSet::new();
    for id in std::iter::once(&session_id).chain(members.iter()) {
        for task_id in adjacency.cited_targets(id, &[EdgeLabel::ReferencesTask]) {
            if nodes.get(task_id).and_then(|r| node_kind(r)) == Some(NodeKind::Task) {
                task_ids.insert(task_id);
            }
        }
    }
    let mut tasks: Vec<TaskRef> = task_ids
        .iter()
        .map(|task_id| {
            let recorded = nodes
                .get(task_id)
                .and_then(|record| match record {
                    GraphRecord::Node { status, .. } => status.as_deref(),
                    _ => None,
                })
                .filter(|status| valid_task_status(status));
            TaskRef {
                record_id: (*task_id).to_owned(),
                status: recorded.unwrap_or("unknown").to_owned(),
                status_recorded: recorded.is_some(),
                trust_class: TASK_TRUST_CLASS,
            }
        })
        .collect();
    if tasks.len() > MAX_TASKS_PER_SESSION {
        let matched = tasks.len();
        tasks.truncate(MAX_TASKS_PER_SESSION);
        let mut diagnostic = SessionsDiagnostic::bare("tasks_truncated");
        diagnostic.session_record_id = Some(session_id.to_owned());
        diagnostic.matched = Some(matched as u64);
        diagnostic.returned = Some(tasks.len() as u64);
        diagnostic.limit = Some(MAX_TASKS_PER_SESSION as u64);
        diagnostics.push(diagnostic);
    }

    // ── Counts ──────────────────────────────────────────────────────────────
    let mut counts = SessionCounts {
        observation: 0,
        decision: 0,
        failure: 0,
        lesson: None,
    };
    for id in members {
        match nodes.get(id).and_then(|r| node_kind(r)) {
            Some(NodeKind::Observation) => counts.observation += 1,
            Some(NodeKind::Decision) => counts.decision += 1,
            Some(NodeKind::Failure) => counts.failure += 1,
            _ => {}
        }
    }

    // ── Handles ─────────────────────────────────────────────────────────────
    let agent_record_id = adjacency
        .session_of_successors
        .get(session_id)
        .and_then(|targets| {
            targets.iter().find(|target| {
                nodes.get(*target).and_then(|r| node_kind(r)) == Some(NodeKind::Agent)
            })
        })
        .map(|target| (*target).to_owned());
    let (summary_label, summary_hash) = safe_session_summary(session_record);

    let row = SessionRow {
        session_record_id: session_id.to_owned(),
        trust_class: SESSION_TRUST_CLASS,
        agent_record_id,
        agent_id: node_agent_id(session_record).map(str::to_owned),
        session_id: node_session_id(session_record).map(str::to_owned),
        summary_label,
        summary_hash,
        first_activity,
        last_activity,
        first_ingested_at,
        last_ingested_at,
        time_basis: if time_source_count == 0 {
            "absent"
        } else {
            "derived_from_member_observed_at"
        },
        time_source_count,
        repository_scope: scope.by_repository.keys().cloned().collect(),
        scope_basis: scope.union_bases(),
        scope_basis_by_repository: scope
            .by_repository
            .iter()
            .map(|(repository, bases)| (repository.clone(), bases.iter().copied().collect()))
            .collect(),
        run_status,
        runs,
        tasks,
        aggregation_scope: WHOLE_SESSION_AGGREGATION,
        record_counts: counts,
    };
    PendingRow {
        row,
        order_instant,
        diagnostics,
    }
}

/// Ordering key for a session's runs: `observed_at` ascending with absent (or
/// unparseable) last, then run record ID ascending.
///
/// Reads the PARSED instant carried alongside the row, never a re-parse of the
/// rendered string. Second and sub-second parts compare separately so no
/// integer conversion can overflow or round two distinct instants together.
fn run_order_key(run: &(Option<DateTime<Utc>>, RunRow)) -> (bool, (i64, u32), &str) {
    let instant = run
        .0
        .map(|dt| (dt.timestamp(), dt.timestamp_subsec_nanos()));
    (
        instant.is_none(),
        instant.unwrap_or((i64::MAX, u32::MAX)),
        run.1.run_record_id.as_str(),
    )
}

/// Minimum and maximum of a parsed instant set, re-rendered as RFC 3339 UTC.
fn bounds(instants: &[DateTime<Utc>]) -> (Option<String>, Option<String>) {
    let min = instants.iter().min().copied();
    let max = instants.iter().max().copied();
    (min.map(render_instant), max.map(render_instant))
}

/// Renders an instant in the Z-normalized RFC 3339 form the graph records use.
///
/// `AutoSi` keeps whatever sub-second precision the source carried (importers
/// emit millisecond timestamps — see `crate::codex::sanitize_rfc3339`) while
/// rendering a whole-second value with no fractional part at all, exactly as
/// `SecondsFormat::Secs` did. Truncating to seconds would report two distinct
/// instants as the same string.
fn render_instant(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

/// Parses an RFC 3339 timestamp into a UTC instant. All comparisons in this
/// lane go through here, never raw string order.
fn parse_instant(rfc3339: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(rfc3339)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Exact producer template a parseable run summary must match.
const RUN_OUTCOME_PREFIX: &str = "AgentRun outcome=";

/// Separator between the outcome and exit-reason tokens.
const RUN_EXIT_REASON_SEPARATOR: &str = " exit_reason=";

/// True when a run summary CLAIMS to carry an outcome (it opens with the
/// producer template's prefix), whether or not the rest parses.
fn claims_outcome(summary: &str) -> bool {
    summary.starts_with(RUN_OUTCOME_PREFIX)
}

/// Parses `AgentRun outcome=<X> exit_reason=<Y>` and nothing else.
///
/// Both tokens must be non-empty, at most 64 characters, and drawn from
/// `[A-Za-z0-9_.:-]`. Anything outside that shape — extra bytes, whitespace,
/// control characters, a missing separator — yields `None` rather than a
/// guessed outcome, so free-form summary text can never be reported as an enum
/// value (and never reaches the answer at all).
fn parse_run_outcome(summary: &str) -> Option<(String, String)> {
    let rest = summary.strip_prefix(RUN_OUTCOME_PREFIX)?;
    let (outcome, exit_reason) = rest.split_once(RUN_EXIT_REASON_SEPARATOR)?;
    if !is_enum_token(outcome) || !is_enum_token(exit_reason) {
        return None;
    }
    Some((outcome.to_owned(), exit_reason.to_owned()))
}

/// Closed charset gate for an outcome/exit-reason token.
fn is_enum_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= 64
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// Redaction-safe `(label, hash)` for an `AgentSession` summary.
///
/// Mirrors the agent-authored branch of the CLI's `safe_summary` helper
/// (`src/cli/output.rs`) — a structured label built from typed fields plus a
/// BLAKE3 handle over the stored bytes — but is computed HERE so the CLI
/// envelope and the daemon verb serialize the identical value. Session
/// summaries are producer-templated today, yet nothing in the schema stops an
/// importer from embedding free text, so the raw summary is never forwarded.
///
/// This function CANNOT delegate to `cli::output::safe_summary`: that helper
/// takes its safe branch only when `cli::trust_class_for` returns
/// `agent_authored`, and `AgentSession` is not in that match arm — it falls to
/// the `_ => "other"` arm and therefore to `safe_summary`'s ELSE branch, which
/// returns the RAW stored summary with no hash. Delegating would leak exactly
/// the bytes this lane exists to withhold, so the duplication here is a safety
/// requirement, not an oversight. (A test in this module pins the trap.)
///
/// The interpolated `who` component is capped at [`SESSIONS_FIELD_MAX_CHARS`]:
/// `agent_id` / `session_id` are unbounded importer-supplied strings, and the
/// label is a CORE value, so capping here bounds both transports identically.
fn safe_session_summary(record: &GraphRecord) -> (String, Option<String>) {
    let GraphRecord::Node {
        kind,
        summary,
        agent_id,
        session_id,
        ..
    } = record
    else {
        return (String::new(), None);
    };
    let who = match (agent_id.as_deref(), session_id.as_deref()) {
        (Some(agent), Some(session)) => format!("{agent}:{session}"),
        (Some(agent), None) => agent.to_owned(),
        _ => "unknown".to_owned(),
    };
    (
        format!("{} by {}", kind.as_str(), capped_field(&who)),
        Some(format!(
            "blake3:{}",
            blake3::hash(summary.as_bytes()).to_hex()
        )),
    )
}

/// Caps a free-text value at [`SESSIONS_FIELD_MAX_CHARS`] on a CHARACTER
/// boundary, appending [`TRUNCATION_MARKER`] so the cut is visible.
///
/// Deterministic: same input, same output, always.
fn capped_field(value: &str) -> String {
    if value.chars().count() <= SESSIONS_FIELD_MAX_CHARS {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(SESSIONS_FIELD_MAX_CHARS).collect();
    out.push(TRUNCATION_MARKER);
    out
}

/// Sanitizes and caps one free-text session field for RENDERED text output.
///
/// Control characters (newline, carriage return, and the ESC that starts an
/// ANSI sequence) become [`CONTROL_REPLACEMENT`], so a crafted `agent_id` or
/// `session_id` can neither forge extra output lines nor drive a terminal; the
/// result is then capped by [`capped_field`]. JSON output does not need the
/// sanitize step (serde escapes control characters) and keeps the raw values,
/// matching the house precedent in `crate::query::changes`.
#[must_use]
pub fn bounded_session_text(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|c| {
            if c.is_control() {
                CONTROL_REPLACEMENT
            } else {
                c
            }
        })
        .collect();
    capped_field(&sanitized)
}

/// Counts distinct live records stamped with a scoped session's `session_id`
/// that no edge path reaches.
///
/// Indexed by stamped `session_id` in ONE pass over the nodes, then one lookup
/// per scoped session. The naive shape — rescanning every node for every scoped
/// session — is `O(sessions × nodes)` and takes tens of seconds on a store with
/// a few thousand sessions.
fn unlinked_stamped_records<'a>(
    scoped_stamps: &BTreeSet<&'a str>,
    all_session_members: &[(&'a str, &'a str, BTreeSet<&'a str>)],
    nodes: &BTreeMap<&'a str, &'a GraphRecord>,
) -> u64 {
    let mut by_stamped_session: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
    for (&id, &record) in nodes {
        if let Some(stamped) = node_session_id(record).filter(|s| !s.is_empty()) {
            by_stamped_session.entry(stamped).or_default().insert(id);
        }
    }

    // Two AgentSession NODE IDs can legitimately share one stamped
    // `session_id` STRING: `build_agent_session_node` hashes `observed_at`
    // into the node id, so re-observing the same logical session at a new
    // instant mints a sibling session node under the same stamp. The set of
    // records genuinely linked to a stamp is therefore the UNION across
    // EVERY LIVE session in the WHOLE STORE sharing it — each session's own
    // record ID plus its edge-derived members — regardless of which
    // repository (if any) that sibling itself resolves to. A sibling scoped
    // to a DIFFERENT repository, or to none at all, is still a REAL session
    // with REAL edge-linked members; checking only sessions scoped to the
    // CURRENTLY queried repository would falsely flag such a sibling's
    // members as unlinked. Only the DIAGNOSTIC below is scoped, via
    // `scoped_stamps`, to stamps actually represented in this digest.
    let mut linked_by_stamped_session: BTreeMap<&'a str, BTreeSet<&'a str>> = BTreeMap::new();
    for &(session_record_id, stamped_session_id, ref members) in all_session_members {
        let linked = linked_by_stamped_session
            .entry(stamped_session_id)
            .or_default();
        linked.insert(session_record_id);
        linked.extend(members.iter().copied());
    }

    let mut unlinked: BTreeSet<&'a str> = BTreeSet::new();
    for &stamped_session_id in scoped_stamps {
        let Some(candidates) = by_stamped_session.get(stamped_session_id) else {
            continue;
        };
        let Some(linked) = linked_by_stamped_session.get(stamped_session_id) else {
            continue;
        };
        for &id in candidates {
            if linked.contains(id) {
                continue;
            }
            unlinked.insert(id);
        }
    }
    unlinked.len() as u64
}

/// Node kind of a record, or `None` for edges/tombstones.
const fn node_kind(record: &GraphRecord) -> Option<NodeKind> {
    match record {
        GraphRecord::Node { kind, .. } => Some(*kind),
        _ => None,
    }
}

/// Recorded `agent_id` string of a node record.
fn node_agent_id(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node { agent_id, .. } => agent_id.as_deref(),
        _ => None,
    }
}

/// Recorded `session_id` string of a node record.
fn node_session_id(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node { session_id, .. } => session_id.as_deref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        AGENT_MEMORY_SCHEMA_VERSION, IdentitySource, PROJECT_SCHEMA_VERSION,
        RepositoryIdentityPayload, SourceSpan, agent_memory_stable_id, project_stable_id,
        stable_id,
    };

    fn repository(display: &str) -> GraphRecord {
        GraphRecord::node(
            stable_id(&["repository", display]),
            NodeKind::Repository,
            None,
            None,
            Some(display.to_owned()),
            format!("Repository {display}"),
        )
        .with_repository_identity(RepositoryIdentityPayload {
            identity_source: IdentitySource::OperatorOverride,
            remote_url: None,
            root_commit_sha: None,
            canonical_path: None,
            basename: display.to_owned(),
        })
    }

    fn symbol(repo_id: &str, name: &str) -> GraphRecord {
        GraphRecord::syntax_node(
            stable_id(&["symbol", repo_id, name]),
            NodeKind::Symbol,
            format!("src/{name}.rs"),
            SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 1,
                end_line: 2,
            },
            name.to_owned(),
            "rust",
            format!("Symbol {name}"),
        )
    }

    fn memory(kind: NodeKind, key: &str, observed: Option<&str>, summary: &str) -> GraphRecord {
        let mut record = GraphRecord::node(
            agent_memory_stable_id(&["node", kind.as_str(), key]),
            kind,
            None,
            None,
            None,
            summary.to_owned(),
        );
        if let GraphRecord::Node {
            schema_version,
            agent_id,
            session_id,
            observed_at,
            ..
        } = &mut record
        {
            *schema_version = AGENT_MEMORY_SCHEMA_VERSION;
            *agent_id = Some("agent-1".to_owned());
            *session_id = Some(key.to_owned());
            *observed_at = observed.map(str::to_owned);
        }
        record
    }

    fn task(key: &str, status_value: &str) -> GraphRecord {
        let id = project_stable_id(&["task", key]);
        let mut record = GraphRecord::node(
            id,
            NodeKind::Task,
            None,
            None,
            Some(key.to_owned()),
            format!("Task {key}"),
        );
        if let GraphRecord::Node {
            schema_version,
            domain,
            status,
            ..
        } = &mut record
        {
            *schema_version = PROJECT_SCHEMA_VERSION;
            *domain = Some("project".to_owned());
            *status = Some(status_value.to_owned());
        }
        record
    }

    fn am_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::agent_memory_edge(
            label,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            format!("{} edge", label.as_str()),
        )
    }

    fn code_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            format!("{} edge", label.as_str()),
        )
    }

    fn digest(records: &[GraphRecord], repository_id: &str) -> SessionsDigest {
        let index = RepositoryIndex::build(records);
        sessions_for_repo(records, &index, repository_id, SESSIONS_DEFAULT_LIMIT)
    }

    #[test]
    fn membership_chain_is_capped_at_three_hops() {
        // O_far sits FOUR hops from the session and must not be a member:
        // O_far -> T2 -> T1 -> R -SESSION_OF-> S.
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let sym = symbol(&repo_id, "alpha");
        let sym_id = sym.id().to_owned();

        let session = memory(
            NodeKind::AgentSession,
            "s",
            Some("2026-01-01T00:00:00Z"),
            "S",
        );
        let run = memory(
            NodeKind::AgentRun,
            "r",
            Some("2026-01-01T00:01:00Z"),
            "AgentRun x",
        );
        let turn1 = memory(NodeKind::AgentTurn, "t1", None, "AgentTurn 1");
        let turn2 = memory(NodeKind::AgentTurn, "t2", None, "AgentTurn 2");
        let near = memory(
            NodeKind::Observation,
            "near",
            Some("2026-01-01T00:02:00Z"),
            "near",
        );
        let far = memory(
            NodeKind::Observation,
            "far",
            Some("2026-01-01T00:03:00Z"),
            "far",
        );
        let (session_id, run_id, turn1_id, turn2_id, near_id, far_id) = (
            session.id().to_owned(),
            run.id().to_owned(),
            turn1.id().to_owned(),
            turn2.id().to_owned(),
            near.id().to_owned(),
            far.id().to_owned(),
        );

        let contains = code_edge(EdgeLabel::Contains, &repo_id, &sym_id);
        let records = vec![
            repo,
            sym,
            contains,
            session,
            run,
            turn1,
            turn2,
            near,
            far,
            am_edge(EdgeLabel::SessionOf, &run_id, &session_id),
            am_edge(EdgeLabel::AuthoredBy, &turn1_id, &run_id),
            am_edge(EdgeLabel::AuthoredBy, &near_id, &turn1_id),
            am_edge(EdgeLabel::AuthoredBy, &turn2_id, &turn1_id),
            am_edge(EdgeLabel::AuthoredBy, &far_id, &turn2_id),
            am_edge(EdgeLabel::MentionsSymbol, &near_id, &sym_id),
        ];

        let result = digest(&records, &repo_id);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(
            result.sessions[0].record_counts.observation, 1,
            "only the three-hop observation is a member: {:?}",
            result.sessions[0]
        );
        assert_eq!(
            result.sessions[0].last_activity.as_deref(),
            Some("2026-01-01T00:02:00Z"),
            "the four-hop record must not contribute to the time bounds"
        );
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn scope_traversal_does_not_follow_task_to_task() {
        // O -REFERENCES_TASK-> TaskA -REFERENCES_TASK-> TaskB -MENTIONS_SYMBOL-> repo B.
        // Only repo A (via TaskA's own citation) may be in scope.
        let repo_a = repository("repo-a");
        let repo_b = repository("repo-b");
        let (repo_a_id, repo_b_id) = (repo_a.id().to_owned(), repo_b.id().to_owned());
        let sym_a = symbol(&repo_a_id, "alpha");
        let sym_b = symbol(&repo_b_id, "beta");
        let (sym_a_id, sym_b_id) = (sym_a.id().to_owned(), sym_b.id().to_owned());
        let contains_a = code_edge(EdgeLabel::Contains, &repo_a_id, &sym_a_id);
        let contains_b = code_edge(EdgeLabel::Contains, &repo_b_id, &sym_b_id);

        let task_a = task("task-a", "open");
        let task_b = task("task-b", "open");
        let (task_a_id, task_b_id) = (task_a.id().to_owned(), task_b.id().to_owned());

        let session = memory(
            NodeKind::AgentSession,
            "s",
            Some("2026-01-01T00:00:00Z"),
            "S",
        );
        let obs = memory(
            NodeKind::Observation,
            "o",
            Some("2026-01-01T00:01:00Z"),
            "o",
        );
        let (session_id, obs_id) = (session.id().to_owned(), obs.id().to_owned());

        let records = vec![
            repo_a,
            repo_b,
            sym_a,
            sym_b,
            contains_a,
            contains_b,
            task_a,
            task_b,
            session,
            obs,
            am_edge(EdgeLabel::AuthoredBy, &obs_id, &session_id),
            am_edge(EdgeLabel::ReferencesTask, &obs_id, &task_a_id),
            GraphRecord::project_edge(
                EdgeLabel::MentionsSymbol,
                task_a_id.clone(),
                sym_a_id,
                Some("1.0".to_owned()),
                "task mentions symbol".to_owned(),
            ),
            GraphRecord::project_edge(
                EdgeLabel::ReferencesTask,
                task_a_id,
                task_b_id.clone(),
                Some("1.0".to_owned()),
                "task references task".to_owned(),
            ),
            GraphRecord::project_edge(
                EdgeLabel::MentionsSymbol,
                task_b_id,
                sym_b_id,
                Some("1.0".to_owned()),
                "task mentions symbol".to_owned(),
            ),
        ];

        let result = digest(&records, &repo_a_id);
        assert_eq!(result.sessions.len(), 1);
        assert_eq!(
            result.sessions[0].repository_scope,
            vec![repo_a_id],
            "task→task traversal must never widen repository scope"
        );
        assert!(
            digest(&records, &repo_b_id).sessions.is_empty(),
            "repo B is reachable only through a second task hop and must not match"
        );
    }

    #[test]
    fn ordering_is_a_total_order_over_shuffled_input() {
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let sym = symbol(&repo_id, "alpha");
        let sym_id = sym.id().to_owned();
        let contains = code_edge(EdgeLabel::Contains, &repo_id, &sym_id);

        let mut records = vec![repo, sym, contains];
        // Two sessions tied on last activity plus one newer and one timeless.
        for (key, observed) in [
            ("s-tie-a", Some("2026-02-01T00:00:00Z")),
            ("s-tie-b", Some("2026-02-01T00:00:00Z")),
            ("s-new", Some("2026-03-01T00:00:00Z")),
            ("s-null", None),
        ] {
            let session = memory(NodeKind::AgentSession, key, observed, "S");
            let obs = memory(
                NodeKind::Observation,
                &format!("o-{key}"),
                observed,
                "observation",
            );
            let (session_id, obs_id) = (session.id().to_owned(), obs.id().to_owned());
            records.push(session);
            records.push(obs);
            records.push(am_edge(EdgeLabel::AuthoredBy, &obs_id, &session_id));
            records.push(am_edge(EdgeLabel::MentionsSymbol, &obs_id, &sym_id));
        }

        let forward = digest(&records, &repo_id);
        let mut shuffled = records.clone();
        shuffled.reverse();
        let reversed = digest(&shuffled, &repo_id);
        assert_eq!(
            forward, reversed,
            "row order must not depend on physical record order"
        );

        let ids: Vec<&str> = forward
            .sessions
            .iter()
            .map(|row| row.session_record_id.as_str())
            .collect();
        assert_eq!(ids.len(), 4);
        // Newest first, timeless last, tie broken on record ID ascending.
        assert_eq!(
            forward.sessions[0].last_activity.as_deref(),
            Some("2026-03-01T00:00:00Z")
        );
        assert!(forward.sessions[3].last_activity.is_none());
        assert!(
            forward.sessions[1].session_record_id < forward.sessions[2].session_record_id,
            "tied rows sort by session_record_id ascending: {ids:?}"
        );
    }

    #[test]
    fn sub_second_activity_differences_order_newest_first() {
        // Two sessions whose last activity differs ONLY in the fractional
        // second. Truncating the rendered bound to whole seconds (or re-parsing
        // a truncated render for the sort key) collapses them into an
        // artificial tie that falls through to the record-ID tie-break — so the
        // LATER instant is deliberately assigned to the session with the
        // LARGER record ID, where that fallback puts it SECOND.
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let sym = symbol(&repo_id, "alpha");
        let sym_id = sym.id().to_owned();
        let contains = code_edge(EdgeLabel::Contains, &repo_id, &sym_id);

        let id_a = agent_memory_stable_id(&["node", NodeKind::AgentSession.as_str(), "s-sub-a"]);
        let id_b = agent_memory_stable_id(&["node", NodeKind::AgentSession.as_str(), "s-sub-b"]);
        let (earlier_key, later_key) = if id_a < id_b {
            ("s-sub-a", "s-sub-b")
        } else {
            ("s-sub-b", "s-sub-a")
        };

        let mut records = vec![repo, sym, contains];
        for (key, observed) in [
            (earlier_key, "2026-02-01T00:00:00.100Z"),
            (later_key, "2026-02-01T00:00:00.900Z"),
        ] {
            let session = memory(NodeKind::AgentSession, key, Some(observed), "S");
            let obs = memory(
                NodeKind::Observation,
                &format!("o-{key}"),
                Some(observed),
                "observation",
            );
            let (session_id, obs_id) = (session.id().to_owned(), obs.id().to_owned());
            records.push(session);
            records.push(obs);
            records.push(am_edge(EdgeLabel::AuthoredBy, &obs_id, &session_id));
            records.push(am_edge(EdgeLabel::MentionsSymbol, &obs_id, &sym_id));
        }

        let result = digest(&records, &repo_id);
        assert_eq!(result.sessions.len(), 2);
        assert_eq!(
            result.sessions[0].last_activity.as_deref(),
            Some("2026-02-01T00:00:00.900Z"),
            "a sub-second-newer session must sort FIRST, and the bound must keep \
             its sub-second precision: {:?}",
            result.sessions
        );
        assert_eq!(
            result.sessions[1].last_activity.as_deref(),
            Some("2026-02-01T00:00:00.100Z")
        );
        // A whole-second instant still renders with no fractional part, so
        // existing fixtures are unaffected by the precision change.
        assert_eq!(
            render_instant(parse_instant("2026-02-01T00:00:00Z").expect("parses")),
            "2026-02-01T00:00:00Z"
        );
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn per_repository_scope_basis_is_not_a_flat_union() {
        // O -MENTIONS_SYMBOL-> repo A  (code_citation, repo A only)
        // O -REFERENCES_TASK-> Task -MENTIONS_SYMBOL-> repo B (task_reference,
        // repo B only). A flat union would claim `code_citation` in repo B's
        // digest, which is not a fact about repo B.
        let repo_a = repository("repo-a");
        let repo_b = repository("repo-b");
        let (repo_a_id, repo_b_id) = (repo_a.id().to_owned(), repo_b.id().to_owned());
        let sym_a = symbol(&repo_a_id, "alpha");
        let sym_b = symbol(&repo_b_id, "beta");
        let (sym_a_id, sym_b_id) = (sym_a.id().to_owned(), sym_b.id().to_owned());
        let contains_a = code_edge(EdgeLabel::Contains, &repo_a_id, &sym_a_id);
        let contains_b = code_edge(EdgeLabel::Contains, &repo_b_id, &sym_b_id);

        let task_b = task("task-b", "open");
        let task_b_id = task_b.id().to_owned();

        let session = memory(
            NodeKind::AgentSession,
            "s",
            Some("2026-01-01T00:00:00Z"),
            "S",
        );
        let obs = memory(
            NodeKind::Observation,
            "o",
            Some("2026-01-01T00:01:00Z"),
            "o",
        );
        let (session_id, obs_id) = (session.id().to_owned(), obs.id().to_owned());

        let records = vec![
            repo_a,
            repo_b,
            sym_a,
            sym_b,
            contains_a,
            contains_b,
            task_b,
            session,
            obs,
            am_edge(EdgeLabel::AuthoredBy, &obs_id, &session_id),
            am_edge(EdgeLabel::MentionsSymbol, &obs_id, &sym_a_id),
            am_edge(EdgeLabel::ReferencesTask, &obs_id, &task_b_id),
            GraphRecord::project_edge(
                EdgeLabel::MentionsSymbol,
                task_b_id,
                sym_b_id,
                Some("1.0".to_owned()),
                "task mentions symbol".to_owned(),
            ),
        ];

        for repository_id in [&repo_a_id, &repo_b_id] {
            let result = digest(&records, repository_id);
            assert_eq!(result.sessions.len(), 1, "the session is in both digests");
            let row = &result.sessions[0];
            assert_eq!(
                row.scope_basis_by_repository,
                BTreeMap::from([
                    (repo_a_id.clone(), vec!["code_citation"]),
                    (repo_b_id.clone(), vec!["task_reference"]),
                ]),
                "each repository must carry only the basis that put IT in scope: {row:?}"
            );
            assert_eq!(
                row.scope_basis,
                vec!["code_citation", "task_reference"],
                "scope_basis stays the sorted UNION: {row:?}"
            );
            assert_eq!(row.aggregation_scope, "whole_session");
        }
    }

    #[test]
    fn safe_session_summary_never_returns_the_raw_summary() {
        // The trap this guards: `cli::trust_class_for` classifies AgentSession
        // as "other", so delegating to `cli::output::safe_summary` would take
        // its ELSE branch and hand back the stored bytes with no hash.
        const RAW: &str = "AgentSession RAW_SUMMARY_SHOULD_NOT_LEAK";
        let record = memory(NodeKind::AgentSession, "s", None, RAW);
        let (label, hash) = safe_session_summary(&record);
        assert_eq!(
            label, "AgentSession by agent-1:s",
            "the label is synthesized from typed fields only"
        );
        assert!(
            !label.contains("RAW_SUMMARY_SHOULD_NOT_LEAK"),
            "the raw summary must never reach the label: {label}"
        );
        assert_eq!(
            hash,
            Some(format!("blake3:{}", blake3::hash(RAW.as_bytes()).to_hex())),
            "the stored summary is exposed ONLY as a BLAKE3 handle"
        );
    }

    #[test]
    fn outcome_charset_gate_rejects_control_and_whitespace() {
        assert_eq!(
            parse_run_outcome("AgentRun outcome=success exit_reason=completed"),
            Some(("success".to_owned(), "completed".to_owned()))
        );
        assert_eq!(
            parse_run_outcome("AgentRun outcome=exit.code:2-b exit_reason=timed_out"),
            Some(("exit.code:2-b".to_owned(), "timed_out".to_owned()))
        );
        for malformed in [
            "AgentRun outcome=succ ess\nLEAK exit_reason=x",
            "AgentRun outcome=succ\tess exit_reason=x",
            "AgentRun outcome=ok exit_reason=we ird",
            "AgentRun outcome= exit_reason=x",
            "AgentRun outcome=ok exit_reason=",
            "AgentRun outcome=ok exit_reason=x trailing",
            "AgentRun outcome=ok",
            "AgentRun claude-code",
            "",
            // A bare control byte (BEL) — never a token character, even though
            // it is neither whitespace nor a separator.
            "AgentRun outcome=o\u{7}k exit_reason=x",
            "AgentRun outcome=ok exit_reason=\u{7}",
            // Non-ASCII is outside the closed `[A-Za-z0-9_.:-]` charset.
            "AgentRun outcome=succès exit_reason=x",
            "AgentRun outcome=ok exit_reason=完了",
        ] {
            assert_eq!(
                parse_run_outcome(malformed),
                None,
                "must never guess an outcome from {malformed:?}"
            );
        }
        // A 64-character token is accepted; 65 is not.
        let ok = "a".repeat(64);
        let too_long = "a".repeat(65);
        assert!(parse_run_outcome(&format!("AgentRun outcome={ok} exit_reason=x")).is_some());
        assert!(parse_run_outcome(&format!("AgentRun outcome={too_long} exit_reason=x")).is_none());
    }

    #[test]
    fn zero_limit_truncates_without_a_no_sessions_diagnostic() {
        // A `limit` of zero is unreachable through the CLI (which validates
        // 1..=SESSIONS_MAX_LIMIT before ever calling this core), but the
        // daemon can pass one after folding in a caller's `budget.max_results:
        // 0`. The repository genuinely HAS a matching session, so the answer
        // must be `results_truncated`, never the false claim `no_sessions`.
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let sym = symbol(&repo_id, "alpha");
        let sym_id = sym.id().to_owned();
        let session = memory(
            NodeKind::AgentSession,
            "s",
            Some("2026-01-01T00:00:00Z"),
            "S",
        );
        let session_id = session.id().to_owned();
        let obs = memory(
            NodeKind::Observation,
            "o",
            Some("2026-01-01T00:00:00Z"),
            "O",
        );
        let obs_id = obs.id().to_owned();
        let records = vec![
            repo,
            sym,
            code_edge(EdgeLabel::Contains, &repo_id, &sym_id),
            session,
            obs,
            am_edge(EdgeLabel::AuthoredBy, &obs_id, &session_id),
            am_edge(EdgeLabel::MentionsSymbol, &obs_id, &sym_id),
        ];
        let index = RepositoryIndex::build(&records);
        let out = sessions_for_repo(&records, &index, &repo_id, 0);

        assert!(
            out.sessions.is_empty(),
            "a zero limit truncates every row: {out:?}"
        );
        let truncated = out
            .diagnostics
            .iter()
            .find(|d| d.code == "results_truncated")
            .unwrap_or_else(|| panic!("must disclose truncation: {out:?}"));
        assert_eq!(truncated.matched, Some(1));
        assert_eq!(truncated.returned, Some(0));
        assert_eq!(truncated.limit, Some(0));
        assert!(
            !out.diagnostics.iter().any(|d| d.code == "no_sessions"),
            "a truncated-to-zero repository with a real session must never \
             also claim no_sessions: {out:?}"
        );
    }

    #[test]
    fn limit_below_matched_count_retains_exactly_the_top_k_most_recent() {
        // `sessions_for_repo` retains only the best `limit` rows via a bounded
        // max-heap rather than materializing every matched row before
        // truncating (issue #112 review round 13: unbounded retention could
        // let the digest's memory scale with `matched` instead of `limit`).
        // Five sessions with distinct last-activity instants, `limit: 2`,
        // exercises repeated eviction (not just the single-swap case a
        // 2-session/limit-1 fixture would cover) — this proves the heap
        // converges on the correct top-K, not merely an order-preserving
        // subset of whichever rows happened to be built first.
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let sym = symbol(&repo_id, "alpha");
        let sym_id = sym.id().to_owned();
        let mut records = vec![repo, sym, code_edge(EdgeLabel::Contains, &repo_id, &sym_id)];
        let mut expected_order = Vec::new();
        for (key, observed) in [
            ("s-1", "2026-01-01T00:00:00Z"),
            ("s-2", "2026-02-01T00:00:00Z"),
            ("s-3", "2026-03-01T00:00:00Z"),
            ("s-4", "2026-04-01T00:00:00Z"),
            ("s-5", "2026-05-01T00:00:00Z"),
        ] {
            let session = memory(NodeKind::AgentSession, key, Some(observed), "S");
            let obs = memory(
                NodeKind::Observation,
                &format!("o-{key}"),
                Some(observed),
                "observation",
            );
            let (session_id, obs_id) = (session.id().to_owned(), obs.id().to_owned());
            expected_order.push(key.to_owned());
            records.push(session);
            records.push(obs);
            records.push(am_edge(EdgeLabel::AuthoredBy, &obs_id, &session_id));
            records.push(am_edge(EdgeLabel::MentionsSymbol, &obs_id, &sym_id));
        }
        expected_order.reverse(); // most-recent-first: s-5, s-4

        let index = RepositoryIndex::build(&records);
        let out = sessions_for_repo(&records, &index, &repo_id, 2);

        let ids: Vec<&str> = out
            .sessions
            .iter()
            .map(|s| s.session_id.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(
            ids,
            expected_order[..2],
            "the two MOST RECENT sessions must survive, in recency order: {out:?}"
        );
        let truncated = out
            .diagnostics
            .iter()
            .find(|d| d.code == "results_truncated")
            .unwrap_or_else(|| panic!("must disclose truncation: {out:?}"));
        assert_eq!(
            truncated.matched,
            Some(5),
            "the TRUE total, got {truncated:?}"
        );
        assert_eq!(truncated.returned, Some(2));
        assert_eq!(truncated.limit, Some(2));
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn unlinked_check_unions_memberships_across_sibling_sessions_sharing_one_session_id() {
        // Two distinct AgentSession NODE IDs sharing one stamped `session_id`
        // STRING — the normal shape `build_agent_session_node` produces when
        // the same logical session is re-observed at a different instant
        // (`observed_at` is a record-id input). Each sibling has its own real
        // AUTHORED_BY/MENTIONS_SYMBOL member; NEITHER member may be reported
        // as unlinked just because it is absent from the OTHER sibling's
        // membership set.
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let sym = symbol(&repo_id, "alpha");
        let sym_id = sym.id().to_owned();

        let stamp = |mut record: GraphRecord| -> GraphRecord {
            if let GraphRecord::Node { session_id, .. } = &mut record {
                *session_id = Some("shared-sess".to_owned());
            }
            record
        };

        let s_a = stamp(memory(
            NodeKind::AgentSession,
            "s-a",
            Some("2026-01-01T00:00:00Z"),
            "S_a",
        ));
        let s_a_id = s_a.id().to_owned();
        let s_b = stamp(memory(
            NodeKind::AgentSession,
            "s-b",
            Some("2026-01-01T01:00:00Z"),
            "S_b",
        ));
        let s_b_id = s_b.id().to_owned();
        let o_a = stamp(memory(
            NodeKind::Observation,
            "o-a",
            Some("2026-01-01T00:30:00Z"),
            "O_a",
        ));
        let o_a_id = o_a.id().to_owned();
        let o_b = stamp(memory(
            NodeKind::Observation,
            "o-b",
            Some("2026-01-01T01:30:00Z"),
            "O_b",
        ));
        let o_b_id = o_b.id().to_owned();

        let records = vec![
            repo,
            sym,
            code_edge(EdgeLabel::Contains, &repo_id, &sym_id),
            s_a,
            s_b,
            o_a,
            o_b,
            am_edge(EdgeLabel::AuthoredBy, &o_a_id, &s_a_id),
            am_edge(EdgeLabel::MentionsSymbol, &o_a_id, &sym_id),
            am_edge(EdgeLabel::AuthoredBy, &o_b_id, &s_b_id),
            am_edge(EdgeLabel::MentionsSymbol, &o_b_id, &sym_id),
        ];
        let index = RepositoryIndex::build(&records);
        let out = sessions_for_repo(&records, &index, &repo_id, SESSIONS_DEFAULT_LIMIT);

        assert_eq!(
            out.sessions.len(),
            2,
            "both sibling sessions must be scoped and returned: {out:?}"
        );
        assert!(
            !out.diagnostics
                .iter()
                .any(|d| d.code == "unlinked_session_stamped_records"),
            "each session's own edge-linked member must never be flagged \
             unlinked merely because it belongs to a SIBLING session sharing \
             the stamp: {out:?}"
        );
        for row in &out.sessions {
            assert_eq!(
                row.record_counts.observation, 1,
                "each sibling session must count its OWN observation only: {row:?}"
            );
        }
    }

    #[test]
    #[allow(clippy::similar_names)]
    fn unlinked_check_unions_across_siblings_scoped_to_different_repositories() {
        // Same stamped session_id, but S_a resolves to repo A and S_b to
        // repo B (a DIFFERENT repository, never appearing in repo A's
        // digest at all). Querying repo A must still recognize O_b as
        // genuinely linked (to S_b) — a sibling scoped to another repository
        // is a real session with real edge-linked members, not evidence of
        // an unlinked stamped record.
        let repo_a = repository("repo-a");
        let repo_a_id = repo_a.id().to_owned();
        let repo_b = repository("repo-b");
        let repo_b_id = repo_b.id().to_owned();
        let sym_a = symbol(&repo_a_id, "alpha");
        let sym_a_id = sym_a.id().to_owned();
        let sym_b = symbol(&repo_b_id, "beta");
        let sym_b_id = sym_b.id().to_owned();

        let stamp = |mut record: GraphRecord| -> GraphRecord {
            if let GraphRecord::Node { session_id, .. } = &mut record {
                *session_id = Some("shared-sess".to_owned());
            }
            record
        };

        let s_a = stamp(memory(
            NodeKind::AgentSession,
            "s-a",
            Some("2026-01-01T00:00:00Z"),
            "S_a",
        ));
        let s_a_id = s_a.id().to_owned();
        let s_b = stamp(memory(
            NodeKind::AgentSession,
            "s-b",
            Some("2026-01-01T01:00:00Z"),
            "S_b",
        ));
        let s_b_id = s_b.id().to_owned();
        let o_a = stamp(memory(
            NodeKind::Observation,
            "o-a",
            Some("2026-01-01T00:30:00Z"),
            "O_a",
        ));
        let o_a_id = o_a.id().to_owned();
        let o_b = stamp(memory(
            NodeKind::Observation,
            "o-b",
            Some("2026-01-01T01:30:00Z"),
            "O_b",
        ));
        let o_b_id = o_b.id().to_owned();

        let records = vec![
            repo_a,
            repo_b,
            sym_a,
            sym_b,
            code_edge(EdgeLabel::Contains, &repo_a_id, &sym_a_id),
            code_edge(EdgeLabel::Contains, &repo_b_id, &sym_b_id),
            s_a,
            s_b,
            o_a,
            o_b,
            am_edge(EdgeLabel::AuthoredBy, &o_a_id, &s_a_id),
            am_edge(EdgeLabel::MentionsSymbol, &o_a_id, &sym_a_id),
            am_edge(EdgeLabel::AuthoredBy, &o_b_id, &s_b_id),
            am_edge(EdgeLabel::MentionsSymbol, &o_b_id, &sym_b_id),
        ];
        let index = RepositoryIndex::build(&records);
        let out_a = sessions_for_repo(&records, &index, &repo_a_id, SESSIONS_DEFAULT_LIMIT);

        assert_eq!(
            out_a.sessions.len(),
            1,
            "only S_a is scoped to repo A: {out_a:?}"
        );
        assert!(
            !out_a
                .diagnostics
                .iter()
                .any(|d| d.code == "unlinked_session_stamped_records"),
            "S_b's real, edge-linked member (O_b) must never be flagged \
             unlinked in repo A's digest just because S_b itself is scoped \
             to a DIFFERENT repository: {out_a:?}"
        );
        assert_eq!(
            out_a.sessions[0].record_counts.observation, 1,
            "repo A's row counts only its OWN member (O_a): {:?}",
            out_a.sessions[0]
        );
    }

    #[test]
    fn unresolved_repository_scope_ids_are_capped_with_the_true_count_disclosed() {
        // A store with many imported-but-not-yet-`link-evidence`d sessions
        // (the documented pre-linking state) must not serialize an unbounded
        // `session_record_ids` list: it is capped at
        // MAX_UNRESOLVED_SESSION_IDS, with `count` staying the TRUE total and
        // `returned`/`limit` disclosing the cap.
        let repo = repository("repo-a");
        let repo_id = repo.id().to_owned();
        let total = MAX_UNRESOLVED_SESSION_IDS + 37;
        let mut records = vec![repo];
        for i in 0..total {
            records.push(memory(
                NodeKind::AgentSession,
                &format!("unresolved-{i}"),
                Some("2026-01-01T00:00:00Z"),
                "S",
            ));
        }
        let index = RepositoryIndex::build(&records);
        let out = sessions_for_repo(&records, &index, &repo_id, SESSIONS_DEFAULT_LIMIT);

        assert!(out.sessions.is_empty(), "no session resolves: {out:?}");
        let diagnostic = out
            .diagnostics
            .iter()
            .find(|d| d.code == "unresolved_repository_scope")
            .unwrap_or_else(|| panic!("must disclose the unresolved sessions: {out:?}"));
        assert_eq!(
            diagnostic.count,
            Some(total as u64),
            "count is the TRUE total, uncapped: {diagnostic:?}"
        );
        assert_eq!(
            diagnostic
                .session_record_ids
                .as_ref()
                .map(Vec::len)
                .unwrap_or_default(),
            MAX_UNRESOLVED_SESSION_IDS,
            "the LIST is capped: {diagnostic:?}"
        );
        assert_eq!(
            diagnostic.returned,
            Some(MAX_UNRESOLVED_SESSION_IDS as u64),
            "returned discloses the capped count: {diagnostic:?}"
        );
        assert_eq!(
            diagnostic.limit,
            Some(MAX_UNRESOLVED_SESSION_IDS as u64),
            "limit names the cap that applied: {diagnostic:?}"
        );
    }
}
