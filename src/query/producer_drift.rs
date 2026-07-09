use std::collections::BTreeMap;

use super::RepositoryIndex;
use crate::ir::{GraphRecord, Producer, ProducerKind, SourceSpan};

// ---------------------------------------------------------------------------
// producer-drift query (issue #234)
// ---------------------------------------------------------------------------

/// Producer identity of the currently running binary: the comparison basis
/// for [`producer_drift`].
///
/// Carries the binary's semver plus every producer-component version compiled
/// into it (all Tree-sitter grammar versions and the incremental cache format
/// version). Deterministic per build: all values are compile-time constants,
/// so repeated runs of one binary always report the same identity.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct CurrentProducerIdentity {
    /// Semver string from `CARGO_PKG_VERSION`.
    pub egregore_version: String,
    /// Every producer-component version this binary would stamp, keyed by the
    /// stable component names from `docs/schema/producer-version.md` §4.
    pub producer_components: BTreeMap<String, String>,
}

impl CurrentProducerIdentity {
    /// Builds the identity of the running binary from compile-time constants.
    #[must_use]
    pub fn of_running_binary() -> Self {
        let mut producer_components = BTreeMap::from([
            (
                "tree_sitter".to_owned(),
                env!("TREE_SITTER_VERSION").to_owned(),
            ),
            (
                "cache_format_version".to_owned(),
                crate::incremental::CACHE_SCHEMA_VERSION.to_string(),
            ),
        ]);
        for language in [
            crate::languages::Language::Rust,
            crate::languages::Language::Python,
            crate::languages::Language::TypeScript,
            crate::languages::Language::Go,
        ] {
            let (key, version) = language.tree_sitter_component();
            producer_components.insert(key.to_owned(), version.to_owned());
        }
        Self {
            egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
            producer_components,
        }
    }
}

/// Which bucket a record's producer identity landed in.
///
/// The derived `Ord` doubles as the deterministic report order: drifted
/// groups first, then current, then the never-flagged non-code bucket, then
/// `legacy_pre_v1`.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProducerDriftBucket {
    /// Code-graph-extraction producer whose grammar/binary identity differs
    /// from the running binary: re-extraction could emit different records.
    Drifted,
    /// Code-graph-extraction producer matching the running binary exactly.
    Current,
    /// Agent-memory, importer, and other non-code-extraction producers.
    /// Never compared against grammar identity, never labeled stale.
    NonCodeProducer,
    /// Records persisted before the producer envelope existed
    /// (`docs/schema/producer-version.md` §6). Never merged into the current
    /// or drifted buckets; backfill is forever out of scope.
    LegacyPreV1,
}

impl ProducerDriftBucket {
    /// Returns the serialized bucket label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drifted => "drifted",
            Self::Current => "current",
            Self::NonCodeProducer => "non_code_producer",
            Self::LegacyPreV1 => "legacy_pre_v1",
        }
    }
}

/// One field of a recorded producer signature that differs from the running
/// binary.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ProducerDriftMismatch {
    /// `egregore_version` or a `producer_components` key (e.g.
    /// `tree_sitter_rust`).
    pub field: String,
    /// The version the stored records carry.
    pub recorded: String,
    /// The running binary's version for this field; `None` when the recorded
    /// component key is unknown to this binary.
    pub current: Option<String>,
}

/// One affected record in a drifted group: a citable handle, never payload.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ProducerDriftRecord<'a> {
    /// Stable record ID.
    pub record_id: &'a str,
    /// `node`, `edge`, or `tombstone`.
    pub record_type: &'static str,
    /// Repo-relative file handle, when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<&'a str>,
    /// Source span handle, when the record carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// One distinct producer signature `(producer_kind, egregore_version,
/// component set)` and the records it stamped.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ProducerDriftGroup<'a> {
    /// Classification of this signature against the running binary.
    pub bucket: ProducerDriftBucket,
    /// Serialized producer kind; `legacy_pre_v1` for pre-envelope records.
    pub producer_kind: &'a str,
    /// Recorded binary version; absent for `legacy_pre_v1` records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egregore_version: Option<&'a str>,
    /// Recorded component versions; absent for `legacy_pre_v1` records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_components: Option<&'a BTreeMap<String, String>>,
    /// Fields that differ from the running binary. Present only on `drifted`
    /// groups — non-code and legacy buckets are never compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mismatches: Option<Vec<ProducerDriftMismatch>>,
    /// Records carrying this signature (in scope).
    pub record_count: usize,
    /// Affected record handles, listed only for `drifted` groups and sorted
    /// by record ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records: Option<Vec<ProducerDriftRecord<'a>>>,
}

/// Per-bucket record tallies for the drift report.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, serde::Serialize)]
pub struct ProducerDriftCounts {
    /// Records considered (after `--repo` scoping).
    pub total: usize,
    /// Code-graph records whose producer differs from the running binary.
    pub drifted: usize,
    /// Code-graph records matching the running binary exactly.
    pub current: usize,
    /// Records from non-code-extraction producers (never flagged).
    pub non_code_producer: usize,
    /// Records with no producer envelope (never flagged, never merged).
    pub legacy_pre_v1: usize,
}

/// A stable machine-readable condition attached to the drift report.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ProducerDriftDiagnostic {
    /// Stable diagnostic code (`no_drift`, `empty_store`).
    pub code: &'static str,
    /// Bounded human-readable detail (labels and counts only — never payload).
    pub detail: String,
}

/// The full producer-drift report: comparison basis, signature groups,
/// tallies, and diagnostics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProducerDriftReport<'a> {
    /// The running binary's producer identity the store was compared against.
    pub current_producer: &'a CurrentProducerIdentity,
    /// Signature groups in deterministic bucket/signature order.
    pub groups: Vec<ProducerDriftGroup<'a>>,
    /// Per-bucket tallies.
    pub counts: ProducerDriftCounts,
    /// Stable diagnostics, sorted by code.
    pub diagnostics: Vec<ProducerDriftDiagnostic>,
}

/// Returns `true` for the producer kinds whose records are deterministic
/// code-graph extraction output and therefore comparable against the running
/// binary's grammar/extractor identity (issue #234 AC3).
const fn is_code_graph_producer_kind(kind: ProducerKind) -> bool {
    matches!(
        kind,
        ProducerKind::CodeGraphExtractor
            | ProducerKind::HistoryReplay
            | ProducerKind::IncrementalCache
    )
}

/// Compares one recorded producer envelope against the running binary.
///
/// A recorded component matches when the running binary knows the key and the
/// versions are equal; a key unknown to this binary is a mismatch with
/// `current: None`. Only `egregore_version` and `producer_components` are
/// compared: `producer_started_at` is wall-clock (comparing it would break
/// the zero-false-positive bar) and `egregore_git` is build provenance
/// subsumed by the version string per the issue #234 signature definition.
fn producer_mismatches(
    producer: &Producer,
    current: &CurrentProducerIdentity,
) -> Vec<ProducerDriftMismatch> {
    let mut mismatches = Vec::new();
    if producer.egregore_version != current.egregore_version {
        mismatches.push(ProducerDriftMismatch {
            field: "egregore_version".to_owned(),
            recorded: producer.egregore_version.clone(),
            current: Some(current.egregore_version.clone()),
        });
    }
    for (key, recorded) in &producer.producer_components {
        match current.producer_components.get(key) {
            Some(version) if version == recorded => {}
            other => mismatches.push(ProducerDriftMismatch {
                field: key.clone(),
                recorded: recorded.clone(),
                current: other.cloned(),
            }),
        }
    }
    mismatches
}

/// Builds the citable handle row for one record in a drifted group.
fn producer_drift_record_row(record: &GraphRecord) -> ProducerDriftRecord<'_> {
    match record {
        GraphRecord::Node {
            id,
            repo_relative_path,
            span,
            ..
        } => ProducerDriftRecord {
            record_id: id,
            record_type: "node",
            repo_relative_path: repo_relative_path.as_deref(),
            span: *span,
        },
        GraphRecord::Edge { id, .. } => ProducerDriftRecord {
            record_id: id,
            record_type: "edge",
            repo_relative_path: None,
            span: None,
        },
        GraphRecord::Tombstone { id, .. } => ProducerDriftRecord {
            record_id: id,
            record_type: "tombstone",
            repo_relative_path: None,
            span: None,
        },
    }
}

/// Audits stored producer identity against the running binary (issue #234).
///
/// Read-only: reports which records a re-extraction with this binary could
/// change, and never re-extracts, re-embeds, or mutates the store. Records
/// group by the distinct signature `(producer_kind, egregore_version,
/// producer_components)`:
///
/// - Code-graph-extraction producers (`code_graph_extractor`,
///   `history_replay`, `incremental_cache`) are compared field-by-field
///   against the running binary; any difference lands the group in the
///   `drifted` bucket with per-field mismatches and per-record handles.
/// - All other producers (agent memory, importers, drift engine, …) land in
///   the never-flagged `non_code_producer` bucket.
/// - Records without a producer envelope land in `legacy_pre_v1`, never
///   merged into any other bucket (`docs/schema/producer-version.md` §6).
///
/// With `repo_scope`, only records attributable to the scoped repository are
/// considered (nodes by ID, edges by source node, tombstones by deleted ID —
/// a deleted *edge* ID resolves through the deleted edge's recorded source
/// node, and a deleted *node* ID through its recorded containment parents,
/// since the repository index owns only IDs reachable in the live topology);
/// unattributable records are excluded from a scoped run.
/// `store_record_parents` supplies record-ID → attribution-parent links for
/// records the slice no longer contains: an embedded store's current-state
/// view suppresses actively tombstoned edges *and* nodes, so the
/// slice-derived map alone would drop every deletion tombstone from a scoped
/// `--data-dir` run. Each tombstoned edge maps to its source node, and each
/// tombstoned containment edge's target maps to that source, so ownership
/// resolves by chasing `deleted record → parent → … → live topology`. Pass
/// an empty map for JSONL graphs (superseded records stay in the stream).
/// Deterministic:
/// output ordering depends only on record content and compile-time constants.
#[must_use]
pub fn producer_drift<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    store_record_parents: &BTreeMap<String, String>,
    current: &'a CurrentProducerIdentity,
) -> ProducerDriftReport<'a> {
    struct GroupAccum<'a> {
        mismatches: Option<Vec<ProducerDriftMismatch>>,
        count: usize,
        records: Vec<ProducerDriftRecord<'a>>,
    }
    type GroupKey<'a> = (
        ProducerDriftBucket,
        &'a str,
        Option<&'a str>,
        Option<&'a BTreeMap<String, String>>,
    );

    let mut groups: BTreeMap<GroupKey<'a>, GroupAccum<'a>> = BTreeMap::new();
    let mut counts = ProducerDriftCounts::default();

    // A tombstone's `deleted_id` may name an *edge* record (the incremental
    // cache tombstones stale DEFINES/CALLS/... edges) or a *node* whose
    // containment topology was deleted along with it (a whole-file
    // invalidation tombstones the File node, its Symbols, and their edges).
    // The repository index owns only IDs reachable in the live topology, so
    // resolving such a deleted ID directly always fails and would silently
    // drop every attributable deletion tombstone from a scoped run. Resolve
    // instead by chasing recorded attribution parents — an edge's source
    // node, a deleted node's containment parent — until a live owner is
    // found. In a JSONL graph the superseded records stay in the slice of an
    // ingested incremental stream; an embedded store's current-state view
    // suppresses them, so the caller-supplied `store_record_parents` fills
    // those gaps. Records that still resolve to no repository stay excluded,
    // as documented.
    let deleted_record_parents: BTreeMap<&str, &str> = if repo_scope.is_some() {
        store_record_parents
            .iter()
            .map(|(id, parent)| (id.as_str(), parent.as_str()))
            .chain(records.iter().filter_map(|record| match record {
                GraphRecord::Edge { id, source, .. } => Some((id.as_str(), source.as_str())),
                GraphRecord::Node { .. } | GraphRecord::Tombstone { .. } => None,
            }))
            .collect()
    } else {
        BTreeMap::new()
    };
    let owner_of_deleted = |deleted_id: &str| -> Option<&str> {
        // Chase bound: parent chains are containment paths (symbol → file →
        // module → repository), so any genuine chain is short; the bound
        // only guards against a malformed store's cyclic parent links.
        const MAX_PARENT_HOPS: usize = 64;
        let mut anchor = deleted_id;
        for _ in 0..=MAX_PARENT_HOPS {
            if let Some(owner) = index.owner_of(anchor) {
                return Some(owner);
            }
            anchor = deleted_record_parents.get(anchor)?;
        }
        None
    };

    for record in records {
        if let Some(scope) = repo_scope {
            let owner = match record {
                GraphRecord::Node { id, .. } => index.owner_of(id),
                GraphRecord::Edge { source, .. } => index.owner_of(source),
                GraphRecord::Tombstone { deleted_id, .. } => owner_of_deleted(deleted_id),
            };
            if owner != Some(scope) {
                continue;
            }
        }
        counts.total += 1;

        let (bucket, mismatches) = match record.producer() {
            None => (ProducerDriftBucket::LegacyPreV1, None),
            Some(producer) if is_code_graph_producer_kind(producer.producer_kind) => {
                let mismatches = producer_mismatches(producer, current);
                if mismatches.is_empty() {
                    (ProducerDriftBucket::Current, None)
                } else {
                    (ProducerDriftBucket::Drifted, Some(mismatches))
                }
            }
            Some(_) => (ProducerDriftBucket::NonCodeProducer, None),
        };
        match bucket {
            ProducerDriftBucket::Drifted => counts.drifted += 1,
            ProducerDriftBucket::Current => counts.current += 1,
            ProducerDriftBucket::NonCodeProducer => counts.non_code_producer += 1,
            ProducerDriftBucket::LegacyPreV1 => counts.legacy_pre_v1 += 1,
        }

        let (kind, version, components) =
            record
                .producer()
                .map_or(("legacy_pre_v1", None, None), |p| {
                    (
                        p.producer_kind.as_str(),
                        Some(p.egregore_version.as_str()),
                        Some(&p.producer_components),
                    )
                });
        let entry = groups
            .entry((bucket, kind, version, components))
            .or_insert_with(|| GroupAccum {
                mismatches,
                count: 0,
                records: Vec::new(),
            });
        entry.count += 1;
        if bucket == ProducerDriftBucket::Drifted {
            entry.records.push(producer_drift_record_row(record));
        }
    }

    let groups = groups
        .into_iter()
        .map(|((bucket, kind, version, components), mut accum)| {
            accum.records.sort_by(|a, b| {
                a.record_id
                    .cmp(b.record_id)
                    .then_with(|| a.repo_relative_path.cmp(&b.repo_relative_path))
                    .then_with(|| a.span.cmp(&b.span))
            });
            ProducerDriftGroup {
                bucket,
                producer_kind: kind,
                egregore_version: version,
                producer_components: components,
                mismatches: accum.mismatches,
                record_count: accum.count,
                records: (bucket == ProducerDriftBucket::Drifted).then_some(accum.records),
            }
        })
        .collect();

    let mut diagnostics = Vec::new();
    if counts.total == 0 {
        diagnostics.push(ProducerDriftDiagnostic {
            code: "empty_store",
            detail: "the supplied store contains no records in scope".to_owned(),
        });
    }
    if counts.drifted == 0 {
        diagnostics.push(ProducerDriftDiagnostic {
            code: "no_drift",
            detail: "every code-graph record matches the current producer identity; \
                     an empty drift result is an explicit answer, not an error"
                .to_owned(),
        });
    }
    diagnostics.sort_by(|a, b| a.code.cmp(b.code));

    ProducerDriftReport {
        current_producer: current,
        groups,
        counts,
        diagnostics,
    }
}
