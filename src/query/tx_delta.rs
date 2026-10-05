//! Transaction-time delta feed for resuming and swarm agents (issue #197).
//!
//! # SPEC
//!
//! A returning agent — or an operator coordinating several agents against one
//! shared store — needs to ask *"what entered the knowledge store since I last
//! looked?"* Transaction-time *as-of* snapshots (`query_symbol_tx_as_of`,
//! issue #66) answer point-in-time state; this module answers the *delta
//! between two points*. It spans every domain on the one axis they share —
//! `transaction_time` — so one cursor reconciles new code facts, agent
//! memory, tasks, and verification evidence without re-reading the store.
//!
//! ## Cursor contract
//!
//! - The cursor is an RFC 3339 instant on the transaction-time axis. Selection
//!   is **strictly after** the cursor: a record stamped exactly at the cursor
//!   was already delivered by the previous call.
//! - `next_cursor` is the maximum *delivered* transaction time (RFC 3339).
//!   Feeding it back as the next cursor returns only records added after the
//!   prior call: no gaps, no overlap.
//! - An empty delta is a well-formed answer (`up_to_date: true`,
//!   `empty_reason: "up_to_date"`), distinct from an invalid cursor, which is
//!   an error. `next_cursor` echoes the input cursor on an empty delta.
//! - Rows order by `(transaction_time, record_id)` ascending: deterministic
//!   for a fixed store.
//! - Retraction and supersession records appear *as records* (classified by
//!   `event`), so a consumer learns a prior record was *retired* — not only
//!   that records were added. Edges and tombstones carry no transaction-time
//!   handle by schema and cannot be positioned on the feed; their exclusion is
//!   counted in `diagnostics`, never silent.
//! - `--limit` truncation only ever cuts *between* transaction-time groups (a
//!   partial group is never delivered), so `next_cursor` always lands on a
//!   fully-delivered instant and resumption stays gap-free.
//! - Records with no transaction-time metadata, or an unparseable handle, are
//!   excluded with counted diagnostics (no current-state fallback: a delta
//!   feed must not invent positions).
//!
//! ## Out of scope
//!
//! Push, subscribe, polling, or webhooks — this is a pull-only query.
//! Valid-time deltas, conflict resolution between concurrent writers, and
//! cross-store aggregation are separate slices.
//!
//! The unit tests below are the executable form of this contract: each
//! property above has a named test.

use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};

use super::{RepositoryIndex, TxDiagnostic, record_transaction_time};
use crate::ir::{Domain, GraphRecord, NodeKind};
use crate::schema_version::{domain_for_node_kind, normalize_domain_name};

/// Default `--limit` for `eg query since`.
pub const SINCE_DEFAULT_LIMIT: usize = 500;

/// Maximum accepted `--limit` for `eg query since`.
pub const SINCE_MAX_LIMIT: usize = 5000;

/// Machine-readable marker for the well-formed empty answer: the store holds
/// nothing newer than the cursor (issue #197 AC7).
pub const SINCE_EMPTY_REASON: &str = "up_to_date";

/// How a delta item entered the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxDeltaEvent {
    /// A newly recorded version (the common case).
    Added,
    /// The record carries `superseded_by`: this version retires a prior
    /// record, and the feed tells the consumer which one.
    Superseded,
    /// A `Retraction` node: an explicit transaction-stamped retraction of
    /// prior knowledge (issues #156 / #184).
    Retracted,
}

impl TxDeltaEvent {
    /// Stable machine-readable label (matches the serde representation).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Superseded => "superseded",
            Self::Retracted => "retracted",
        }
    }
}

/// One record on the delta feed.
#[derive(Debug, Clone)]
pub struct TxDeltaItem<'r> {
    /// The record as stored.
    pub record: &'r GraphRecord,
    /// Parsed transaction-time handle (strictly after the cursor).
    pub transaction_time: DateTime<FixedOffset>,
    /// How the record entered the store.
    pub event: TxDeltaEvent,
    /// Canonical domain (`codegraph`, `agent_memory`, `verification`, …).
    pub domain: String,
    /// Node kind name (only nodes carry a transaction-time handle).
    pub kind: &'static str,
    /// Replacement record ID when `event == Superseded`.
    pub superseded_by: Option<&'r str>,
}

/// Deterministic transaction-time delta over a record set.
#[derive(Debug)]
pub struct TxDeltaResult<'r> {
    /// Selected items, ordered by `(transaction_time, record_id)`.
    pub items: Vec<TxDeltaItem<'r>>,
    /// Resumption cursor: maximum delivered transaction time as RFC 3339, or
    /// the input cursor when the delta is empty.
    pub next_cursor: String,
    /// True when no record is newer than the cursor: a well-formed empty
    /// answer, distinct from an invalid cursor (which errors).
    pub up_to_date: bool,
    /// Matching records before `--limit` truncation.
    pub total_matching: usize,
    /// Items returned after `--limit` truncation.
    pub returned: usize,
    /// Whether `--limit` cut the feed (always between tx groups).
    pub truncated: bool,
    /// Machine-readable edge-case notes (excluded records, empty store).
    /// Sorted and deduplicated.
    pub diagnostics: Vec<TxDiagnostic>,
}

/// Error that aborts a delta query before any rows are produced.
///
/// Distinct from [`TxDiagnostic`]: an error means the query itself was
/// malformed (unparseable cursor, unknown domain, bad limit), so no result
/// set exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxDeltaError {
    /// Stable machine-readable code (`invalid_cursor`, …).
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

/// Canonical domain names accepted by the `--domain` filter.
const fn canonical_domains() -> [&'static str; 8] {
    [
        Domain::CodeGraph.as_str(),
        Domain::AgentMemory.as_str(),
        Domain::Verification.as_str(),
        Domain::Artifact.as_str(),
        Domain::Project.as_str(),
        Domain::Semantic.as_str(),
        Domain::UserContext.as_str(),
        Domain::Log.as_str(),
    ]
}

/// Resolves a node's canonical domain: the explicit `domain` field when
/// present (normalized), otherwise the kind-derived domain.
fn node_domain(kind: NodeKind, domain: Option<&str>) -> String {
    domain.map_or_else(
        || domain_for_node_kind(kind.as_str()).to_owned(),
        normalize_domain_name,
    )
}

/// Classifies how a node record entered the store.
fn classify_event(kind: NodeKind, superseded_by: Option<&str>) -> TxDeltaEvent {
    if kind == NodeKind::Retraction {
        TxDeltaEvent::Retracted
    } else if superseded_by.is_some() {
        TxDeltaEvent::Superseded
    } else {
        TxDeltaEvent::Added
    }
}

/// Computes the transaction-time delta of `records` since `tx_after`.
///
/// See the module-level SPEC for the cursor contract. `domains` holds
/// canonical domain names (validated; empty means all domains), `repo_id`
/// optionally scopes to one repository (issue #193 selectors), and `limit`
/// caps returned rows, truncating only between transaction-time groups so
/// `next_cursor` stays gap-free.
///
/// # Errors
///
/// Returns [`TxDeltaError`] with code `invalid_limit`, `invalid_cursor`, or
/// `unknown_domain` when the request itself is malformed.
pub fn transaction_delta<'r>(
    records: &'r [GraphRecord],
    tx_after: &str,
    domains: &[String],
    repo_id: Option<&str>,
    limit: usize,
) -> Result<TxDeltaResult<'r>, TxDeltaError> {
    if limit == 0 || limit > SINCE_MAX_LIMIT {
        return Err(TxDeltaError {
            code: "invalid_limit".to_owned(),
            message: format!(
                "--limit must be between 1 and {SINCE_MAX_LIMIT} (default {SINCE_DEFAULT_LIMIT})"
            ),
        });
    }
    let cursor = DateTime::parse_from_rfc3339(tx_after).map_err(|e| TxDeltaError {
        code: "invalid_cursor".to_owned(),
        message: format!(
            "invalid --tx-after cursor '{tx_after}': {e}; expected an RFC 3339 instant"
        ),
    })?;
    let canonical = canonical_domains();
    let wanted: Vec<String> = domains
        .iter()
        .map(|d| {
            let normalized = normalize_domain_name(d);
            if canonical.contains(&normalized.as_str()) {
                Ok(normalized)
            } else {
                Err(TxDeltaError {
                    code: "unknown_domain".to_owned(),
                    message: format!(
                        "unknown --domain '{d}'; expected one of: {}",
                        canonical.join(", ")
                    ),
                })
            }
        })
        .collect::<Result<_, _>>()?;

    let index = RepositoryIndex::build(records);

    let mut items: Vec<TxDeltaItem<'r>> = Vec::new();
    let mut missing_tx: usize = 0;
    let mut invalid_tx: usize = 0;
    let mut non_positionable: usize = 0;
    let mut stamped: usize = 0;

    for record in records {
        if let Some(repo) = repo_id
            && index.owner_of(record.id()) != Some(repo)
        {
            continue;
        }
        // Edges and tombstones carry no transaction-time handle by schema, so
        // they cannot be positioned on the feed. Counted, never silent.
        let GraphRecord::Node {
            kind,
            domain,
            superseded_by,
            ..
        } = record
        else {
            non_positionable += 1;
            continue;
        };
        let domain_name = node_domain(*kind, domain.as_deref());
        if !wanted.is_empty() && !wanted.contains(&domain_name) {
            continue;
        }
        let Some(tx_str) = record_transaction_time(record) else {
            missing_tx += 1;
            continue;
        };
        let Ok(tx) = DateTime::parse_from_rfc3339(tx_str) else {
            invalid_tx += 1;
            continue;
        };
        stamped += 1;
        // Strictly after: the cursor is the last instant the consumer has
        // already seen, so equality means "already delivered".
        if tx <= cursor {
            continue;
        }
        items.push(TxDeltaItem {
            record,
            transaction_time: tx,
            event: classify_event(*kind, superseded_by.as_deref()),
            domain: domain_name,
            kind: record.node_kind_name().unwrap_or("node"),
            superseded_by: superseded_by.as_deref(),
        });
    }

    // Deterministic total order: instants first, record ID breaks ties.
    items.sort_by(|a, b| {
        a.transaction_time
            .cmp(&b.transaction_time)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });

    // Truncate between transaction-time groups only: deliver every group
    // strictly before the cut instant, but always at least the first group (a
    // single instant may hold more than `limit` records). The cursor then
    // lands on a fully-delivered instant — no gaps, no overlap on resume.
    let total_matching = items.len();
    let mut truncated = false;
    if items.len() > limit {
        truncated = true;
        let first_tx = items[0].transaction_time;
        let cut_tx = items[limit].transaction_time;
        let end = if cut_tx == first_tx {
            items
                .iter()
                .position(|it| it.transaction_time > first_tx)
                .unwrap_or(items.len())
        } else {
            items
                .iter()
                .position(|it| it.transaction_time >= cut_tx)
                .unwrap_or(items.len())
        };
        items.truncate(end);
    }
    let returned = items.len();

    let next_cursor = items.last().map_or_else(
        || tx_after.to_owned(),
        |it| {
            it.transaction_time
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::Secs, true)
        },
    );

    let mut diagnostics: Vec<TxDiagnostic> = Vec::new();
    if missing_tx > 0 {
        diagnostics.push(TxDiagnostic {
            code: "missing_transaction_metadata".to_owned(),
            message: format!(
                "{missing_tx} node record(s) carry no transaction-time metadata; excluded (no current-state fallback)"
            ),
        });
    }
    if invalid_tx > 0 {
        diagnostics.push(TxDiagnostic {
            code: "invalid_record_transaction_time".to_owned(),
            message: format!(
                "{invalid_tx} record(s) carry an unparseable transaction_time; excluded"
            ),
        });
    }
    if non_positionable > 0 {
        diagnostics.push(TxDiagnostic {
            code: "non_positionable_records".to_owned(),
            message: format!(
                "{non_positionable} edge/tombstone record(s) carry no transaction-time handle by schema; they cannot appear on the feed"
            ),
        });
    }
    if stamped == 0 {
        diagnostics.push(TxDiagnostic {
            code: "no_transaction_stamps".to_owned(),
            message:
                "no record in scope carries a parseable transaction-time handle; the feed is vacuously up-to-date"
                    .to_owned(),
        });
    }
    diagnostics.sort_by(|a, b| a.code.cmp(&b.code).then_with(|| a.message.cmp(&b.message)));
    diagnostics.dedup();

    Ok(TxDeltaResult {
        items,
        next_cursor,
        up_to_date: returned == 0,
        total_matching,
        returned,
        truncated,
        diagnostics,
    })
}

#[cfg(test)]
mod tests {
    //! RED-first contract tests for the transaction-time delta feed.
    //!
    //! Each acceptance criterion from issue #197 has a named test. These were
    //! written against the SPEC before the implementation existed; with the
    //! `todo!()` stub above they all fail, and with the real implementation
    //! they all pass.

    use super::*;
    use crate::ir::SCHEMA_VERSION;

    const T0: &str = "2025-12-31T00:00:00Z";
    const T1: &str = "2026-01-01T00:00:00Z";
    const T2: &str = "2026-01-02T00:00:00Z";
    const T3: &str = "2026-01-03T00:00:00Z";

    fn node(id: &str, kind: NodeKind, tx: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("body of {id}"),
        )
        .with_transaction_time(tx)
    }

    fn node_no_tx(id: &str, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("body of {id}"),
        )
    }

    fn tombstone(id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: id.to_owned(),
            schema_version: SCHEMA_VERSION,
            deleted_id: id.to_owned(),
            summary: format!("tombstone of {id}"),
            producer: None,
        }
    }

    fn ids<'a>(result: &'a TxDeltaResult<'a>) -> Vec<&'a str> {
        result.items.iter().map(|it| it.record.id()).collect()
    }

    fn txs(result: &TxDeltaResult<'_>) -> Vec<String> {
        result
            .items
            .iter()
            .map(|it| {
                it.transaction_time
                    .with_timezone(&Utc)
                    .to_rfc3339_opts(SecondsFormat::Secs, true)
            })
            .collect()
    }

    /// AC1: strictly-after selection with the minimum payload on every row.
    #[test]
    fn returns_records_strictly_after_cursor_with_minimum_payload() {
        let records = vec![
            node("old", NodeKind::Symbol, T1),
            node("new-a", NodeKind::Symbol, T2),
            node("new-b", NodeKind::Observation, T3),
        ];
        let result = transaction_delta(&records, T1, &[], None, 500).expect("valid request");

        assert_eq!(ids(&result), vec!["new-a", "new-b"]);
        for item in &result.items {
            assert!(!item.domain.is_empty(), "domain required");
            assert!(!item.kind.is_empty(), "kind required");
            assert!(!item.record.id().is_empty(), "record_id required");
        }
        assert_eq!(txs(&result), vec![T2, T3]);
        assert_eq!(result.next_cursor, T3);
        assert!(!result.up_to_date);
        assert_eq!(result.total_matching, 2);
        assert_eq!(result.returned, 2);
        assert!(!result.truncated);
    }

    /// The cursor is exclusive: a record stamped exactly at the cursor was
    /// already delivered by the previous call.
    #[test]
    fn cursor_is_exclusive() {
        let records = vec![node("at-cursor", NodeKind::Symbol, T2)];
        let result = transaction_delta(&records, T2, &[], None, 500).expect("valid request");
        assert!(result.items.is_empty());
        assert!(result.up_to_date);
        assert_eq!(result.next_cursor, T2, "empty delta echoes the cursor");
    }

    /// AC7: an empty delta is a well-formed up-to-date answer, not an error.
    #[test]
    fn empty_delta_is_up_to_date_not_an_error() {
        let records = vec![node("old", NodeKind::Symbol, T1)];
        let result = transaction_delta(&records, T3, &[], None, 500).expect("valid request");
        assert!(result.up_to_date);
        assert!(result.items.is_empty());
        assert_eq!(result.next_cursor, T3);
        assert_eq!(result.total_matching, 0);
    }

    /// AC7: a malformed cursor is a machine-readable error, not an empty feed.
    #[test]
    fn invalid_cursor_is_an_error() {
        let records = vec![node("x", NodeKind::Symbol, T1)];
        let err = transaction_delta(&records, "not-a-time", &[], None, 500).expect_err("must fail");
        assert_eq!(err.code, "invalid_cursor");
        assert!(!err.message.is_empty());
    }

    /// AC2: `next_cursor` round-trips with no gaps and no overlap.
    #[test]
    fn next_cursor_round_trips_without_gaps_or_overlap() {
        let records = vec![
            node("a", NodeKind::Symbol, T1),
            node("b", NodeKind::Symbol, T2),
            node("c", NodeKind::Symbol, T3),
        ];
        let first = transaction_delta(&records, T0, &[], None, 500).expect("first");
        assert_eq!(ids(&first), vec!["a", "b", "c"]);

        let second =
            transaction_delta(&records, &first.next_cursor, &[], None, 500).expect("second");
        assert!(second.items.is_empty(), "no overlap on resume");
        assert!(second.up_to_date);
        assert_eq!(second.next_cursor, first.next_cursor);

        // A mid-window resume replays exactly the tail.
        let mid = transaction_delta(&records, T1, &[], None, 500).expect("mid");
        assert_eq!(ids(&mid), vec!["b", "c"]);
    }

    /// AC3: window partitions compose — delta(T0,T2) as a set equals
    /// delta(T0,T1) ∪ delta(T1,T2).
    #[test]
    fn window_partitions_compose_without_gaps_or_duplicates() {
        let records = vec![
            node("a", NodeKind::Symbol, T1),
            node("b", NodeKind::Symbol, T2),
            node("c", NodeKind::Symbol, T2),
            node("d", NodeKind::Symbol, T3),
        ];
        let whole = transaction_delta(&records, T0, &[], None, 500).expect("whole");
        let left = transaction_delta(&records, T0, &[], None, 500)
            .map(|mut r| {
                r.items.retain(|it| it.transaction_time <= t1());
                r
            })
            .expect("left");
        let right = transaction_delta(&records, T1, &[], None, 500).expect("right");

        let mut composed: Vec<&str> = left
            .items
            .iter()
            .chain(right.items.iter())
            .map(|it| it.record.id())
            .collect();
        composed.sort_unstable();
        composed.dedup();
        let mut whole_ids = ids(&whole);
        whole_ids.sort_unstable();
        assert_eq!(composed, whole_ids, "partitions compose set-equally");
        assert_eq!(composed.len(), 4, "no gaps, no duplicates");
    }

    fn t1() -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(T1).unwrap()
    }

    /// Deterministic ordering: equal instants order by record ID; repeated
    /// runs are byte-identical.
    #[test]
    fn deterministic_order_by_tx_then_record_id() {
        let records = vec![
            node("z-last", NodeKind::Symbol, T2),
            node("a-first", NodeKind::Symbol, T2),
            node("m-mid", NodeKind::Symbol, T1),
        ];

        let first = transaction_delta(&records, T0, &[], None, 500).expect("first");
        let second = transaction_delta(&records, T0, &[], None, 500).expect("second");

        assert_eq!(ids(&first), vec!["m-mid", "a-first", "z-last"]);
        let json = |r: &TxDeltaResult<'_>| {
            r.items
                .iter()
                .map(|it| {
                    format!(
                        "{}:{}",
                        it.transaction_time
                            .with_timezone(&Utc)
                            .to_rfc3339_opts(SecondsFormat::Secs, true),
                        it.record.id()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(json(&first), json(&second), "repeatable across runs");
    }

    /// AC5: `--domain` filters to the requested domains; unknown names are a
    /// machine-readable error; filtering is repeatable.
    #[test]
    fn domain_filter_restricts_and_validates() {
        let records = vec![
            node("s", NodeKind::Symbol, T2),
            node("o", NodeKind::Observation, T2).with_domain("agent_memory", SCHEMA_VERSION),
            node("v", NodeKind::TestRun, T2).with_domain("verification", SCHEMA_VERSION),
        ];
        let domains = vec!["agent_memory".to_owned()];
        let result = transaction_delta(&records, T0, &domains, None, 500).expect("filtered");
        assert_eq!(ids(&result), vec!["o"]);
        assert_eq!(result.items[0].domain, "agent_memory");

        let again = transaction_delta(&records, T0, &domains, None, 500).expect("again");
        assert_eq!(ids(&again), ids(&result), "repeatable domain filtering");

        let bad = vec!["nope".to_owned()];
        let err = transaction_delta(&records, T0, &bad, None, 500).expect_err("unknown domain");
        assert_eq!(err.code, "unknown_domain");
    }

    /// AC6: supersession and retraction surface as records with `event`
    /// labels; versions are never collapsed.
    #[test]
    fn supersession_and_retraction_appear_as_records() {
        let records = vec![
            node("obs-1", NodeKind::Observation, T1).with_domain("agent_memory", SCHEMA_VERSION),
            node("obs-1", NodeKind::Observation, T2)
                .with_domain("agent_memory", SCHEMA_VERSION)
                .with_superseded_by("agent_memory:v1:obs-2"),
            node("retract-1", NodeKind::Retraction, T3).with_domain("agent_memory", SCHEMA_VERSION),
        ];
        let result = transaction_delta(&records, T0, &[], None, 500).expect("valid request");
        assert_eq!(result.items.len(), 3, "every version is an event");

        let events: Vec<TxDeltaEvent> = result.items.iter().map(|it| it.event).collect();
        assert_eq!(
            events,
            vec![
                TxDeltaEvent::Added,
                TxDeltaEvent::Superseded,
                TxDeltaEvent::Retracted
            ]
        );
        assert_eq!(result.items[1].superseded_by, Some("agent_memory:v1:obs-2"));
    }

    /// Edges and tombstones carry no transaction-time handle by schema: they
    /// cannot be positioned on the feed, and the exclusion is counted.
    #[test]
    fn unpositionable_records_are_excluded_with_diagnostics() {
        let records = vec![node("n", NodeKind::Symbol, T2), tombstone("t")];
        let result = transaction_delta(&records, T0, &[], None, 500).expect("valid request");
        assert_eq!(ids(&result), vec!["n"]);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "non_positionable_records"),
            "exclusion must be counted, never silent"
        );
    }

    /// An unparseable transaction-time handle excludes the record with a
    /// counted diagnostic (no current-state fallback).
    #[test]
    fn unparseable_tx_handle_is_excluded_with_diagnostic() {
        let mut bad = node("bad", NodeKind::Symbol, T2);
        // Corrupt the handle after stamping: the record keeps its shape but
        // the feed must refuse to position it.
        if let GraphRecord::Node {
            transaction_time, ..
        } = &mut bad
        {
            *transaction_time = Some("not-a-time".to_owned());
        }
        let records = vec![node("good", NodeKind::Symbol, T2), bad];
        let result = transaction_delta(&records, T0, &[], None, 500).expect("valid request");
        assert_eq!(ids(&result), vec!["good"]);
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "invalid_record_transaction_time")
        );
    }

    /// Truncation cuts between transaction-time groups, never inside one, so
    /// `next_cursor` lands on a fully-delivered instant.
    #[test]
    fn limit_truncates_between_tx_groups_not_inside_them() {
        let records = vec![
            node("a1", NodeKind::Symbol, T1),
            node("a2", NodeKind::Symbol, T1),
            node("a3", NodeKind::Symbol, T1),
            node("b1", NodeKind::Symbol, T2),
            node("b2", NodeKind::Symbol, T2),
            node("b3", NodeKind::Symbol, T2),
        ];
        let page1 = transaction_delta(&records, T0, &[], None, 4).expect("page1");
        assert!(page1.truncated);
        // Limit 4 would split the T2 group; the whole T1 group (3) is
        // delivered instead and the cursor stops at T1.
        assert_eq!(ids(&page1), vec!["a1", "a2", "a3"]);
        assert_eq!(page1.next_cursor, T1);
        assert_eq!(page1.total_matching, 6);

        let page2 = transaction_delta(&records, &page1.next_cursor, &[], None, 500).expect("page2");
        assert_eq!(ids(&page2), vec!["b1", "b2", "b3"]);
        assert!(!page2.truncated);
        assert!(!page2.up_to_date);
    }

    /// A single instant holding more than `limit` records is delivered whole:
    /// splitting it would lose rows on resume from a timestamp-only cursor.
    #[test]
    fn oversized_single_group_is_delivered_whole() {
        let records = vec![
            node("a", NodeKind::Symbol, T1),
            node("b", NodeKind::Symbol, T1),
            node("c", NodeKind::Symbol, T1),
        ];
        let result = transaction_delta(&records, T0, &[], None, 2).expect("valid request");
        assert_eq!(result.items.len(), 3, "group delivered whole");
        assert!(result.truncated);
        assert_eq!(result.next_cursor, T1);

        let resume =
            transaction_delta(&records, &result.next_cursor, &[], None, 500).expect("resume");
        assert!(resume.up_to_date, "no rows lost, none repeated");
    }

    /// A store with no transaction stamps anywhere is vacuously up-to-date,
    /// with an explicit diagnostic.
    #[test]
    fn store_without_tx_stamps_is_vacuously_up_to_date() {
        let records = vec![node_no_tx("n", NodeKind::Symbol)];
        let result = transaction_delta(&records, T0, &[], None, 500).expect("valid request");
        assert!(result.up_to_date);
        assert!(result.items.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "no_transaction_stamps")
        );
    }

    /// AC1: one call spans every domain on the shared transaction-time axis.
    #[test]
    fn single_call_spans_all_domains() {
        let records = vec![
            node("s", NodeKind::Symbol, T1),
            node("o", NodeKind::Observation, T2).with_domain("agent_memory", SCHEMA_VERSION),
            node("v", NodeKind::TestRun, T3).with_domain("verification", SCHEMA_VERSION),
        ];
        let result = transaction_delta(&records, T0, &[], None, 500).expect("valid request");
        assert_eq!(ids(&result), vec!["s", "o", "v"]);
        let domains: Vec<&str> = result.items.iter().map(|it| it.domain.as_str()).collect();
        assert_eq!(domains, vec!["codegraph", "agent_memory", "verification"]);
    }

    /// Limit bounds are validated before any work happens.
    #[test]
    fn limit_bounds_are_validated() {
        let records = vec![node("a", NodeKind::Symbol, T1)];
        for bad in [0, SINCE_MAX_LIMIT + 1] {
            let err = transaction_delta(&records, T0, &[], None, bad).expect_err("bad limit");
            assert_eq!(err.code, "invalid_limit", "limit {bad}");
        }
    }

    /// Non-UTC cursors round-trip: the cursor is an instant, not a string.
    #[test]
    fn non_utc_cursor_round_trips() {
        let records = vec![node("a", NodeKind::Symbol, "2026-02-01T05:00:00+05:00")];
        let result = transaction_delta(&records, "2026-01-01T00:00:00+05:00", &[], None, 500)
            .expect("valid request");
        assert_eq!(result.items.len(), 1);
        let resume =
            transaction_delta(&records, &result.next_cursor, &[], None, 500).expect("resume");
        assert!(resume.up_to_date);
    }
}
