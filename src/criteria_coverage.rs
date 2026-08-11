//! The store-wide acceptance-criterion proof-gap census (issue #115).
//!
//! # What this answers
//!
//! Every existing lane starts from ONE handle: `eg query task` (#48) drills into a
//! single `Task`'s criteria, `eg query verification-coverage` (#109) reports code
//! *symbols* lacking verification, `eg audit memory-health` (#94) ages
//! agent-memory. None answers the store-wide question **"across all imported
//! work, which acceptance criteria are actually proven by passing evidence, and
//! which tasks are marked done while owning unproven criteria?"** Without that
//! roll-up a reader takes `Task.status: closed_completed` as "done" while the
//! criteria sit unproven underneath — the exact *closed-but-unproven* failure the
//! verification domain exists to prevent.
//!
//! # Trust separation (the load-bearing rule)
//!
//! `proven` means exactly one thing: **the criterion is closed by a live
//! `CLOSES_ACCEPTANCE_CRITERION` link to a verification record whose recorded
//! outcome is passing.** Being a verification record takes three checks, not one
//! — a verification-domain record ID, a `NodeKind` the verification domain
//! permits, and not the criterion itself (see [`resolve_handle`]).
//!
//! A criterion is never counted proven from:
//!
//! * `Task.status` — a human toggle in an issue tracker;
//! * its own `AcceptanceCriterion.status: verified` — a claim, echoed for
//!   citation but never read for bucketing;
//! * a commit message, prose, or semantic similarity;
//! * an `agent_memory:v<N>:` record, whatever its kind — notably the
//!   `NodeKind::CommandEvidence` that `eg command-evidence` mints from an
//!   agent-supplied `--exit-code`. Such a link lands in
//!   `non_verification_evidence` — reported, never counted.
//!
//! # What the gate does NOT establish (honest limit)
//!
//! The gate proves the closing record was **minted as verification evidence**.
//! It does **not** prove the outcome was independently observed, and this lane
//! must not be read as though it did:
//!
//! * `eg write verification --status pass` mints a `verification:v<N>:`
//!   `NodeKind::Verification` whose `status` comes **straight from its caller**.
//! * The Claude Code and Antigravity transcript importers mint
//!   verification-domain `Verification` / `CommandRun` records from what an
//!   agent transcript *said* a command did.
//! * `eg capture-tests` is the one trunk writer that mints a verification-domain
//!   record by parsing an independent libtest artifact.
//!
//! So `proven` is *"closed by a passing verification record"*, not *"someone
//! independently confirmed this works"*. Rather than silently pick a winner
//! among writers, every closing link discloses the
//! [`ClosingLinkJson::producer_kind`] that wrote it, so a reader can see whether
//! a `proven` rests on a captured test run or on an agent's own account.
//! (`producer_kind` does not fully discriminate: `eg capture-tests` and
//! `eg write verification` both record `observation_writer`.)
//!
//! # Bucket precedence (fail-closed)
//!
//! Buckets are mutually exclusive and partition every live criterion. The schema
//! makes `CLOSES_ACCEPTANCE_CRITERION` many:1, so a criterion normally carries
//! exactly one closing handle and precedence never fires. When several are
//! present the *most alarming* wins, so a passing link can never mask a failing
//! or dangling one:
//!
//! `failed_evidence` > `dangling_evidence` > `non_verification_evidence` >
//! `inconclusive_evidence` > `proven` > `unverified`.
//!
//! Every handle is listed on the row with its own resolution, so the derivation
//! is auditable rather than asserted.
//!
//! # What a number here is not
//!
//! A poor coverage number is a reason to **review/verify**, never proof that any
//! one task is wrong. Absence of closing evidence means *no imported proof*, not
//! that the work is broken or untested. Presence of a passing record is a
//! recorded execution, never proof of correctness.
//!
//! Pure and deterministic: no I/O, no network, no wall clock; byte-identical
//! across runs on an unchanged store and independent of record order.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};
use crate::query::liveness::Liveness;
use crate::query::{
    VerificationOutcome, is_verification_domain_kind, is_verification_domain_record,
    verification_outcome,
};

/// The verbatim report disclaimer.
pub const CRITERIA_COVERAGE_DISCLAIMER: &str = "`proven` means a criterion is CLOSED BY A PASSING \
VERIFICATION RECORD; it does NOT mean the outcome was independently observed \u{2014} `eg write \
verification --status pass` and the agent-transcript importers both mint verification-domain records \
from agent-supplied outcomes, so read each row's producer_kind before trusting it; a poor coverage \
number is a reason to REVIEW/VERIFY, never proof that any one task is wrong; absence of closing \
evidence means no imported proof, not that the work is broken; never an auditor opinion";

/// Default per-list row cap.
pub const CRITERIA_COVERAGE_DEFAULT_LIMIT: usize = 500;

/// Maximum accepted `--limit`.
pub const CRITERIA_COVERAGE_MAX_LIMIT: usize = 1000;

/// Numerator, denominator, and the derived ratio (`None` when the denominator is
/// zero — never a divide-by-zero and never a bare percentage).
///
/// Deliberately NOT unified with `crate::memory_health::MemoryHealthCounts`,
/// whose `ratio` is a bare `f64` and therefore fabricates a value on a zero
/// denominator. `Option<f64>` is the whole point here: issue #115 requires a
/// zero denominator to be a stable diagnostic, not a number. Unify toward THIS
/// shape, never back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ratio {
    /// Count matching the metric.
    pub numerator: usize,
    /// Total population.
    pub denominator: usize,
    /// `numerator / denominator`, or `null` when the denominator is zero.
    pub ratio: Option<f64>,
}

/// The closed bucket vocabulary. Buckets are mutually exclusive and partition
/// every live acceptance criterion in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CriterionBucket {
    /// Closed by a resolvable verification-domain record with a passing outcome.
    Proven,
    /// A closing link resolves to a verification-domain record whose outcome is failing.
    FailedEvidence,
    /// A closing handle does not resolve to a live record.
    DanglingEvidence,
    /// A closing link resolves to a record that is not verification-domain.
    NonVerificationEvidence,
    /// A closing link resolves to a verification-domain record with no recognizable outcome.
    InconclusiveEvidence,
    /// No closing verification evidence at all.
    Unverified,
}

impl CriterionBucket {
    /// Every bucket, in documentation order.
    pub const ALL: [Self; 6] = [
        Self::Proven,
        Self::FailedEvidence,
        Self::DanglingEvidence,
        Self::NonVerificationEvidence,
        Self::InconclusiveEvidence,
        Self::Unverified,
    ];

    /// The stable wire string.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Proven => "proven",
            Self::FailedEvidence => "failed_evidence",
            Self::DanglingEvidence => "dangling_evidence",
            Self::NonVerificationEvidence => "non_verification_evidence",
            Self::InconclusiveEvidence => "inconclusive_evidence",
            Self::Unverified => "unverified",
        }
    }
}

/// How one closing handle resolved. Closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LinkResolution {
    /// Live verification-domain record with a passing recorded outcome.
    Passing,
    /// Live verification-domain record with a failing recorded outcome.
    Failing,
    /// Live verification-domain record whose outcome is neither passing nor failing.
    Inconclusive,
    /// Live record that is not a verification record: its ID is not
    /// verification-domain, its `NodeKind` is not one the verification domain
    /// permits, or it is the criterion itself.
    NotVerificationRecord,
    /// Several physical versions of the target disagree about the outcome, and
    /// this transport cannot say which is current. Never proof — see
    /// [`resolve_handle`].
    AmbiguousVersions,
    /// The handle names no live record (absent or tombstoned).
    Unresolved,
}

impl LinkResolution {
    /// Every resolution, in documentation order.
    pub const ALL: [Self; 6] = [
        Self::Passing,
        Self::Failing,
        Self::Inconclusive,
        Self::NotVerificationRecord,
        Self::AmbiguousVersions,
        Self::Unresolved,
    ];

    /// The stable wire string.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Passing => "passing",
            Self::Failing => "failing",
            Self::Inconclusive => "inconclusive",
            Self::NotVerificationRecord => "not_verification_record",
            Self::AmbiguousVersions => "ambiguous_versions",
            Self::Unresolved => "unresolved",
        }
    }
}

/// One closing handle carried by a criterion, with how it resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosingLinkJson {
    /// The closing verification record handle as recorded. Control-sanitized
    /// but never truncated — see [`handle_field`].
    pub handle: String,
    /// Where the handle came from: `closes_edge` and/or `verification_link_id`.
    pub origins: Vec<String>,
    /// The closed-set resolution wire name.
    pub resolution: String,
    /// The target's `NodeKind`, when it resolved. Always sourced from
    /// `NodeKind::as_str()` — a CLOSED compile-time vocabulary, never a store
    /// string — so it is the kind field a consumer may trust.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_kind: Option<String>,
    /// The target's recorded `verification_kind`, when it resolved and carries
    /// one. FREE TEXT read back from the store — sanitized and length-capped
    /// via [`bounded_field`]. Prefer [`Self::node_kind`] when you need a kind
    /// you can trust.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_kind: Option<String>,
    /// The target's recorded `status`, when it resolved and carries one.
    ///
    /// `docs/schema/verification.md` defines `status` as a FREE STRING with no
    /// enum enforcement at v1, so this value is operator- or
    /// attacker-controlled: it is sanitized and length-capped via
    /// [`bounded_field`] before it reaches output. It is echoed only so a
    /// reader can see what the record CLAIMED; the trustworthy reading of the
    /// outcome is [`Self::resolution`], which is a closed set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// The target's recorded `exit_code`, when it resolved and carries one.
    /// An integer, so it needs no sanitizing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Which Egregore subsystem WROTE the closing record (`ProducerKind`, a
    /// closed compile-time vocabulary).
    ///
    /// Disclosed because the verification-domain gate establishes only that a
    /// record was minted as verification evidence — NOT that its outcome was
    /// independently observed. `eg write verification --status pass` and the
    /// agent-transcript importers both mint verification-domain records from
    /// agent-supplied outcomes. This field lets a reader see the origin instead
    /// of taking `proven` on faith. HONEST LIMIT: it does not fully
    /// discriminate — `eg capture-tests` (which parses a real libtest artifact)
    /// and `eg write verification` (which takes `--status` from its caller) both
    /// record `observation_writer`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_kind: Option<String>,
}

/// One acceptance criterion, classified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionRowJson {
    /// Stable `AcceptanceCriterion` record ID (always cited).
    pub record_id: String,
    /// Owning `Task` record ID, when derivable.
    pub parent_task_id: Option<String>,
    /// The owning task's recorded status, when the task resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_status: Option<String>,
    /// The criterion's own recorded status enum. Echoed for citation only — it
    /// NEVER drives the bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub criterion_status: Option<String>,
    /// Position within the parent task's criterion list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u32>,
    /// The assigned bucket's wire name.
    pub bucket: String,
    /// Every closing handle with its resolution, sorted by handle.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub closing_links: Vec<ClosingLinkJson>,
    /// The deciding passing verification record ID on a `proven` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proving_verification_id: Option<String>,
    /// Repo-relative declaration path, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Source-system handle, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_handle: Option<String>,
    /// `ExternalLink` record ID carrying the source-system handle, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_link_id: Option<String>,
    /// `true` when the owning task is in a closed/done state and this criterion
    /// is not `proven`.
    pub claimed_done_unproven: bool,
}

/// One tripped gate threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriteriaCoverageBreach {
    /// Stable metric name.
    pub metric: String,
    /// Observed value, rendered as a string so ratios and counts share one shape.
    pub observed: String,
    /// The configured bound.
    pub bound: String,
    /// Redaction-safe detail.
    pub message: String,
}

/// A stable, redaction-safe report diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriteriaCoverageDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Record IDs the diagnostic derives from, sorted.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub record_ids: Vec<String>,
    /// Redaction-safe detail.
    pub detail: String,
}

/// The thresholds echoed into the report.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CriteriaCoverageThresholds {
    /// Minimum acceptable `proven / total` ratio.
    pub min_proven_ratio: f64,
    /// Maximum acceptable claimed-done-but-unproven criterion count.
    pub max_claimed_done_unproven: usize,
}

/// Caller configuration.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CriteriaCoverageConfig {
    /// Minimum acceptable `proven / total` ratio.
    pub min_proven_ratio: f64,
    /// Maximum acceptable claimed-done-but-unproven criterion count.
    pub max_claimed_done_unproven: usize,
    /// Per-list row cap.
    pub limit: usize,
}

impl Default for CriteriaCoverageConfig {
    fn default() -> Self {
        Self {
            min_proven_ratio: 1.0,
            max_claimed_done_unproven: 0,
            limit: CRITERIA_COVERAGE_DEFAULT_LIMIT,
        }
    }
}

/// The full deterministic `eg audit criteria-coverage` report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CriteriaCoverageReport {
    /// Whether every configured threshold was met.
    pub ok: bool,
    /// Live acceptance criteria in the store (the denominator).
    pub total_criteria: usize,
    /// Closed by a resolvable passing verification record.
    pub proven: Ratio,
    /// No closing verification evidence at all.
    pub unverified: Ratio,
    /// Closing link resolves to a failing verification record.
    pub failed_evidence: Ratio,
    /// Closing handle does not resolve.
    pub dangling_evidence: Ratio,
    /// Closing link resolves outside the verification domain.
    pub non_verification_evidence: Ratio,
    /// Closing link resolves to an unrecognizable outcome.
    pub inconclusive_evidence: Ratio,
    /// Everything not `proven` — the proof gap.
    pub proof_gap: Ratio,
    /// Per-bucket counts; every bucket present (0 when empty).
    pub bucket_counts: BTreeMap<String, usize>,
    /// The task statuses treated as closed/done.
    pub done_task_statuses: Vec<String>,
    /// Size of the claimed-done-but-unproven set (pre-truncation).
    pub claimed_done_unproven_count: usize,
    /// The claimed-done-but-unproven rows, sorted by record ID.
    pub claimed_done_unproven: Vec<CriterionRowJson>,
    /// Every classified criterion, sorted by record ID.
    pub criteria: Vec<CriterionRowJson>,
    /// The thresholds in effect.
    pub thresholds: CriteriaCoverageThresholds,
    /// Tripped thresholds, sorted by metric.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub breaches: Vec<CriteriaCoverageBreach>,
    /// Stable, redaction-safe diagnostics.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub diagnostics: Vec<CriteriaCoverageDiagnostic>,
    /// The verbatim always-present disclaimer.
    pub disclaimer: String,
}

/// Maximum characters of any free-text store value this report renders.
///
/// Reuses the #104 bound so the hardened surfaces cannot drift.
pub const CRITERIA_FIELD_MAX_CHARS: usize = crate::embeddings::IDENTITY_FIELD_MAX_CHARS;

/// Sanitizes and length-caps one FREE-TEXT value read back from the store.
///
/// The emitted field NAMES are allow-listed, but their VALUES are read back
/// from a store and are therefore operator- or attacker-controlled — most
/// sharply `status`, which `docs/schema/verification.md` defines as a free
/// string with **no enum enforcement at v1**, and `verification_kind`, which is
/// human-readable text. A hand-authored record can carry megabytes of text,
/// embedded newlines that forge extra output lines in `--format text`, or the
/// ESC that starts an ANSI sequence.
///
/// Delegates to the single hardened #104 implementation
/// ([`crate::embeddings::bounded_identity_field`]) — control characters become
/// `.`, then the value is truncated on a character boundary with a visible `…`
/// marker — so this lane, the semantic-index refusal path, and the control-
/// catalog envelope all share one rule.
fn bounded_field(value: &str) -> String {
    crate::embeddings::bounded_identity_field(value)
}

/// Sanitizes one HANDLE value (record ID, path, source handle) read back from
/// the store, **without** truncating it.
///
/// Control characters are neutralized for exactly the reasons in
/// [`bounded_field`], but the length cap is deliberately NOT applied: a
/// truncated handle is no longer a citation — an agent cannot look it up, and a
/// prefix of a record ID silently reads like a valid one. The citation contract
/// (issue #115 AC6: every row carries a citable record ID and, where present, a
/// repo-relative file/span or source-system handle) requires the value to stay
/// usable. This mirrors the same two-tier split `crate::evidence_pack` draws
/// between `sanitize_catalog_text` and `bounded_catalog_field`.
fn handle_field(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '.' } else { c })
        .collect()
}

/// Task statuses treated as a closed/done state for the
/// **claimed-done-but-unproven** set.
///
/// Deliberately just `closed_completed` out of the closed project-domain
/// vocabulary (`open` / `in_progress` / `blocked` / `closed_completed` /
/// `closed_dropped` / `unknown`). `closed_dropped` is *closed* but claims no
/// completion — the work was abandoned, not asserted done — so counting it would
/// manufacture a proof gap for deliberately dropped work. The set is echoed into
/// every report (`done_task_statuses`) so the definition is visible in the
/// output rather than buried here.
const DONE_TASK_STATUSES: &[&str] = &["closed_completed"];

/// Origin marker for a handle recorded on the `CLOSES_ACCEPTANCE_CRITERION` edge.
const ORIGIN_EDGE: &str = "closes_edge";
/// Origin marker for a handle recorded in the denormalized field.
const ORIGIN_FIELD: &str = "verification_link_id";

/// Builds a ratio, reporting `None` rather than dividing by zero.
fn ratio(numerator: usize, denominator: usize) -> Ratio {
    #[allow(clippy::cast_precision_loss)]
    let value = (denominator > 0).then(|| numerator as f64 / denominator as f64);
    Ratio {
        numerator,
        denominator,
        ratio: value,
    }
}

/// How one closing handle resolved, plus the target's reported metadata.
struct ResolvedLink {
    /// The closed-set resolution.
    resolution: LinkResolution,
    /// The target's `NodeKind` — a closed compile-time vocabulary.
    node_kind: Option<&'static str>,
    // (kept as `&'static str` inside the core so it CANNOT be built from a
    // store string; widened to `String` only at the serialized boundary.)
    /// The target's free-text `verification_kind`, already bounded.
    verification_kind: Option<String>,
    /// The target's free-text `status`, already bounded.
    status: Option<String>,
    /// The target's `exit_code`.
    exit_code: Option<i64>,
    /// The writing subsystem's `ProducerKind` — a closed vocabulary.
    producer_kind: Option<&'static str>,
}

/// Resolves one closing handle against the live node view.
///
/// Two hardening rules live here, in the pure core, so both transports inherit
/// them:
///
/// 1. **Bounding.** Every free-text value read off the target goes through
///    [`bounded_field`], so neither transport can render a raw store value.
///
/// 2. **Version ambiguity fails closed.** `versions` carries *every* live
///    physical write of the handle, not just one. Egregore writes graph JSONL
///    with lexicographically SORTED lines (`Graph::to_jsonl`, `eg export`), so
///    over `--graph` the order of two writes of one id carries no information
///    about which is current — "latest write wins" is simply unavailable on that
///    transport. If a `TestRun` were rewritten from `passed` to `failed` under
///    one id, picking by position could report the stale PASSING version and
///    hand back `proven` / exit 0 for a criterion the store records as failed.
///    That is precisely the "report completion it never proved" failure this
///    lane exists to prevent, so when live versions DISAGREE about the outcome
///    the link resolves [`LinkResolution::AmbiguousVersions`] — never proof, and
///    never a fabricated failure either, since which version is current is
///    genuinely unknown. Use `--data-dir` for an authoritative current-state
///    read.
///
/// # Being a verification record takes THREE checks, not one
///
/// `criterion_id` is passed in so a criterion can never close itself.
///
/// The record ID prefix alone is not sufficient. The embedded ingest path does
/// not run the daemon's `validate_verification_domain_records`, so a store can
/// hold a node with a `verification:v<N>:` ID and an `Observation`, `Task`, or
/// even `AcceptanceCriterion` kind — each of which the prefix check alone would
/// happily certify as proof. A link is treated as verification evidence only
/// when ALL of:
///
/// 1. the ID is verification-domain ([`is_verification_domain_record`]);
/// 2. the `NodeKind` is one the verification domain permits
///    ([`is_verification_domain_kind`] — the SAME list the daemon write path
///    enforces);
/// 3. the handle is not the criterion's own record ID.
///
/// Anything else resolves [`LinkResolution::NotVerificationRecord`].
fn resolve_handle(criterion_id: &str, versions: Option<&Vec<&GraphRecord>>) -> ResolvedLink {
    let unresolved = ResolvedLink {
        resolution: LinkResolution::Unresolved,
        node_kind: None,
        verification_kind: None,
        status: None,
        exit_code: None,
        producer_kind: None,
    };
    // Absent OR tombstoned: either way the handle names no live record, and is
    // reported with its ORIGINAL text rather than silently dropped.
    let Some(versions) = versions.filter(|v| !v.is_empty()) else {
        return unresolved;
    };
    // Metadata is reported from the last version in the slice; the resolution
    // below never depends on that choice.
    let Some(GraphRecord::Node {
        kind,
        verification_kind,
        status,
        exit_code,
        producer,
        ..
    }) = versions.last().copied()
    else {
        return unresolved;
    };
    let common = ResolvedLink {
        resolution: LinkResolution::Unresolved,
        node_kind: Some(kind.as_str()),
        verification_kind: verification_kind.as_deref().map(bounded_field),
        status: status.as_deref().map(bounded_field),
        exit_code: *exit_code,
        producer_kind: producer.as_ref().map(|p| p.producer_kind.as_str()),
    };
    // A criterion is never its own proof. The kind check below already rejects
    // the ordinary shape of this (an `AcceptanceCriterion` kind), but an id
    // carrying BOTH a criterion and a verification-kind node would slip past it,
    // so the self-reference is refused explicitly.
    let self_closing = versions.iter().any(|v| v.id() == criterion_id);
    // The ID-prefix gate is version-invariant (every version of one id shares
    // it); the KIND gate is checked across every version, so a single
    // non-verification version is enough to refuse.
    let verification_record = !self_closing
        && is_verification_domain_record(versions[0])
        && versions.iter().copied().all(is_verification_domain_kind);
    if !verification_record {
        return ResolvedLink {
            resolution: LinkResolution::NotVerificationRecord,
            ..common
        };
    }
    let outcomes: BTreeSet<LinkResolution> = versions
        .iter()
        .map(|version| match verification_outcome(version) {
            VerificationOutcome::Passing => LinkResolution::Passing,
            VerificationOutcome::Failing => LinkResolution::Failing,
            VerificationOutcome::Inconclusive => LinkResolution::Inconclusive,
        })
        .collect();
    let resolution = if outcomes.len() > 1 {
        LinkResolution::AmbiguousVersions
    } else {
        outcomes
            .into_iter()
            .next()
            .unwrap_or(LinkResolution::Inconclusive)
    };
    ResolvedLink {
        resolution,
        ..common
    }
}

/// Applies the documented fail-closed bucket precedence over one criterion's
/// resolved closing links.
fn bucket_for(resolutions: &[LinkResolution]) -> CriterionBucket {
    if resolutions.is_empty() {
        return CriterionBucket::Unverified;
    }
    let has = |wanted: LinkResolution| resolutions.contains(&wanted);
    if has(LinkResolution::Failing) {
        CriterionBucket::FailedEvidence
    } else if has(LinkResolution::Unresolved) {
        CriterionBucket::DanglingEvidence
    } else if has(LinkResolution::AmbiguousVersions) {
        // "Which version is current is unknown" is an inconclusive outcome, not
        // a failure: reporting `failed_evidence` would fabricate a failure the
        // store never recorded. Either way it is NOT proof.
        CriterionBucket::InconclusiveEvidence
    } else if has(LinkResolution::NotVerificationRecord) {
        CriterionBucket::NonVerificationEvidence
    } else if has(LinkResolution::Inconclusive) {
        CriterionBucket::InconclusiveEvidence
    } else {
        CriterionBucket::Proven
    }
}

/// Truncates one row list to `limit`, recording a `results_truncated` diagnostic
/// carrying the true pre-truncation count.
fn truncate_rows(
    rows: &mut Vec<CriterionRowJson>,
    limit: usize,
    noun: &str,
    diagnostics: &mut Vec<CriteriaCoverageDiagnostic>,
) {
    if rows.len() > limit {
        let total = rows.len();
        rows.truncate(limit);
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: "results_truncated".to_owned(),
            record_ids: Vec::new(),
            detail: format!(
                "showing {limit} of {total} {noun} rows; raise --limit to see the rest"
            ),
        });
    }
}

/// Runs the store-wide acceptance-criterion verification-coverage census
/// (issue #115).
///
/// Pure and deterministic. See the module documentation for the trust-separation
/// rule, the fail-closed bucket precedence, and what a coverage number is not.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn run_criteria_coverage(
    records: &[GraphRecord],
    config: &CriteriaCoverageConfig,
) -> CriteriaCoverageReport {
    let liveness = Liveness::new(records);

    // EVERY live physical version of each node, keyed by id. A graph file can
    // hold several writes of one id (and Egregore writes them SORTED, not in
    // append order — see `resolve_handle`), while the embedded `--data-dir` read
    // exposes only the current one. Keeping all of them lets the outcome
    // decision fail closed when they disagree instead of silently picking; a
    // rewritten criterion is still counted ONCE, because the census iterates
    // over ids.
    let mut node_versions: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node { id, .. } = record
            && !liveness.deleted(id.as_str())
        {
            node_versions.entry(id.as_str()).or_default().push(record);
        }
    }
    // The representative version of each id, used wherever the field read is
    // version-invariant in practice (kind, ownership, display metadata).
    let live_nodes: BTreeMap<&str, &GraphRecord> = node_versions
        .iter()
        .filter_map(|(id, versions)| versions.last().map(|last| (*id, *last)))
        .collect();

    // Live latest-version edges we join on. Edge-version selection is edge-only
    // (issue #391) so a same-id node write cannot shadow a live adjacency.
    let mut closing_edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut owning_edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let mut external_handles: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
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
        if liveness.deleted(id.as_str()) || !liveness.is_latest_edge_version(id.as_str(), index) {
            continue;
        }
        let bucket = match label {
            EdgeLabel::ClosesAcceptanceCriterion => &mut closing_edges,
            EdgeLabel::OwnedByTask => &mut owning_edges,
            EdgeLabel::ExternalHandle => &mut external_handles,
            _ => continue,
        };
        bucket
            .entry(source.as_str())
            .or_default()
            .insert(target.as_str());
    }

    let mut rows: Vec<CriterionRowJson> = Vec::new();
    let mut diagnostics: Vec<CriteriaCoverageDiagnostic> = Vec::new();
    let mut dangling_ids: Vec<String> = Vec::new();
    let mut parent_conflict_ids: Vec<String> = Vec::new();
    let mut parent_unresolved_ids: Vec<String> = Vec::new();
    let mut superseded_ids: Vec<String> = Vec::new();
    let mut bucket_counts: BTreeMap<String, usize> = CriterionBucket::ALL
        .iter()
        .map(|b| (b.as_wire().to_owned(), 0usize))
        .collect();

    // `live_nodes` is a BTreeMap, so iteration is record-ID ordered and the whole
    // pass is independent of input record order.
    for (&id, &record) in &live_nodes {
        let GraphRecord::Node {
            kind: NodeKind::AcceptanceCriterion,
            parent_task_id,
            ordinal,
            status,
            repo_relative_path,
            span,
            source_handle,
            ..
        } = record
        else {
            continue;
        };

        // Closing handles from BOTH representations: the edge and the
        // denormalized field the schema requires to match it.
        let mut origins_by_handle: BTreeMap<&str, BTreeSet<&'static str>> = BTreeMap::new();
        for target in closing_edges.get(id).into_iter().flatten() {
            origins_by_handle
                .entry(target)
                .or_default()
                .insert(ORIGIN_EDGE);
        }
        if let GraphRecord::Node {
            verification_link_id: Some(field_handle),
            ..
        } = record
        {
            origins_by_handle
                .entry(field_handle.as_str())
                .or_default()
                .insert(ORIGIN_FIELD);
        }

        let mut closing_links: Vec<ClosingLinkJson> = Vec::new();
        let mut resolutions: Vec<LinkResolution> = Vec::new();
        let mut passing_handles: Vec<&str> = Vec::new();
        for (&handle, origins) in &origins_by_handle {
            let resolved = resolve_handle(id, node_versions.get(handle));
            if resolved.resolution == LinkResolution::Passing {
                passing_handles.push(handle);
            }
            resolutions.push(resolved.resolution);
            closing_links.push(ClosingLinkJson {
                // A dangling handle is reported with its ORIGINAL text — which
                // is exactly the value NO writer validated — so it is
                // control-sanitized (never truncated; see `handle_field`).
                handle: handle_field(handle),
                origins: origins.iter().map(|o| (*o).to_owned()).collect(),
                resolution: resolved.resolution.as_wire().to_owned(),
                node_kind: resolved.node_kind.map(ToOwned::to_owned),
                verification_kind: resolved.verification_kind,
                status: resolved.status,
                exit_code: resolved.exit_code,
                producer_kind: resolved.producer_kind.map(ToOwned::to_owned),
            });
        }

        let bucket = bucket_for(&resolutions);
        // Keyed on the PRESENCE of an unresolved handle, not on the bucket: a
        // criterion carrying both a failing and a dangling handle lands in
        // `failed_evidence` by precedence, and keying on the bucket would drop
        // it from the diagnostic that promises to name EVERY criterion whose
        // closing handle resolves to no live record.
        if resolutions.contains(&LinkResolution::Unresolved) {
            dangling_ids.push(handle_field(id));
        }

        // Ownership from BOTH representations. The schema requires the
        // `OWNED_BY_TASK` target to equal `parent_task_id`; when they disagree the
        // field wins (it is the record's own claim) and the conflict is reported
        // rather than silently resolved.
        let edge_parents: BTreeSet<&str> = owning_edges.get(id).cloned().unwrap_or_default();
        let field_parent = parent_task_id.as_deref();
        let resolved_parent: Option<&str> = field_parent.or_else(|| edge_parents.first().copied());
        if let Some(field) = field_parent
            && !edge_parents.is_empty()
            && !edge_parents.contains(field)
        {
            parent_conflict_ids.push(handle_field(id));
        }
        // Reads the recorded status of one candidate parent, but only when that
        // id really resolves to a live `Task` — a non-Task node never lends its
        // `status` to a criterion.
        let task_status_of = |parent: &str| -> Option<&String> {
            match live_nodes.get(parent) {
                Some(GraphRecord::Node {
                    kind: NodeKind::Task,
                    status,
                    ..
                }) => status.as_ref(),
                _ => None,
            }
        };
        let parent_task_status = resolved_parent.and_then(task_status_of).cloned();
        if resolved_parent.is_none_or(|parent| {
            !matches!(
                live_nodes.get(parent),
                Some(GraphRecord::Node {
                    kind: NodeKind::Task,
                    ..
                })
            )
        }) {
            parent_unresolved_ids.push(handle_field(id));
        }

        // The claimed-done test runs over EVERY candidate parent — the
        // denormalized field AND every `OWNED_BY_TASK` edge target — not just the
        // one displayed. When the two representations disagree, letting the
        // displayed field alone decide would be FAIL-OPEN in the single metric
        // this command exists to enforce: a criterion whose edge names a
        // completed task would slip out of the gap set. The gate is the one
        // place ambiguity must widen the alarm, not narrow it.
        //
        // The comparison is trimmed and lowercased, matching the sibling
        // `verification_outcome` rule, so a `--graph` (which no importer
        // validated) cannot smuggle a criterion past the gate with
        // `"Closed_Completed"` or a stray leading space.
        let claimed_done = field_parent
            .into_iter()
            .chain(edge_parents.iter().copied())
            .filter_map(task_status_of)
            .any(|status| {
                DONE_TASK_STATUSES.contains(&status.trim().to_ascii_lowercase().as_str())
            })
            && bucket != CriterionBucket::Proven;

        // A criterion recorded `superseded` was REPLACED, not left unproven —
        // the same argument that excludes `closed_dropped` from the done set.
        // It is still counted (dropping it would silently shrink the census),
        // but the asymmetry is disclosed in the output rather than left to a
        // reader to discover from the absence of a rule.
        if status
            .as_deref()
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("superseded"))
            && bucket != CriterionBucket::Proven
        {
            superseded_ids.push(handle_field(id));
        }

        *bucket_counts
            .entry(bucket.as_wire().to_owned())
            .or_insert(0) += 1;

        // Every free-text store value is bounded and every handle is
        // control-sanitized HERE, in the pure core, so `--format json` and
        // `--format text` inherit the same guarantee and neither transport can
        // render a raw store value. `status` in particular is a documented FREE
        // STRING with no enum enforcement at v1.
        rows.push(CriterionRowJson {
            record_id: handle_field(id),
            parent_task_id: resolved_parent.map(handle_field),
            parent_task_status: parent_task_status.as_deref().map(bounded_field),
            criterion_status: status.as_deref().map(bounded_field),
            ordinal: *ordinal,
            bucket: bucket.as_wire().to_owned(),
            closing_links,
            // Only a `proven` row names a proving record: on any other bucket the
            // deciding evidence is not proof, so naming one would overclaim.
            proving_verification_id: (bucket == CriterionBucket::Proven)
                .then(|| passing_handles.first().map(|h| handle_field(h)))
                .flatten(),
            repo_relative_path: repo_relative_path.as_deref().map(handle_field),
            span: *span,
            source_handle: source_handle.as_deref().map(handle_field),
            // Liveness-gated like every other cited target: an `EXTERNAL_HANDLE`
            // edge can outlive the `ExternalLink` node it names, and emitting a
            // handle to a record that no longer exists would be a dead citation.
            external_link_id: external_handles
                .get(id)
                .and_then(|links| links.iter().find(|l| live_nodes.contains_key(*l)))
                .map(|l| handle_field(l)),
            claimed_done_unproven: claimed_done,
        });
    }

    rows.sort_by(|a, b| a.record_id.cmp(&b.record_id));

    let total_criteria = rows.len();
    let count_of = |bucket: CriterionBucket| -> usize {
        bucket_counts.get(bucket.as_wire()).copied().unwrap_or(0)
    };
    let proven_count = count_of(CriterionBucket::Proven);

    let mut claimed_done_unproven: Vec<CriterionRowJson> = rows
        .iter()
        .filter(|r| r.claimed_done_unproven)
        .cloned()
        .collect();
    let claimed_done_unproven_count = claimed_done_unproven.len();

    // ── diagnostics ─────────────────────────────────────────────────────────
    if total_criteria == 0 {
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: "no_acceptance_criteria".to_owned(),
            record_ids: Vec::new(),
            detail: "the store records no live acceptance criteria; coverage ratios have a zero \
                     denominator and are reported as null. This is not a failure — it means no \
                     criteria were imported, not that none exist"
                .to_owned(),
        });
    }
    if !dangling_ids.is_empty() {
        dangling_ids.sort();
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: "dangling_closing_evidence".to_owned(),
            record_ids: dangling_ids,
            detail: "criterion(s) carry a closing verification handle that resolves to no live \
                     record; reported in their own bucket with the original handles, never \
                     counted proven and never merged into unverified"
                .to_owned(),
        });
    }
    if !parent_conflict_ids.is_empty() {
        parent_conflict_ids.sort();
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: "criterion_parent_task_conflict".to_owned(),
            record_ids: parent_conflict_ids,
            detail: "criterion(s) whose OWNED_BY_TASK edge target differs from parent_task_id; \
                     the recorded field wins and the conflict is reported"
                .to_owned(),
        });
    }
    if !superseded_ids.is_empty() {
        superseded_ids.sort();
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: "superseded_criteria_counted".to_owned(),
            record_ids: superseded_ids,
            detail: "criterion(s) recorded `status: superseded` are COUNTED in the proof gap; \
                     unlike a `closed_dropped` task, a superseded criterion is not excluded, so \
                     the gap may include requirements that were replaced rather than left unproven"
                .to_owned(),
        });
    }
    if !parent_unresolved_ids.is_empty() {
        parent_unresolved_ids.sort();
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: "criterion_parent_task_unresolved".to_owned(),
            record_ids: parent_unresolved_ids,
            detail: "criterion(s) whose owning Task does not resolve to a live Task record; they \
                     are still counted in the census but can never enter the \
                     claimed-done-but-unproven set"
                .to_owned(),
        });
    }

    // ── gate ────────────────────────────────────────────────────────────────
    let proven_ratio = ratio(proven_count, total_criteria);
    let mut breaches: Vec<CriteriaCoverageBreach> = Vec::new();
    // A zero denominator is a VACUOUS pass: with no criteria imported there is no
    // proof gap to report, and inventing a breach from an absent population would
    // be the same overclaim in the other direction.
    // Each breach mints its paired diagnostic from the SAME message, so the two
    // renderings of one gate failure cannot drift apart.
    let mut breach = |metric: &str,
                      code: &str,
                      observed: String,
                      bound: String,
                      message: String,
                      record_ids: Vec<String>| {
        diagnostics.push(CriteriaCoverageDiagnostic {
            code: code.to_owned(),
            record_ids,
            detail: message.clone(),
        });
        breaches.push(CriteriaCoverageBreach {
            metric: metric.to_owned(),
            observed,
            bound,
            message,
        });
    };
    if let Some(observed) = proven_ratio.ratio
        && observed < config.min_proven_ratio
    {
        breach(
            "proven_ratio",
            "below_proven_ratio_threshold",
            format!("{observed:.4}"),
            format!("{:.4}", config.min_proven_ratio),
            format!(
                "proven ratio {observed:.4} ({proven_count}/{total_criteria}) is below the \
                 required minimum {:.4}",
                config.min_proven_ratio
            ),
            Vec::new(),
        );
    }
    if claimed_done_unproven_count > config.max_claimed_done_unproven {
        breach(
            "claimed_done_unproven",
            "above_claimed_done_unproven_threshold",
            claimed_done_unproven_count.to_string(),
            config.max_claimed_done_unproven.to_string(),
            format!(
                "{claimed_done_unproven_count} criterion(s) are owned by a closed/done task yet \
                 carry no passing verification evidence; the allowed maximum is {}",
                config.max_claimed_done_unproven
            ),
            claimed_done_unproven
                .iter()
                .map(|r| r.record_id.clone())
                .collect(),
        );
    }
    let ok = breaches.is_empty();

    // ── shape the output ────────────────────────────────────────────────────
    let mut criteria = rows;
    truncate_rows(&mut criteria, config.limit, "criteria", &mut diagnostics);
    truncate_rows(
        &mut claimed_done_unproven,
        config.limit,
        "claimed_done_unproven",
        &mut diagnostics,
    );

    breaches.sort_by(|a, b| a.metric.cmp(&b.metric));
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(&b.code)
            .then_with(|| a.record_ids.cmp(&b.record_ids))
    });

    CriteriaCoverageReport {
        ok,
        total_criteria,
        proven: proven_ratio,
        unverified: ratio(count_of(CriterionBucket::Unverified), total_criteria),
        failed_evidence: ratio(count_of(CriterionBucket::FailedEvidence), total_criteria),
        dangling_evidence: ratio(count_of(CriterionBucket::DanglingEvidence), total_criteria),
        non_verification_evidence: ratio(
            count_of(CriterionBucket::NonVerificationEvidence),
            total_criteria,
        ),
        inconclusive_evidence: ratio(
            count_of(CriterionBucket::InconclusiveEvidence),
            total_criteria,
        ),
        proof_gap: ratio(total_criteria - proven_count, total_criteria),
        bucket_counts,
        done_task_statuses: DONE_TASK_STATUSES.iter().map(|s| (*s).to_owned()).collect(),
        claimed_done_unproven_count,
        claimed_done_unproven,
        criteria,
        thresholds: CriteriaCoverageThresholds {
            min_proven_ratio: config.min_proven_ratio,
            max_claimed_done_unproven: config.max_claimed_done_unproven,
        },
        breaches,
        diagnostics,
        disclaimer: CRITERIA_COVERAGE_DISCLAIMER.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, NodeKind, PROJECT_SCHEMA_VERSION,
        VERIFICATION_SCHEMA_VERSION,
    };

    // ── fixture builders ────────────────────────────────────────────────────

    fn task(id: &str, status: &str) -> GraphRecord {
        let mut r = GraphRecord::node(
            id.to_owned(),
            NodeKind::Task,
            None,
            None,
            None,
            "task".to_owned(),
        )
        .with_domain("project", PROJECT_SCHEMA_VERSION);
        if let GraphRecord::Node {
            status: s, title, ..
        } = &mut r
        {
            *s = Some(status.to_owned());
            // A secret title that must never escape into the report.
            *title = Some("SECRET-TASK-TITLE".to_owned());
        }
        r
    }

    fn criterion(id: &str, parent: &str, ordinal: u32, status: &str) -> GraphRecord {
        let mut r = GraphRecord::node(
            id.to_owned(),
            NodeKind::AcceptanceCriterion,
            None,
            None,
            None,
            "criterion".to_owned(),
        )
        .with_domain("project", PROJECT_SCHEMA_VERSION);
        if let GraphRecord::Node {
            parent_task_id,
            ordinal: o,
            status: s,
            text,
            ..
        } = &mut r
        {
            *parent_task_id = Some(parent.to_owned());
            *o = Some(ordinal);
            *s = Some(status.to_owned());
            // Verbatim falsifiable claim: must never escape into the report.
            *text = Some("SECRET-CRITERION-TEXT".to_owned());
        }
        r
    }

    fn with_link_field(mut r: GraphRecord, target: &str) -> GraphRecord {
        if let GraphRecord::Node {
            verification_link_id,
            ..
        } = &mut r
        {
            *verification_link_id = Some(target.to_owned());
        }
        r
    }

    fn verification(
        id: &str,
        kind: NodeKind,
        status: Option<&str>,
        code: Option<i64>,
    ) -> GraphRecord {
        let mut r = GraphRecord::node(id.to_owned(), kind, None, None, None, "run".to_owned())
            .with_domain("verification", VERIFICATION_SCHEMA_VERSION);
        if let GraphRecord::Node {
            status: s,
            exit_code,
            text,
            ..
        } = &mut r
        {
            *s = status.map(ToOwned::to_owned);
            *exit_code = code;
            *text = Some("SECRET-COMMAND-OUTPUT".to_owned());
        }
        r
    }

    /// A verification-SHAPED node minted in the agent-memory domain (what
    /// `eg command-evidence` writes) — an agent's own claim about itself.
    fn agent_minted_evidence(id: &str) -> GraphRecord {
        let mut r = GraphRecord::node(
            id.to_owned(),
            NodeKind::CommandEvidence,
            None,
            None,
            None,
            "self-signed".to_owned(),
        )
        .with_domain("agent_memory", AGENT_MEMORY_SCHEMA_VERSION);
        if let GraphRecord::Node {
            status, exit_code, ..
        } = &mut r
        {
            *status = Some("pass".to_owned());
            *exit_code = Some(0);
        }
        r
    }

    fn closes(ac: &str, verification_id: &str) -> GraphRecord {
        GraphRecord::project_edge(
            EdgeLabel::ClosesAcceptanceCriterion,
            ac.to_owned(),
            verification_id.to_owned(),
            None,
            "closes".to_owned(),
        )
    }

    fn owned_by(ac: &str, task_id: &str) -> GraphRecord {
        GraphRecord::project_edge(
            EdgeLabel::OwnedByTask,
            ac.to_owned(),
            task_id.to_owned(),
            None,
            "owned by".to_owned(),
        )
    }

    fn tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("project:v{PROJECT_SCHEMA_VERSION}:tomb-{deleted_id}"),
            schema_version: PROJECT_SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    const AC_P1: &str = "project:v1:ac-p1";
    const AC_P2: &str = "project:v1:ac-p2";
    const AC_U1: &str = "project:v1:ac-u1";
    const AC_U2: &str = "project:v1:ac-u2";
    const AC_F1: &str = "project:v1:ac-f1";
    const AC_D1: &str = "project:v1:ac-d1";
    const AC_D2: &str = "project:v1:ac-d2";
    const AC_N1: &str = "project:v1:ac-n1";
    const AC_I1: &str = "project:v1:ac-i1";
    const AC_X1: &str = "project:v1:ac-x1";

    const TASK_DONE: &str = "project:v1:task-done";
    const TASK_OPEN: &str = "project:v1:task-open";
    const TASK_DROPPED: &str = "project:v1:task-dropped";

    const VER_PASS_1: &str = "verification:v1:pass1";
    const VER_PASS_2: &str = "verification:v1:pass2";
    const VER_FAIL_1: &str = "verification:v1:fail1";
    const VER_GHOST: &str = "verification:v1:ghost";
    const VER_TOMBED: &str = "verification:v1:tombed";
    const VER_INCON: &str = "verification:v1:incon";
    const AGENT_EVIDENCE: &str = "agent_memory:v1:selfsigned";

    /// The labeled fixture: 10 live criteria across three tasks, one per case.
    ///
    /// | criterion | task | expected bucket |
    /// |---|---|---|
    /// | `ac-p1` | done | `proven` (edge → passing `TestRun`) |
    /// | `ac-p2` | open | `proven` (field → passing `CommandRun`) |
    /// | `ac-u1` | done | `unverified` (no links) |
    /// | `ac-u2` | open | `unverified` (no links, but `status: verified`) |
    /// | `ac-f1` | done | `failed_evidence` |
    /// | `ac-d1` | open | `dangling_evidence` (absent target) |
    /// | `ac-d2` | done | `dangling_evidence` (tombstoned target) |
    /// | `ac-n1` | open | `non_verification_evidence` (agent-minted) |
    /// | `ac-i1` | open | `inconclusive_evidence` |
    /// | `ac-x1` | dropped | `unverified`, NOT claimed-done |
    fn planted() -> Vec<GraphRecord> {
        vec![
            task(TASK_DONE, "closed_completed"),
            task(TASK_OPEN, "open"),
            task(TASK_DROPPED, "closed_dropped"),
            verification(VER_PASS_1, NodeKind::TestRun, Some("pass"), None),
            verification(VER_PASS_2, NodeKind::CommandRun, None, Some(0)),
            verification(VER_FAIL_1, NodeKind::TestRun, Some("fail"), None),
            verification(VER_TOMBED, NodeKind::TestRun, Some("pass"), None),
            verification(
                VER_INCON,
                NodeKind::Verification,
                Some("inconclusive"),
                None,
            ),
            agent_minted_evidence(AGENT_EVIDENCE),
            tombstone(VER_TOMBED),
            criterion(AC_P1, TASK_DONE, 0, "verified"),
            closes(AC_P1, VER_PASS_1),
            owned_by(AC_P1, TASK_DONE),
            with_link_field(criterion(AC_P2, TASK_OPEN, 0, "verified"), VER_PASS_2),
            owned_by(AC_P2, TASK_OPEN),
            criterion(AC_U1, TASK_DONE, 1, "unverified"),
            owned_by(AC_U1, TASK_DONE),
            // The lie test: recorded `verified` with no closing evidence at all.
            criterion(AC_U2, TASK_OPEN, 1, "verified"),
            owned_by(AC_U2, TASK_OPEN),
            criterion(AC_F1, TASK_DONE, 2, "failed"),
            closes(AC_F1, VER_FAIL_1),
            owned_by(AC_F1, TASK_DONE),
            criterion(AC_D1, TASK_OPEN, 2, "verified"),
            closes(AC_D1, VER_GHOST),
            owned_by(AC_D1, TASK_OPEN),
            criterion(AC_D2, TASK_DONE, 3, "verified"),
            closes(AC_D2, VER_TOMBED),
            owned_by(AC_D2, TASK_DONE),
            criterion(AC_N1, TASK_OPEN, 3, "verified"),
            closes(AC_N1, AGENT_EVIDENCE),
            owned_by(AC_N1, TASK_OPEN),
            criterion(AC_I1, TASK_OPEN, 4, "unknown"),
            closes(AC_I1, VER_INCON),
            owned_by(AC_I1, TASK_OPEN),
            criterion(AC_X1, TASK_DROPPED, 0, "unverified"),
            owned_by(AC_X1, TASK_DROPPED),
        ]
    }

    fn report() -> CriteriaCoverageReport {
        run_criteria_coverage(&planted(), &CriteriaCoverageConfig::default())
    }

    fn row<'a>(report: &'a CriteriaCoverageReport, id: &str) -> &'a CriterionRowJson {
        report
            .criteria
            .iter()
            .find(|r| r.record_id == id)
            .unwrap_or_else(|| panic!("row for {id} present"))
    }

    // ── AC2: buckets, numerator/denominator, zero-denominator ───────────────

    #[test]
    fn every_criterion_lands_in_its_labeled_bucket() {
        let report = report();
        assert_eq!(report.total_criteria, 10);
        for (id, expected) in [
            (AC_P1, "proven"),
            (AC_P2, "proven"),
            (AC_U1, "unverified"),
            (AC_U2, "unverified"),
            (AC_F1, "failed_evidence"),
            (AC_D1, "dangling_evidence"),
            (AC_D2, "dangling_evidence"),
            (AC_N1, "non_verification_evidence"),
            (AC_I1, "inconclusive_evidence"),
            (AC_X1, "unverified"),
        ] {
            assert_eq!(row(&report, id).bucket, expected, "bucket for {id}");
        }
    }

    #[test]
    fn buckets_are_mutually_exclusive_and_partition_the_census() {
        let report = report();
        let sum: usize = report.bucket_counts.values().sum();
        assert_eq!(sum, report.total_criteria, "buckets must partition");
        // Every bucket is present in the tally even at zero.
        for bucket in CriterionBucket::ALL {
            assert!(
                report.bucket_counts.contains_key(bucket.as_wire()),
                "bucket {} missing from tally",
                bucket.as_wire()
            );
        }
        assert_eq!(report.criteria.len(), report.total_criteria);
    }

    #[test]
    fn ratios_carry_numerator_and_denominator() {
        let report = report();
        assert_eq!(report.proven.numerator, 2);
        assert_eq!(report.proven.denominator, 10);
        assert!((report.proven.ratio.expect("ratio") - 0.2).abs() < 1e-9);
        assert_eq!(report.unverified.numerator, 3);
        assert_eq!(report.failed_evidence.numerator, 1);
        assert_eq!(report.dangling_evidence.numerator, 2);
        assert_eq!(report.non_verification_evidence.numerator, 1);
        assert_eq!(report.inconclusive_evidence.numerator, 1);
        assert_eq!(report.proof_gap.numerator, 8);
        assert_eq!(report.proof_gap.denominator, 10);
    }

    #[test]
    fn zero_denominator_is_a_stable_diagnostic_never_a_divide_by_zero() {
        let report = run_criteria_coverage(&[], &CriteriaCoverageConfig::default());
        assert_eq!(report.total_criteria, 0);
        assert!(report.proven.ratio.is_none(), "ratio must be null, not NaN");
        assert!(report.proof_gap.ratio.is_none());
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "no_acceptance_criteria"),
            "empty store must carry the stable diagnostic"
        );
        assert!(
            report.ok,
            "an empty store is a vacuous pass, never a breach"
        );
    }

    // ── AC3: claimed-done-but-unproven ──────────────────────────────────────

    #[test]
    fn claimed_done_unproven_names_exactly_the_closed_task_gap() {
        let report = report();
        let ids: Vec<&str> = report
            .claimed_done_unproven
            .iter()
            .map(|r| r.record_id.as_str())
            .collect();
        assert_eq!(ids, vec![AC_D2, AC_F1, AC_U1], "sorted by record ID");
        assert_eq!(report.claimed_done_unproven_count, 3);
        // A proven criterion under a done task is NOT in the set.
        assert!(!ids.contains(&AC_P1));
        // A criterion under a DROPPED task claims no completion.
        assert!(!ids.contains(&AC_X1));
        assert_eq!(report.done_task_statuses, vec!["closed_completed"]);
    }

    #[test]
    fn claimed_done_rows_cite_criterion_task_and_verification_handles() {
        let report = report();
        for r in &report.claimed_done_unproven {
            assert!(!r.record_id.is_empty(), "criterion record ID cited");
            assert_eq!(
                r.parent_task_id.as_deref(),
                Some(TASK_DONE),
                "parent task record ID cited"
            );
            assert!(r.claimed_done_unproven);
        }
        // Where a closing verification record exists, it is cited.
        let failed = report
            .claimed_done_unproven
            .iter()
            .find(|r| r.record_id == AC_F1)
            .expect("failed row");
        assert_eq!(
            failed
                .closing_links
                .iter()
                .map(|l| l.handle.as_str())
                .collect::<Vec<_>>(),
            vec![VER_FAIL_1]
        );
        // A dangling row keeps the ORIGINAL handle.
        let dangling = report
            .claimed_done_unproven
            .iter()
            .find(|r| r.record_id == AC_D2)
            .expect("dangling row");
        assert_eq!(dangling.closing_links[0].handle, VER_TOMBED);
        assert_eq!(dangling.closing_links[0].resolution, "unresolved");
    }

    // ── AC4: trust separation ───────────────────────────────────────────────

    #[test]
    fn recorded_criterion_status_never_confers_proof() {
        // `ac-u2` records `status: verified` with NO closing evidence. Reading
        // the status field would report it proven; only edges may.
        let report = report();
        assert_eq!(row(&report, AC_U2).bucket, "unverified");
        assert_eq!(
            row(&report, AC_U2).criterion_status.as_deref(),
            Some("verified"),
            "the recorded status is echoed for citation, but never drives the bucket"
        );
    }

    #[test]
    fn agent_minted_evidence_cannot_self_certify_a_criterion() {
        // An `agent_memory:v1:` CommandEvidence claiming `pass`/exit 0 must not
        // prove a criterion: an agent-authored claim is never evidence for itself.
        let report = report();
        assert_eq!(row(&report, AC_N1).bucket, "non_verification_evidence");
        assert_eq!(
            row(&report, AC_N1).closing_links[0].resolution,
            "not_verification_record"
        );
        assert!(row(&report, AC_N1).proving_verification_id.is_none());
    }

    #[test]
    fn closed_task_status_never_confers_proof() {
        // Every criterion under the done task that lacks passing evidence stays
        // in a non-proven bucket regardless of `Task.status: closed_completed`.
        let report = report();
        for id in [AC_U1, AC_F1, AC_D2] {
            assert_ne!(row(&report, id).bucket, "proven", "{id} proven from status");
        }
    }

    #[test]
    fn failing_evidence_dominates_a_co_present_passing_link() {
        // Fail-closed precedence: a criterion carrying BOTH a passing and a
        // failing closing link is never reported proven.
        let mut records = planted();
        records.push(closes(AC_P1, VER_FAIL_1));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(row(&report, AC_P1).bucket, "failed_evidence");
        assert_eq!(row(&report, AC_P1).closing_links.len(), 2);
    }

    // ── AC5: dangling edges ─────────────────────────────────────────────────

    #[test]
    fn dangling_edges_are_their_own_bucket_with_original_handles() {
        let report = report();
        for (id, handle) in [(AC_D1, VER_GHOST), (AC_D2, VER_TOMBED)] {
            let r = row(&report, id);
            assert_eq!(r.bucket, "dangling_evidence");
            assert_eq!(r.closing_links[0].handle, handle);
            assert_eq!(r.closing_links[0].resolution, "unresolved");
        }
        // Never merged into unverified, never counted proven.
        assert_eq!(report.unverified.numerator, 3);
        assert_eq!(report.proven.numerator, 2);
    }

    // ── AC6: citable rows ───────────────────────────────────────────────────

    #[test]
    fn every_row_carries_a_citable_record_id() {
        let report = report();
        for r in &report.criteria {
            assert!(r.record_id.starts_with("project:v"), "{}", r.record_id);
            assert!(r.parent_task_id.is_some(), "{} parent", r.record_id);
        }
    }

    #[test]
    fn ownership_resolves_from_the_edge_when_the_field_is_absent() {
        // `OWNED_BY_TASK` alone must establish ownership (the schema keeps the
        // edge and the denormalized field in sync; either is sufficient).
        let mut records = planted();
        for r in &mut records {
            if let GraphRecord::Node {
                id, parent_task_id, ..
            } = r
                && id == AC_U1
            {
                *parent_task_id = None;
            }
        }
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(
            row(&report, AC_U1).parent_task_id.as_deref(),
            Some(TASK_DONE)
        );
        assert!(row(&report, AC_U1).claimed_done_unproven);
    }

    // ── AC7: threshold gate ─────────────────────────────────────────────────

    #[test]
    fn gate_trips_on_the_planted_proof_gap_naming_metric_and_value() {
        let report = report();
        assert!(!report.ok, "a store over the proof-gap line is never ready");
        let metrics: Vec<&str> = report.breaches.iter().map(|b| b.metric.as_str()).collect();
        assert!(metrics.contains(&"proven_ratio"), "{metrics:?}");
        assert!(metrics.contains(&"claimed_done_unproven"), "{metrics:?}");
        let proven = report
            .breaches
            .iter()
            .find(|b| b.metric == "proven_ratio")
            .expect("proven breach");
        assert!(proven.observed.contains("0.2"), "{}", proven.observed);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "below_proven_ratio_threshold")
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "above_claimed_done_unproven_threshold")
        );
    }

    #[test]
    fn gate_passes_under_relaxed_bounds() {
        let config = CriteriaCoverageConfig {
            min_proven_ratio: 0.1,
            max_claimed_done_unproven: 5,
            limit: CRITERIA_COVERAGE_DEFAULT_LIMIT,
        };
        let report = run_criteria_coverage(&planted(), &config);
        assert!(report.ok, "breaches={:?}", report.breaches);
        assert!(report.breaches.is_empty());
    }

    #[test]
    fn gate_passes_on_a_fully_proven_store() {
        let records = vec![
            task(TASK_DONE, "closed_completed"),
            verification(VER_PASS_1, NodeKind::TestRun, Some("passed"), None),
            criterion(AC_P1, TASK_DONE, 0, "verified"),
            closes(AC_P1, VER_PASS_1),
            owned_by(AC_P1, TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert!(report.ok, "breaches={:?}", report.breaches);
        assert_eq!(report.proven.numerator, 1);
        assert!((report.proven.ratio.expect("ratio") - 1.0).abs() < 1e-9);
        assert_eq!(report.claimed_done_unproven_count, 0);
    }

    // ── AC8: redaction safety ───────────────────────────────────────────────

    #[test]
    fn output_never_leaks_prose_bodies() {
        let json = serde_json::to_string(&report()).expect("serialize");
        for secret in [
            "SECRET-CRITERION-TEXT",
            "SECRET-TASK-TITLE",
            "SECRET-COMMAND-OUTPUT",
        ] {
            assert!(!json.contains(secret), "report leaked {secret}");
        }
    }

    // ── AC9: determinism ────────────────────────────────────────────────────

    #[test]
    fn output_is_byte_identical_across_five_runs() {
        let records = planted();
        let first = serde_json::to_string(&run_criteria_coverage(
            &records,
            &CriteriaCoverageConfig::default(),
        ))
        .expect("serialize");
        for _ in 0..5 {
            let again = serde_json::to_string(&run_criteria_coverage(
                &records,
                &CriteriaCoverageConfig::default(),
            ))
            .expect("serialize");
            assert_eq!(
                first, again,
                "criteria-coverage output must be byte-identical"
            );
        }
    }

    #[test]
    fn record_order_does_not_change_the_answer() {
        // Append ORDER is deliberately meaningful for one thing only: a tombstone
        // versus a later re-write of the same id (latest-write-wins liveness). So
        // this scrambles every node and edge while keeping tombstones after their
        // targets — the report must be identical, proving nothing else in the
        // derivation depends on iteration order.
        let forward = run_criteria_coverage(&planted(), &CriteriaCoverageConfig::default());
        let (tombstones, mut rest): (Vec<_>, Vec<_>) = planted()
            .into_iter()
            .partition(|r| matches!(r, GraphRecord::Tombstone { .. }));
        rest.reverse();
        rest.extend(tombstones);
        let scrambled = run_criteria_coverage(&rest, &CriteriaCoverageConfig::default());
        assert_eq!(
            serde_json::to_string(&forward).expect("a"),
            serde_json::to_string(&scrambled).expect("b"),
            "ordering of input records must not change the report"
        );
    }

    // ── liveness ────────────────────────────────────────────────────────────

    #[test]
    fn tombstoned_criteria_leave_the_census() {
        let mut records = planted();
        records.push(tombstone(AC_U1));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(report.total_criteria, 9);
        assert!(report.criteria.iter().all(|r| r.record_id != AC_U1));
    }

    #[test]
    fn a_criterion_reingested_after_its_tombstone_is_live_again() {
        let mut records = planted();
        records.push(tombstone(AC_U1));
        records.push(criterion(AC_U1, TASK_DONE, 1, "unverified"));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(report.total_criteria, 10);
    }

    #[test]
    fn a_retracted_closing_edge_stops_proving() {
        let mut records = planted();
        let edge_id = closes(AC_P1, VER_PASS_1).id().to_owned();
        records.push(tombstone(&edge_id));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(
            row(&report, AC_P1).bucket,
            "unverified",
            "a retracted closing edge withdraws the proof"
        );
    }

    #[test]
    fn duplicate_criterion_versions_are_counted_once() {
        let mut records = planted();
        records.push(criterion(AC_U1, TASK_DONE, 1, "unverified"));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(
            report.total_criteria, 10,
            "an append-only rewrite is one criterion"
        );
    }

    // ── limits ──────────────────────────────────────────────────────────────

    #[test]
    fn limit_truncates_with_a_diagnostic_that_keeps_the_true_count() {
        let config = CriteriaCoverageConfig {
            limit: 2,
            ..CriteriaCoverageConfig::default()
        };
        let report = run_criteria_coverage(&planted(), &config);
        assert_eq!(report.total_criteria, 10, "counts are pre-truncation");
        assert_eq!(report.criteria.len(), 2);
        assert_eq!(report.claimed_done_unproven_count, 3);
        assert_eq!(report.claimed_done_unproven.len(), 2);
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "results_truncated")
        );
    }

    // ── review round 2: the gate is three checks, not one ───────────────────

    /// A node carrying a `verification:v1:` ID but a NON-verification kind must
    /// never prove a criterion.
    ///
    /// The embedded ingest path does not run the daemon's
    /// `validate_verification_domain_records`, so such a record really can sit
    /// in a store. The ID-prefix check alone would certify it.
    #[test]
    fn a_verification_prefixed_non_verification_kind_is_not_proof() {
        for kind in [
            NodeKind::Observation,
            NodeKind::Task,
            NodeKind::AcceptanceCriterion,
            NodeKind::Agent,
        ] {
            let impostor = verification("verification:v1:impostor", kind, Some("pass"), Some(0));
            let records = vec![
                task(TASK_DONE, "closed_completed"),
                impostor,
                criterion(AC_P1, TASK_DONE, 0, "verified"),
                closes(AC_P1, "verification:v1:impostor"),
                owned_by(AC_P1, TASK_DONE),
            ];
            let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
            assert_eq!(
                row(&report, AC_P1).bucket,
                "non_verification_evidence",
                "a {kind:?} with a verification: prefix must not prove"
            );
            assert_eq!(
                row(&report, AC_P1).closing_links[0].resolution,
                "not_verification_record"
            );
            assert!(row(&report, AC_P1).proving_verification_id.is_none());
            assert!(!report.ok, "and the gate must still fail");
        }
    }

    /// A criterion whose closing handle is its OWN record ID is never proof.
    #[test]
    fn a_criterion_can_never_close_itself() {
        let self_closing = with_link_field(
            criterion("verification:v1:self", TASK_DONE, 0, "verified"),
            "verification:v1:self",
        );
        let records = vec![
            task(TASK_DONE, "closed_completed"),
            self_closing,
            owned_by("verification:v1:self", TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        let row = &report.criteria[0];
        assert_eq!(row.bucket, "non_verification_evidence");
        assert!(row.proving_verification_id.is_none());
        assert!(row.claimed_done_unproven);
        assert!(!report.ok);
    }

    /// Two live versions of one closing record that DISAGREE about the outcome
    /// must never resolve to proof.
    ///
    /// Egregore writes graph JSONL sorted, so position carries no information
    /// about which version is current; silently picking one could hand back
    /// `proven` for a criterion the store records as failed.
    #[test]
    fn disagreeing_versions_of_one_record_fail_closed() {
        let mut records = planted();
        // A SECOND physical write of the passing record, now failing.
        records.push(verification(
            VER_PASS_1,
            NodeKind::TestRun,
            Some("fail"),
            None,
        ));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(
            row(&report, AC_P1).bucket,
            "inconclusive_evidence",
            "version ambiguity is never proof"
        );
        assert_eq!(
            row(&report, AC_P1).closing_links[0].resolution,
            "ambiguous_versions"
        );
        assert!(row(&report, AC_P1).proving_verification_id.is_none());
    }

    /// Agreeing duplicate versions stay decisive — the ambiguity rule must not
    /// punish an ordinary idempotent re-ingest.
    #[test]
    fn agreeing_duplicate_versions_still_prove() {
        let mut records = planted();
        records.push(verification(
            VER_PASS_1,
            NodeKind::TestRun,
            Some("passed"),
            None,
        ));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(row(&report, AC_P1).bucket, "proven");
    }

    /// The closing record's writing subsystem is disclosed, because the
    /// verification-domain gate does NOT establish independent observation.
    #[test]
    fn the_closing_records_producer_is_disclosed() {
        let mut passing = verification(VER_PASS_1, NodeKind::TestRun, Some("pass"), None);
        if let GraphRecord::Node { producer, .. } = &mut passing {
            *producer = Some(crate::ir::Producer {
                egregore_version: "0.0.0".to_owned(),
                egregore_git: None,
                producer_kind: crate::ir::ProducerKind::ClaudeCodeImporter,
                producer_components: std::collections::BTreeMap::new(),
                producer_started_at: "2026-01-01T00:00:00Z".to_owned(),
            });
        }
        let records = vec![
            task(TASK_DONE, "closed_completed"),
            passing,
            criterion(AC_P1, TASK_DONE, 0, "verified"),
            closes(AC_P1, VER_PASS_1),
            owned_by(AC_P1, TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(row(&report, AC_P1).bucket, "proven");
        assert_eq!(
            row(&report, AC_P1).closing_links[0]
                .producer_kind
                .as_deref(),
            Some("claude_code_importer"),
            "a transcript-derived 'proof' must disclose its origin"
        );
    }

    // ── review round 2: hardening of operator-controlled values ─────────────

    /// Free-text store values are control-sanitized AND length-capped.
    ///
    /// `docs/schema/verification.md` documents `status` as a free string with no
    /// enum enforcement, so a crafted record could otherwise forge output lines
    /// in `--format text` or drive the reader's terminal.
    #[test]
    fn free_text_store_values_are_sanitized_and_bounded() {
        let hostile = format!("pass\u{1b}[2J\nok: true\n{}", "A".repeat(5000));
        let mut evil = verification("verification:v1:evil", NodeKind::TestRun, None, None);
        if let GraphRecord::Node {
            status,
            verification_kind,
            ..
        } = &mut evil
        {
            *status = Some(hostile.clone());
            *verification_kind = Some(hostile.clone());
        }
        let mut crit = criterion(AC_P1, TASK_DONE, 0, &hostile);
        if let GraphRecord::Node { source_handle, .. } = &mut crit {
            *source_handle = Some("handle\nforged: line".to_owned());
        }
        let records = vec![
            task(TASK_DONE, &hostile),
            evil,
            crit,
            closes(AC_P1, "verification:v1:evil"),
            owned_by(AC_P1, TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        let r = row(&report, AC_P1);
        for value in [
            r.criterion_status.as_deref(),
            r.parent_task_status.as_deref(),
            r.closing_links[0].status.as_deref(),
            r.closing_links[0].verification_kind.as_deref(),
        ] {
            let value = value.expect("value present");
            assert!(
                !value.chars().any(char::is_control),
                "control characters survived: {value:?}"
            );
            assert!(
                value.chars().count() <= CRITERIA_FIELD_MAX_CHARS + 1,
                "value not bounded: {} chars",
                value.chars().count()
            );
        }
        // Handles are sanitized but NOT truncated: a truncated handle is not a
        // citation.
        let handle = r.source_handle.as_deref().expect("source handle");
        assert!(!handle.chars().any(char::is_control));
        assert!(handle.contains("forged"), "handle must not be truncated");
    }

    /// A record ID carrying control characters is sanitized but kept whole, so
    /// it stays a usable citation.
    #[test]
    fn record_ids_are_sanitized_but_never_truncated() {
        let long_id = format!("project:v1:{}", "z".repeat(400));
        let records = vec![
            task(TASK_DONE, "closed_completed"),
            criterion(&long_id, TASK_DONE, 0, "unverified"),
            owned_by(&long_id, TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(
            report.criteria[0].record_id, long_id,
            "a record ID must survive whole to remain citable"
        );
    }

    // ── review round 2: diagnostics and ownership ───────────────────────────

    /// The dangling diagnostic names every criterion carrying an unresolved
    /// handle, even when a higher-precedence bucket claims the row.
    #[test]
    fn dangling_diagnostic_covers_handles_masked_by_precedence() {
        let mut records = planted();
        // AC_F1 is `failed_evidence`; give it a dangling handle too.
        records.push(closes(AC_F1, "verification:v1:ghost2"));
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(row(&report, AC_F1).bucket, "failed_evidence");
        let diag = report
            .diagnostics
            .iter()
            .find(|d| d.code == "dangling_closing_evidence")
            .expect("diagnostic present");
        assert!(
            diag.record_ids.contains(&AC_F1.to_owned()),
            "a dangling handle masked by precedence must still be named: {:?}",
            diag.record_ids
        );
    }

    /// When the `OWNED_BY_TASK` edge and `parent_task_id` disagree, the field is
    /// displayed and the conflict reported — but the claimed-done test widens
    /// over BOTH, because narrowing it would be fail-open in the one metric this
    /// command exists to enforce.
    #[test]
    fn parent_conflict_is_reported_and_the_gate_widens() {
        let records = vec![
            task(TASK_DONE, "closed_completed"),
            task(TASK_OPEN, "open"),
            // Field says OPEN, edge says DONE.
            criterion(AC_U1, TASK_OPEN, 0, "unverified"),
            owned_by(AC_U1, TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        let r = row(&report, AC_U1);
        assert_eq!(
            r.parent_task_id.as_deref(),
            Some(TASK_OPEN),
            "the recorded field is displayed"
        );
        assert!(
            r.claimed_done_unproven,
            "the edge's done task must still pull it into the gap set"
        );
        assert!(
            report
                .diagnostics
                .iter()
                .any(|d| d.code == "criterion_parent_task_conflict"
                    && d.record_ids.contains(&AC_U1.to_owned()))
        );
    }

    /// A done-state comparison that is case- or whitespace-sensitive would let a
    /// hand-authored graph smuggle a criterion past the gate.
    #[test]
    fn done_status_comparison_is_trimmed_and_case_insensitive() {
        for status in [
            "closed_completed",
            "  Closed_Completed  ",
            "CLOSED_COMPLETED",
        ] {
            let records = vec![
                task(TASK_DONE, status),
                criterion(AC_U1, TASK_DONE, 0, "unverified"),
                owned_by(AC_U1, TASK_DONE),
            ];
            let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
            assert!(
                row(&report, AC_U1).claimed_done_unproven,
                "status {status:?} must count as done"
            );
        }
    }

    /// A criterion whose owning task is absent, tombstoned, or not a `Task` is
    /// still counted, reported, and never claimed-done.
    #[test]
    fn an_unresolvable_parent_task_is_reported_never_guessed() {
        let cases: Vec<Vec<GraphRecord>> = vec![
            // Absent parent.
            vec![criterion(AC_U1, "project:v1:ghost-task", 0, "unverified")],
            // Tombstoned parent.
            vec![
                task(TASK_DONE, "closed_completed"),
                criterion(AC_U1, TASK_DONE, 0, "unverified"),
                tombstone(TASK_DONE),
            ],
            // Parent id names a non-Task node.
            vec![
                verification(VER_PASS_1, NodeKind::TestRun, Some("pass"), None),
                criterion(AC_U1, VER_PASS_1, 0, "unverified"),
            ],
        ];
        for records in cases {
            let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
            assert_eq!(report.total_criteria, 1, "still counted in the census");
            let r = row(&report, AC_U1);
            assert!(!r.claimed_done_unproven, "never guessed into the gap set");
            assert!(r.parent_task_status.is_none());
            assert!(
                report
                    .diagnostics
                    .iter()
                    .any(|d| d.code == "criterion_parent_task_unresolved")
            );
        }
    }

    /// An `EXTERNAL_HANDLE` edge can outlive the `ExternalLink` it names; a dead
    /// citation must not be emitted.
    #[test]
    fn external_link_citation_is_liveness_gated() {
        let link_id = "project:v1:extlink";
        let external = |id: &str| {
            GraphRecord::node(
                id.to_owned(),
                NodeKind::ExternalLink,
                None,
                None,
                None,
                "link".to_owned(),
            )
            .with_domain("project", PROJECT_SCHEMA_VERSION)
        };
        let edge = GraphRecord::project_edge(
            EdgeLabel::ExternalHandle,
            AC_U1.to_owned(),
            link_id.to_owned(),
            None,
            "external handle".to_owned(),
        );
        let base = vec![
            task(TASK_OPEN, "open"),
            criterion(AC_U1, TASK_OPEN, 0, "unverified"),
        ];

        let mut live = base;
        live.push(external(link_id));
        live.push(edge);
        let report = run_criteria_coverage(&live, &CriteriaCoverageConfig::default());
        assert_eq!(
            row(&report, AC_U1).external_link_id.as_deref(),
            Some(link_id),
            "a live external link is cited"
        );

        let mut dead = live;
        dead.push(tombstone(link_id));
        let report = run_criteria_coverage(&dead, &CriteriaCoverageConfig::default());
        assert!(
            row(&report, AC_U1).external_link_id.is_none(),
            "a dead citation must not be emitted"
        );
    }

    /// A superseded criterion is still counted in the proof gap; the report
    /// discloses that rather than leaving the asymmetry with `closed_dropped`
    /// invisible.
    #[test]
    fn superseded_criteria_are_counted_and_disclosed() {
        let records = vec![
            task(TASK_DONE, "closed_completed"),
            criterion(AC_U1, TASK_DONE, 0, "superseded"),
            owned_by(AC_U1, TASK_DONE),
        ];
        let report = run_criteria_coverage(&records, &CriteriaCoverageConfig::default());
        assert_eq!(report.unverified.numerator, 1);
        let diag = report
            .diagnostics
            .iter()
            .find(|d| d.code == "superseded_criteria_counted")
            .expect("the asymmetry must be disclosed in the output");
        assert!(diag.record_ids.contains(&AC_U1.to_owned()));
    }

    #[test]
    fn wire_names_are_pinned_exactly() {
        // Renaming any of these silently breaks the JSON contract, so pin the
        // exact strings rather than only their uniqueness.
        let buckets: Vec<&str> = CriterionBucket::ALL.iter().map(|b| b.as_wire()).collect();
        assert_eq!(
            buckets,
            vec![
                "proven",
                "failed_evidence",
                "dangling_evidence",
                "non_verification_evidence",
                "inconclusive_evidence",
                "unverified",
            ]
        );
        let resolutions: Vec<&str> = LinkResolution::ALL.iter().map(|r| r.as_wire()).collect();
        assert_eq!(
            resolutions,
            vec![
                "passing",
                "failing",
                "inconclusive",
                "not_verification_record",
                "ambiguous_versions",
                "unresolved",
            ]
        );
    }

    #[test]
    fn bucket_wire_names_are_unique_and_stable() {
        let mut seen = std::collections::BTreeSet::new();
        for bucket in CriterionBucket::ALL {
            assert!(
                seen.insert(bucket.as_wire()),
                "duplicate {}",
                bucket.as_wire()
            );
        }
        assert_eq!(seen.len(), CriterionBucket::ALL.len());
    }
}
