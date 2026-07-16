use std::collections::{BTreeMap, BTreeSet};

use super::{RepositoryIndex, path_is_under_prefix, public_api_surface};
use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

// ---------------------------------------------------------------------------
// verification-coverage lane (issue #109)
// ---------------------------------------------------------------------------
//
// Answers "which externally-reachable public symbols carry recorded
// verification evidence, and which do not?" by joining the issue #213 public
// surface with the recorded verification-domain nodes over the evidence-link
// edge/citation registry — never a `#[test]`/coverage-tool grep, never a
// build or coverage run.
//
// A public symbol S is COVERED when some verification-domain node V
// (Verification / CommandRun / TestRun / ProofResult / CIStatus /
// CommandEvidence / BenchmarkRun / CoverageReport, or a node whose domain
// override is `verification`) is connected to S by an evidence relation —
// either directly to the symbol (any evidence-link label; `link_level`
// `symbol`) or to S's containing file via `TOUCHED_FILE` / `FAILED_ON`
// (`link_level` `file`). An agent-memory node linking to S never confers
// coverage: one endpoint of the relation must be a verification-domain node.
//
// Capability-degradation contract (mirrors `undocumented`'s
// `doc_facts_unavailable` verdict): on trunk no writer links a
// verification-domain node to a code Symbol/File, so by default this lane is a
// capability-absent lane. When the store carries no verification records at
// all, or carries them but none link to code, the report is
// `capability_present = false` with a single `verification_facts_unavailable`
// diagnostic and EMPTY buckets — the lane never floods every symbol into
// "uncovered" because the linking writer never ran. Absence of recorded
// evidence is a prioritization signal, NEVER proof that code is untested,
// unverified in reality, unsafe, or broken; presence is a recorded link,
// never proof of correctness or that a test/proof passed.

/// Default per-bucket row cap when `--limit` is not supplied.
pub const VERIFICATION_COVERAGE_DEFAULT_LIMIT: usize = 500;

/// Maximum accepted `--limit` value.
pub const VERIFICATION_COVERAGE_MAX_LIMIT: usize = 1000;

/// How a supplied `scope` handle resolved against the in-store code items.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ScopeOutcome {
    /// No `scope` handle was supplied.
    NoScope,
    /// The handle matched at least one in-store code item.
    Matched,
    /// A path-shaped handle matched no in-store code item (CLI exit 2,
    /// `scope_not_found`).
    NotFoundPath,
    /// A name/id-shaped handle matched no in-store code item (CLI exit 2,
    /// `no_match`).
    NoMatch,
}

/// One recorded verification link crediting a covered symbol.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct CoverageEvidence<'a> {
    /// Stable record ID of the verification-domain node.
    pub record_id: &'a str,
    /// Reported verification kind: the node's `verification_kind` when present,
    /// else the node-kind string.
    pub verification_kind: &'a str,
    /// Evidence-link edge label the coverage was derived from.
    pub edge_label: &'static str,
    /// `symbol` for a symbol-direct link, `file` for a file-level
    /// (`TOUCHED_FILE`/`FAILED_ON`) link.
    pub link_level: &'static str,
}

/// One covered public symbol plus the verification links crediting it.
#[derive(Debug, Clone)]
pub struct CoveredItem<'a> {
    /// Stable record ID of the covered `Symbol` (or re-export site) node.
    pub record_id: &'a str,
    /// Item kind from the public surface (`function`, `struct`, ...).
    pub kind: String,
    /// Externally visible crate-relative path.
    pub path: String,
    /// Repo-relative file of the declaration.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration.
    pub span: Option<SourceSpan>,
    /// Crediting verification links, sorted by
    /// `(record_id, edge_label, link_level)` and de-duplicated.
    pub verification: Vec<CoverageEvidence<'a>>,
}

/// One public symbol with no recorded verification evidence.
#[derive(Debug, Clone)]
pub struct UncoveredItem<'a> {
    /// Stable record ID of the uncovered `Symbol` (or re-export site) node.
    pub record_id: &'a str,
    /// Item kind from the public surface.
    pub kind: String,
    /// Externally visible crate-relative path.
    pub path: String,
    /// Repo-relative file of the declaration.
    pub repo_relative_path: Option<&'a str>,
    /// Source span of the declaration.
    pub span: Option<SourceSpan>,
    /// Schema version of the symbol record.
    pub schema_version: u32,
    /// Recorded valid time of the symbol record, when present.
    pub valid_time: Option<&'a str>,
    /// Introducing commit SHA, when the record is history-backed.
    pub git_commit: Option<&'a str>,
}

/// Deterministic tallies for the verification-coverage lane.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct VerificationCoverageCounts {
    /// Public symbols in scope (`covered + uncovered`, pre-truncation).
    pub symbols_in_scope: usize,
    /// Covered symbols (pre-truncation).
    pub covered: usize,
    /// Uncovered symbols (pre-truncation).
    pub uncovered: usize,
    /// Verification-domain nodes in the (repo-scoped) store.
    pub verification_records_in_store: usize,
    /// Distinct evidence relations between a verification node and a code
    /// Symbol/File in the (repo-scoped) store.
    pub verification_code_links_in_store: usize,
    /// `true` when the covered bucket was truncated to `--limit`.
    pub covered_truncated: bool,
    /// `true` when the uncovered bucket was truncated to `--limit`.
    pub uncovered_truncated: bool,
}

/// A stable machine-readable condition attached to the coverage result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct VerificationCoverageDiagnostic {
    /// Stable diagnostic code.
    pub code: &'static str,
    /// Bounded human-readable detail (counts and markers only — never payload).
    pub detail: String,
}

/// The verification-coverage report: buckets, tallies, and diagnostics.
#[derive(Debug, Clone)]
pub struct VerificationCoverageReport<'a> {
    /// `true` when the store records at least one verification→code link, so
    /// the covered/uncovered partition is meaningful.
    pub capability_present: bool,
    /// When capability is absent, the reason: `no_verification_records` or
    /// `no_verification_code_links`.
    pub capability_reason: Option<&'static str>,
    /// How a supplied `scope` handle resolved.
    pub scope_outcome: ScopeOutcome,
    /// Covered symbols, sorted by `(repo_relative_path, start_line, record_id)`.
    pub covered: Vec<CoveredItem<'a>>,
    /// Uncovered symbols, sorted by `(repo_relative_path, start_line,
    /// record_id)`.
    pub uncovered: Vec<UncoveredItem<'a>>,
    /// Deterministic tallies.
    pub counts: VerificationCoverageCounts,
    /// Stable diagnostics, sorted and de-duplicated.
    pub diagnostics: Vec<VerificationCoverageDiagnostic>,
}

/// A crediting-evidence tuple: `(ver_record_id, ver_kind, edge_label,
/// link_level)`.
type CoverageTuple<'a> = (&'a str, &'a str, &'static str, &'static str);

/// Truncates one result bucket to `limit`, flagging the truncation and pushing a
/// `results_truncated` diagnostic carrying the true pre-truncation count.
fn truncate_bucket<T>(
    rows: &mut Vec<T>,
    limit: usize,
    truncated: &mut bool,
    noun: &str,
    diagnostics: &mut Vec<VerificationCoverageDiagnostic>,
) {
    if rows.len() > limit {
        let total = rows.len();
        rows.truncate(limit);
        *truncated = true;
        diagnostics.push(VerificationCoverageDiagnostic {
            code: "results_truncated",
            detail: format!(
                "showing {limit} of {total} {noun} rows; raise --limit to see the rest"
            ),
        });
    }
}

/// Returns the reported verification kind when `record` is a
/// verification-domain node, else `None`.
fn verification_kind_of(record: &GraphRecord) -> Option<&str> {
    let GraphRecord::Node {
        kind,
        domain,
        verification_kind,
        ..
    } = record
    else {
        return None;
    };
    let is_verification = matches!(
        kind,
        NodeKind::Verification
            | NodeKind::CommandRun
            | NodeKind::TestRun
            | NodeKind::ProofResult
            | NodeKind::CIStatus
            | NodeKind::CommandEvidence
            | NodeKind::BenchmarkRun
            | NodeKind::CoverageReport
    ) || domain.as_deref() == Some("verification");
    if !is_verification {
        return None;
    }
    Some(verification_kind.as_deref().unwrap_or(kind.as_str()))
}

/// Computes verification coverage of the externally-reachable public surface
/// (issue #109).
///
/// `scope` (optional) filters the surface to one code item, resolved in
/// precedence order: exact record ID, else exact symbol path/name, else a
/// segment-aware repo-relative path prefix. A supplied scope matching no code
/// item is reported through [`ScopeOutcome`] for the CLI to map to exit 2.
///
/// `limit` (default [`VERIFICATION_COVERAGE_DEFAULT_LIMIT`]) caps EACH bucket
/// independently after sorting; a truncated bucket sets its `*_truncated`
/// count flag and pushes a `results_truncated` diagnostic carrying the true
/// pre-truncation count. Deterministic: output ordering depends only on record
/// content.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn verification_coverage<'a>(
    records: &'a [GraphRecord],
    index: &RepositoryIndex,
    repo_scope: Option<&str>,
    scope: Option<&str>,
    limit: Option<usize>,
) -> VerificationCoverageReport<'a> {
    let surface = public_api_surface(records, index, repo_scope);

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

    let owner_key = |id: &str| index.owner_of(id).unwrap_or("");
    // A code item counts only when it belongs to the scoped repository (an
    // unattributed code node is excluded under a repo scope, matching the
    // public surface's own filter).
    let code_in_scope = |id: &str| repo_scope.is_none_or(|s| index.owner_of(id) == Some(s));
    // A verification node counts when it belongs to the scoped repository OR
    // carries no repository attribution at all (verification records are
    // typically unattributed); a node owned by a DIFFERENT repository does not.
    let ver_in_scope =
        |id: &str| repo_scope.is_none_or(|s| index.owner_of(id).is_none_or(|o| o == s));

    // Verification-domain nodes: id -> reported kind.
    let mut ver_kind: BTreeMap<&str, &str> = BTreeMap::new();
    // Code File nodes: id -> (owner, path); and code Symbol node ids.
    let mut file_key: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    let mut symbol_ids: BTreeSet<&str> = BTreeSet::new();
    let mut nodes_by_id: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            repo_relative_path,
            ..
        } = record
        else {
            continue;
        };
        nodes_by_id.insert(id.as_str(), record);
        if tombstoned.contains(id.as_str()) {
            continue;
        }
        if let Some(kind_str) = verification_kind_of(record) {
            if ver_in_scope(id) {
                ver_kind.insert(id.as_str(), kind_str);
            }
            continue;
        }
        if !code_in_scope(id) {
            continue;
        }
        match kind {
            NodeKind::File => {
                if let Some(path) = repo_relative_path {
                    file_key.insert(id.as_str(), (owner_key(id), path.as_str()));
                }
            }
            NodeKind::Symbol => {
                symbol_ids.insert(id.as_str());
            }
            _ => {}
        }
    }

    // Every evidence relation, from both representations and considering both
    // endpoint orderings: graph edges whose label is an evidence-link label,
    // plus EvidenceLinks carried on a node (`evidence_links` and the
    // user-context `supporting_evidence`).
    let mut relations: Vec<(&str, &str, EdgeLabel)> = Vec::new();
    for record in records {
        match record {
            GraphRecord::Edge {
                id,
                label,
                source,
                target,
                ..
            } if label.is_evidence_link_label() && !tombstoned.contains(id.as_str()) => {
                relations.push((source.as_str(), target.as_str(), *label));
            }
            GraphRecord::Node {
                id,
                evidence_links,
                user_context,
                ..
            } if !tombstoned.contains(id.as_str()) => {
                let carried = evidence_links
                    .iter()
                    .flatten()
                    .chain(user_context.supporting_evidence.iter().flatten());
                for link in carried {
                    let Some(target) = link.target_record_id.as_deref() else {
                        continue;
                    };
                    let Some(label) = EdgeLabel::from_relation(&link.relation) else {
                        continue;
                    };
                    if label.is_evidence_link_label() {
                        relations.push((id.as_str(), target, label));
                    }
                }
            }
            _ => {}
        }
    }

    // Resolve each relation to a (verification node, code endpoint) pair.
    let mut symbol_evidence: BTreeMap<&str, BTreeSet<CoverageTuple<'a>>> = BTreeMap::new();
    let mut file_evidence: BTreeMap<(&str, &str), BTreeSet<CoverageTuple<'a>>> = BTreeMap::new();
    let mut code_links: BTreeSet<(&str, &str, &'static str)> = BTreeSet::new();
    for (a, b, label) in relations {
        let a_ver = ver_kind.get(a).copied();
        let b_ver = ver_kind.get(b).copied();
        let (ver_id, ver, other) = match (a_ver, b_ver) {
            (Some(k), None) => (a, k, b),
            (None, Some(k)) => (b, k, a),
            _ => continue, // neither or both verification: not a code link
        };
        let label_str = label.as_str();
        if symbol_ids.contains(other) {
            code_links.insert((ver_id, other, label_str));
            symbol_evidence
                .entry(other)
                .or_default()
                .insert((ver_id, ver, label_str, "symbol"));
        } else if let Some(&(owner, path)) = file_key.get(other) {
            code_links.insert((ver_id, other, label_str));
            if matches!(label, EdgeLabel::TouchedFile | EdgeLabel::FailedOn) {
                file_evidence
                    .entry((owner, path))
                    .or_default()
                    .insert((ver_id, ver, label_str, "file"));
            }
        }
    }

    let verification_records_in_store = ver_kind.len();
    let verification_code_links_in_store = code_links.len();
    let capability_present =
        verification_records_in_store > 0 && verification_code_links_in_store > 0;
    let capability_reason = if verification_records_in_store == 0 {
        Some("no_verification_records")
    } else if verification_code_links_in_store == 0 {
        Some("no_verification_code_links")
    } else {
        None
    };

    // Resolve the optional scope handle against the public surface items, in
    // precedence order: exact record ID, exact symbol name, then a
    // segment-aware repo-relative path prefix.
    let (in_scope_items, scope_outcome): (Vec<&super::PublicApiItem<'a>>, ScopeOutcome) =
        match scope {
            None => (surface.items.iter().collect(), ScopeOutcome::NoScope),
            Some(s) => {
                let by_id: Vec<_> = surface.items.iter().filter(|i| i.record_id == s).collect();
                let by_name: Vec<_> = surface.items.iter().filter(|i| i.path == s).collect();
                let by_path: Vec<_> = surface
                    .items
                    .iter()
                    .filter(|i| {
                        i.repo_relative_path
                            .is_some_and(|p| path_is_under_prefix(p, s))
                    })
                    .collect();
                match [by_id, by_name, by_path]
                    .into_iter()
                    .find(|candidate| !candidate.is_empty())
                {
                    Some(items) => (items, ScopeOutcome::Matched),
                    None if s.contains('/') => (Vec::new(), ScopeOutcome::NotFoundPath),
                    None => (Vec::new(), ScopeOutcome::NoMatch),
                }
            }
        };

    let mut counts = VerificationCoverageCounts {
        verification_records_in_store,
        verification_code_links_in_store,
        symbols_in_scope: in_scope_items.len(),
        ..VerificationCoverageCounts::default()
    };
    let mut diagnostics: Vec<VerificationCoverageDiagnostic> = Vec::new();

    if !capability_present {
        // In this branch capability is absent, so exactly one reason holds.
        let detail = if verification_records_in_store == 0 {
            "the store records no verification-domain nodes; \
                 verification coverage cannot be assessed. Absence of recorded evidence is a \
                 prioritization signal, never proof that code is untested, unverified in \
                 reality, unsafe, or broken"
        } else {
            "the store records verification-domain nodes but none link to a code \
                 Symbol/File (no linking writer has run); verification coverage cannot be \
                 assessed. Absence of recorded evidence is a prioritization signal, never \
                 proof that code is untested, unverified in reality, unsafe, or broken"
        };
        diagnostics.push(VerificationCoverageDiagnostic {
            code: "verification_facts_unavailable",
            detail: detail.to_owned(),
        });
        return VerificationCoverageReport {
            capability_present,
            capability_reason,
            scope_outcome,
            covered: Vec::new(),
            uncovered: Vec::new(),
            counts,
            diagnostics,
        };
    }

    // Partition in-scope public symbols into covered / uncovered.
    let mut covered: Vec<CoveredItem<'a>> = Vec::new();
    let mut uncovered: Vec<UncoveredItem<'a>> = Vec::new();
    for item in &in_scope_items {
        let owner = owner_key(item.record_id);
        let mut evidence: Vec<CoverageTuple<'a>> = Vec::new();
        if let Some(set) = symbol_evidence.get(item.record_id) {
            evidence.extend(set.iter().copied());
        }
        if let Some(path) = item.repo_relative_path {
            if let Some(set) = file_evidence.get(&(owner, path)) {
                evidence.extend(set.iter().copied());
            }
        }
        if evidence.is_empty() {
            let (schema_version, valid_time, git_commit) =
                match nodes_by_id.get(item.record_id).copied() {
                    Some(GraphRecord::Node {
                        schema_version,
                        temporal,
                        valid_time,
                        ..
                    }) => (
                        *schema_version,
                        temporal
                            .as_ref()
                            .map(|t| t.valid_time.as_str())
                            .or(valid_time.as_deref()),
                        temporal.as_ref().map(|t| t.git_commit.as_str()),
                    ),
                    _ => (0, None, None),
                };
            uncovered.push(UncoveredItem {
                record_id: item.record_id,
                kind: item.kind.clone(),
                path: item.path.clone(),
                repo_relative_path: item.repo_relative_path,
                span: item.span,
                schema_version,
                valid_time,
                git_commit,
            });
        } else {
            evidence.sort_by(|x, y| x.0.cmp(y.0).then(x.2.cmp(y.2)).then(x.3.cmp(y.3)));
            evidence.dedup();
            covered.push(CoveredItem {
                record_id: item.record_id,
                kind: item.kind.clone(),
                path: item.path.clone(),
                repo_relative_path: item.repo_relative_path,
                span: item.span,
                verification: evidence
                    .into_iter()
                    .map(|(record_id, verification_kind, edge_label, link_level)| {
                        CoverageEvidence {
                            record_id,
                            verification_kind,
                            edge_label,
                            link_level,
                        }
                    })
                    .collect(),
            });
        }
    }

    // Sort each bucket by (repo-relative path, start line, record id), comparing
    // borrowed tuples so no allocation happens per comparison.
    let sort_key = |path: Option<&'a str>, span: Option<SourceSpan>, id: &'a str| {
        (path.unwrap_or(""), span.map_or(0, |s| s.start_line), id)
    };
    covered.sort_by_key(|c| sort_key(c.repo_relative_path, c.span, c.record_id));
    uncovered.sort_by_key(|u| sort_key(u.repo_relative_path, u.span, u.record_id));

    counts.covered = covered.len();
    counts.uncovered = uncovered.len();

    let limit = limit.unwrap_or(VERIFICATION_COVERAGE_DEFAULT_LIMIT);
    truncate_bucket(
        &mut covered,
        limit,
        &mut counts.covered_truncated,
        "covered",
        &mut diagnostics,
    );
    truncate_bucket(
        &mut uncovered,
        limit,
        &mut counts.uncovered_truncated,
        "uncovered",
        &mut diagnostics,
    );

    diagnostics.sort_by(|a, b| a.code.cmp(b.code).then_with(|| a.detail.cmp(&b.detail)));
    diagnostics.dedup();

    VerificationCoverageReport {
        capability_present,
        capability_reason,
        scope_outcome,
        covered,
        uncovered,
        counts,
        diagnostics,
    }
}
