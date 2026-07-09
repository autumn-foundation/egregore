//! Pre-ingest referential-integrity validation for code-graph JSONL (issue #103).
//!
//! One read-only pass over an already-parsed record set that asserts the graph
//! is *referentially closed*: every edge endpoint resolves to a present node,
//! typed edges target nodes of an allowed kind, and tombstones do not conflict
//! with live records. It validates structural reference closure only — not
//! parse correctness, semantic accuracy, schema-version compatibility, or
//! whether extraction was complete.
//!
//! Diagnostics are redaction-safe by construction: they carry record IDs,
//! stable defect categories, relation labels, repo-relative paths, spans, and
//! counts — never record summaries, source text, or payload content.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan};

/// Stable defect category: an edge endpoint that resolves to no node record
/// and no tombstone in the graph.
pub const DANGLING_EDGE_ENDPOINT: &str = "dangling_edge_endpoint";
/// Stable defect category: a typed edge whose target node is present but of a
/// disallowed kind for the relation.
pub const EDGE_TARGET_KIND_VIOLATION: &str = "edge_target_kind_violation";
/// Stable defect category: an edge endpoint that resolves only to a tombstone
/// (the record is deleted and no node record with the same ID supersedes it).
pub const EDGE_TO_TOMBSTONED_RECORD: &str = "edge_to_tombstoned_record";
/// Stable defect category: a topology node with no incident edge, invisible
/// to edge-walking queries such as `eg query file`.
pub const ORPHAN_NODE: &str = "orphan_node";
/// Stable defect category: a tombstone whose deleted record is still
/// referenced by at least one live edge.
pub const TOMBSTONE_STRANDS_LIVE_EDGE: &str = "tombstone_strands_live_edge";

/// Node kinds that must be reachable through at least one edge.
///
/// `Repository` is the containment root and `Diagnostic` markers legitimately
/// stand alone (extractor warnings carry no edges); `Commit`/`Change` records
/// are always edge-attached by the history producer, and non-code-graph kinds
/// are out of scope for code-graph reference closure.
/// `DependencyDeclaration` facts are always emitted with their manifest
/// `File —CONTAINS→ DependencyDeclaration` attribution edge — the ownership
/// chain `--repo` scoping walks — so an unattached one is a defect
/// (PR #314 review).
const ORPHANABLE_KINDS: [NodeKind; 5] = [
    NodeKind::File,
    NodeKind::Module,
    NodeKind::Symbol,
    NodeKind::Import,
    NodeKind::DependencyDeclaration,
];

/// Allowed target node kinds for the typed code-graph relations checked by
/// issue #103, matching what the extractor and history replay actually emit.
const fn allowed_target_kinds(label: EdgeLabel) -> Option<&'static [NodeKind]> {
    match label {
        EdgeLabel::Defines => Some(&[NodeKind::Symbol]),
        EdgeLabel::Contains => Some(&[
            NodeKind::Change,
            NodeKind::Commit,
            // Repository —CONTAINS→ Diagnostic attributes skipped-manifest
            // coverage holes to their repository (issue #180).
            NodeKind::Diagnostic,
            // `File CONTAINS DebtMarker` attributes debt-comment markers
            // (issue #218) to their owning file.
            NodeKind::DebtMarker,
            // Manifest File —CONTAINS→ DependencyDeclaration attaches Cargo
            // dependency facts to their repository topology (issue #180).
            NodeKind::DependencyDeclaration,
            NodeKind::File,
            NodeKind::Module,
            // `File CONTAINS PanicRiskSite`: unwrap/expect panic-risk call
            // sites are contained by their owning file (issue #223).
            NodeKind::PanicRiskSite,
            // `File` CONTAINS `UnsafeSite` attributes unsafe-surface sites to
            // their owning file (issue #222).
            NodeKind::UnsafeSite,
        ]),
        EdgeLabel::Calls | EdgeLabel::Mentions => Some(&[NodeKind::Diagnostic, NodeKind::Symbol]),
        EdgeLabel::Imports => Some(&[NodeKind::Import]),
        _ => None,
    }
}

/// One machine-readable referential-integrity diagnostic.
///
/// Field population depends on `code`; unset fields are omitted from JSON.
/// The derived `Ord` doubles as the canonical output order: diagnostics sort
/// by category code first, then by the offending record IDs.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct ValidationDiagnostic {
    /// Stable defect category.
    pub code: &'static str,
    /// Offending edge record ID (edge-side categories).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_id: Option<String>,
    /// Relation label of the offending edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    /// Which endpoint offends: `source` or `target`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<&'static str>,
    /// Referenced ID that resolves to nothing (`dangling_edge_endpoint`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_id: Option<String>,
    /// Present-but-wrong-kind target (`edge_target_kind_violation`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    /// Observed kind of the violating target node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<&'static str>,
    /// Allowed target kinds for the relation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_kinds: Option<Vec<&'static str>>,
    /// Referenced ID that is tombstoned and unsuperseded
    /// (`edge_to_tombstoned_record`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstoned_id: Option<String>,
    /// Tombstone record ID involved in the defect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstone_id: Option<String>,
    /// Deleted record ID named by a stranding tombstone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_id: Option<String>,
    /// Live edge IDs still referencing a tombstoned record, sorted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stranded_edge_ids: Option<Vec<String>>,
    /// Offending node record ID (`orphan_node`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_id: Option<String>,
    /// Node kind of the offending record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
    /// Repo-relative path of the offending or referenced node, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Source span of the offending or referenced node, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

impl ValidationDiagnostic {
    const fn new(code: &'static str) -> Self {
        Self {
            code,
            edge_id: None,
            relation: None,
            endpoint: None,
            missing_id: None,
            target_id: None,
            target_kind: None,
            allowed_kinds: None,
            tombstoned_id: None,
            tombstone_id: None,
            deleted_id: None,
            stranded_edge_ids: None,
            record_id: None,
            kind: None,
            repo_relative_path: None,
            span: None,
        }
    }

    /// One human-readable line for `--format text`. Redaction-safe: renders
    /// only the same IDs, labels, paths, spans, and counts as the JSON form.
    #[must_use]
    pub fn to_text(&self) -> String {
        fn push(parts: &mut Vec<String>, key: &str, value: Option<&str>) {
            if let Some(value) = value {
                parts.push(format!("{key}={value}"));
            }
        }
        let mut parts = vec![format!("defect {}", self.code)];
        push(&mut parts, "edge", self.edge_id.as_deref());
        push(&mut parts, "relation", self.relation.as_deref());
        push(&mut parts, "endpoint", self.endpoint);
        push(&mut parts, "missing", self.missing_id.as_deref());
        push(&mut parts, "target", self.target_id.as_deref());
        push(&mut parts, "target_kind", self.target_kind);
        if let Some(allowed) = &self.allowed_kinds {
            parts.push(format!("allowed_kinds={}", allowed.join(",")));
        }
        push(&mut parts, "tombstoned", self.tombstoned_id.as_deref());
        push(&mut parts, "tombstone", self.tombstone_id.as_deref());
        push(&mut parts, "deleted", self.deleted_id.as_deref());
        if let Some(stranded) = &self.stranded_edge_ids {
            parts.push(format!("stranded_edges={}", stranded.join(",")));
        }
        push(&mut parts, "record", self.record_id.as_deref());
        push(&mut parts, "kind", self.kind);
        push(&mut parts, "path", self.repo_relative_path.as_deref());
        if let Some(span) = self.span {
            parts.push(format!("lines={}-{}", span.start_line, span.end_line));
        }
        parts.join(" ")
    }
}

/// Result of one referential-integrity pass over a record set.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ValidationReport {
    /// Diagnostics in canonical order (category code, then record IDs).
    pub diagnostics: Vec<ValidationDiagnostic>,
    /// Total records inspected.
    pub records: usize,
    /// Node records inspected.
    pub nodes: usize,
    /// Edge records inspected.
    pub edges: usize,
    /// Tombstone records inspected.
    pub tombstones: usize,
}

impl ValidationReport {
    /// Returns `true` when the graph is referentially closed.
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.diagnostics.is_empty()
    }
}

/// Deterministic per-ID index over one record set.
///
/// Node IDs may recur (history replay re-emits a node at every commit); the
/// first record in input order supplies the diagnostic citation.
#[derive(Default)]
struct GraphIndex<'a> {
    /// Every kind observed per node ID.
    node_kinds: BTreeMap<&'a str, BTreeSet<NodeKind>>,
    /// First node record per ID, for path/span citations.
    node_first: BTreeMap<&'a str, &'a GraphRecord>,
    /// Tombstone record IDs per deleted ID.
    tombstones_by_deleted: BTreeMap<&'a str, BTreeSet<&'a str>>,
    /// Node record count.
    nodes: usize,
    /// Edge record count.
    edges: usize,
    /// Tombstone record count.
    tombstones: usize,
}

impl<'a> GraphIndex<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut index = Self::default();
        for record in records {
            match record {
                GraphRecord::Node { id, kind, .. } => {
                    index.nodes += 1;
                    index.node_kinds.entry(id).or_default().insert(*kind);
                    index.node_first.entry(id).or_insert(record);
                }
                GraphRecord::Edge { .. } => index.edges += 1,
                GraphRecord::Tombstone { id, deleted_id, .. } => {
                    index.tombstones += 1;
                    index
                        .tombstones_by_deleted
                        .entry(deleted_id)
                        .or_default()
                        .insert(id);
                }
            }
        }
        index
    }

    /// Attaches the cited node's repo-relative path and span, when present.
    fn cite_node(&self, diagnostic: &mut ValidationDiagnostic, id: &str) {
        if let Some(GraphRecord::Node {
            repo_relative_path,
            span,
            ..
        }) = self.node_first.get(id)
        {
            diagnostic.repo_relative_path.clone_from(repo_relative_path);
            diagnostic.span = *span;
        }
    }
}

/// Checks every edge for endpoint resolution, tombstoned references, and typed
/// target kinds. Returns the set of IDs incident to any edge and, per
/// tombstoned ID, the live edges still referencing it.
fn check_edges<'a>(
    records: &'a [GraphRecord],
    index: &GraphIndex<'a>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) -> (BTreeSet<&'a str>, BTreeMap<&'a str, BTreeSet<&'a str>>) {
    let mut incident: BTreeSet<&str> = BTreeSet::new();
    let mut stranded_by_deleted: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for record in records {
        let GraphRecord::Edge {
            id: edge_id,
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        incident.insert(source);
        incident.insert(target);

        for (endpoint, endpoint_id) in [("source", source), ("target", target)] {
            if let Some(tombstone_ids) = index.tombstones_by_deleted.get(endpoint_id.as_str()) {
                stranded_by_deleted
                    .entry(endpoint_id)
                    .or_default()
                    .insert(edge_id);
                // A surviving node record with the same ID supersedes the
                // tombstone for the edge-side check: the reference still
                // resolves, and the conflict is reported on the tombstone.
                if !index.node_kinds.contains_key(endpoint_id.as_str()) {
                    let mut diagnostic = ValidationDiagnostic::new(EDGE_TO_TOMBSTONED_RECORD);
                    diagnostic.edge_id = Some(edge_id.clone());
                    diagnostic.relation = Some(label.as_str().to_owned());
                    diagnostic.endpoint = Some(endpoint);
                    diagnostic.tombstoned_id = Some(endpoint_id.clone());
                    diagnostic.tombstone_id =
                        tombstone_ids.iter().next().map(|id| (*id).to_owned());
                    diagnostics.insert(diagnostic);
                }
            } else if !index.node_kinds.contains_key(endpoint_id.as_str()) {
                let mut diagnostic = ValidationDiagnostic::new(DANGLING_EDGE_ENDPOINT);
                diagnostic.edge_id = Some(edge_id.clone());
                diagnostic.relation = Some(label.as_str().to_owned());
                diagnostic.endpoint = Some(endpoint);
                diagnostic.missing_id = Some(endpoint_id.clone());
                diagnostics.insert(diagnostic);
            }
        }

        // Typed relation target-kind check (present targets only; missing or
        // tombstoned targets are already reported above).
        if let (Some(allowed), Some(kinds)) = (
            allowed_target_kinds(*label),
            index.node_kinds.get(target.as_str()),
        ) && !kinds.iter().any(|kind| allowed.contains(kind))
        {
            let mut diagnostic = ValidationDiagnostic::new(EDGE_TARGET_KIND_VIOLATION);
            diagnostic.edge_id = Some(edge_id.clone());
            diagnostic.relation = Some(label.as_str().to_owned());
            diagnostic.target_id = Some(target.clone());
            diagnostic.target_kind = kinds.iter().next().map(|kind| kind.as_str());
            diagnostic.allowed_kinds = Some(allowed.iter().map(|kind| kind.as_str()).collect());
            index.cite_node(&mut diagnostic, target);
            diagnostics.insert(diagnostic);
        }
    }
    (incident, stranded_by_deleted)
}

/// Reports every tombstone whose deleted record is still referenced by a live
/// edge as source or target.
fn check_tombstones(
    index: &GraphIndex<'_>,
    stranded_by_deleted: &BTreeMap<&str, BTreeSet<&str>>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) {
    for (deleted_id, tombstone_ids) in &index.tombstones_by_deleted {
        let Some(stranded) = stranded_by_deleted.get(deleted_id) else {
            continue;
        };
        for tombstone_id in tombstone_ids {
            let mut diagnostic = ValidationDiagnostic::new(TOMBSTONE_STRANDS_LIVE_EDGE);
            diagnostic.tombstone_id = Some((*tombstone_id).to_owned());
            diagnostic.deleted_id = Some((*deleted_id).to_owned());
            diagnostic.stranded_edge_ids =
                Some(stranded.iter().map(|id| (*id).to_owned()).collect());
            index.cite_node(&mut diagnostic, deleted_id);
            diagnostics.insert(diagnostic);
        }
    }
}

/// Reports every topology node with zero incident edges.
fn check_orphans(
    index: &GraphIndex<'_>,
    incident: &BTreeSet<&str>,
    diagnostics: &mut BTreeSet<ValidationDiagnostic>,
) {
    for (id, kinds) in &index.node_kinds {
        if incident.contains(id) {
            continue;
        }
        let Some(orphan_kind) = kinds.iter().find(|kind| ORPHANABLE_KINDS.contains(kind)) else {
            continue;
        };
        let mut diagnostic = ValidationDiagnostic::new(ORPHAN_NODE);
        diagnostic.record_id = Some((*id).to_owned());
        diagnostic.kind = Some(orphan_kind.as_str());
        index.cite_node(&mut diagnostic, id);
        diagnostics.insert(diagnostic);
    }
}

/// Validates referential integrity over an already-parsed record set.
///
/// Checks, in one deterministic pass:
///
/// 1. every edge endpoint (source and target) resolves to a node present in
///    the graph (`dangling_edge_endpoint`);
/// 2. every `DEFINES`, `CONTAINS`, `CALLS`, `IMPORTS`, and `MENTIONS` edge
///    target is a node of an allowed kind (`edge_target_kind_violation`);
/// 3. no edge references a tombstoned-and-unsuperseded record — a tombstoned
///    ID with no surviving node record (`edge_to_tombstoned_record`);
/// 4. no record is named by a tombstone yet still referenced by a live edge
///    (`tombstone_strands_live_edge`);
/// 5. no topology node (`File`, `Module`, `Symbol`, `Import`,
///    `DependencyDeclaration`) is orphaned with zero incident edges
///    (`orphan_node`).
///
/// The output is deterministic: diagnostics are deduplicated and sorted in
/// canonical order, so repeated validation of the same input is identical.
#[must_use]
pub fn validate_records(records: &[GraphRecord]) -> ValidationReport {
    let index = GraphIndex::build(records);
    let mut diagnostics: BTreeSet<ValidationDiagnostic> = BTreeSet::new();

    let (incident, stranded_by_deleted) = check_edges(records, &index, &mut diagnostics);
    check_tombstones(&index, &stranded_by_deleted, &mut diagnostics);
    check_orphans(&index, &incident, &mut diagnostics);

    ValidationReport {
        diagnostics: diagnostics.into_iter().collect(),
        records: records.len(),
        nodes: index.nodes,
        edges: index.edges,
        tombstones: index.tombstones,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::SCHEMA_VERSION;

    fn node(id: &str, kind: NodeKind) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            Some("src/lib.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 1,
                start_line: 1,
                end_line: 1,
            }),
            Some("n".to_owned()),
            "test node".to_owned(),
        )
    }

    fn edge(id: &str, label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::Edge {
            id: id.to_owned(),
            schema_version: SCHEMA_VERSION,
            label,
            source: source.to_owned(),
            target: target.to_owned(),
            confidence: None,
            resolution: None,
            temporal: None,
            summary: "test edge".to_owned(),
            producer: None,
        }
    }

    fn tombstone(id: &str, deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: id.to_owned(),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "test tombstone".to_owned(),
            producer: None,
        }
    }

    #[test]
    fn clean_graph_has_no_diagnostics() {
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:sym", NodeKind::Symbol),
            edge("e:def", EdgeLabel::Defines, "n:file", "n:sym"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
        assert_eq!((report.nodes, report.edges, report.tombstones), (2, 1, 0));
    }

    #[test]
    fn contains_edge_to_dependency_declaration_is_allowed() {
        // Issue #180 topology: File(Cargo.toml) —CONTAINS→ DependencyDeclaration.
        let records = vec![
            node("n:manifest", NodeKind::File),
            node("n:dep", NodeKind::DependencyDeclaration),
            edge("e:contains", EdgeLabel::Contains, "n:manifest", "n:dep"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn unattached_dependency_declaration_is_an_orphan() {
        // PR #314 review: a standalone `DependencyDeclaration` without its
        // `File —CONTAINS→ DependencyDeclaration` attribution edge breaks
        // the repository-ownership chain `--repo` scoping relies on.
        let records = vec![node("n:dep", NodeKind::DependencyDeclaration)];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![ORPHAN_NODE]);
        assert_eq!(report.diagnostics[0].kind, Some("DependencyDeclaration"));
    }

    #[test]
    fn dangling_endpoints_report_per_endpoint() {
        let records = vec![edge("e:x", EdgeLabel::Calls, "n:missing-a", "n:missing-b")];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![DANGLING_EDGE_ENDPOINT, DANGLING_EDGE_ENDPOINT]);
        let endpoints: Vec<_> = report
            .diagnostics
            .iter()
            .filter_map(|d| d.endpoint)
            .collect();
        assert_eq!(endpoints, vec!["source", "target"]);
    }

    #[test]
    fn contains_edge_to_unsafe_site_is_allowed() {
        // `File` CONTAINS `UnsafeSite` is what the issue #222 extractor emits;
        // the referential-integrity gate must accept it.
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:unsafe", NodeKind::UnsafeSite),
            edge("e:contains", EdgeLabel::Contains, "n:file", "n:unsafe"),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn repository_and_diagnostic_nodes_are_never_orphans() {
        let records = vec![
            node("n:repo", NodeKind::Repository),
            node("n:diag", NodeKind::Diagnostic),
        ];
        let report = validate_records(&records);
        assert!(report.is_clean(), "got {:?}", report.diagnostics);
    }

    #[test]
    fn tombstoned_id_with_surviving_node_is_superseded_for_edge_check() {
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:sym", NodeKind::Symbol),
            edge("e:def", EdgeLabel::Defines, "n:file", "n:sym"),
            tombstone("t:1", "n:sym"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec![TOMBSTONE_STRANDS_LIVE_EDGE]);
        let diagnostic = &report.diagnostics[0];
        assert_eq!(diagnostic.deleted_id.as_deref(), Some("n:sym"));
        assert_eq!(
            diagnostic.stranded_edge_ids.as_deref(),
            Some(&["e:def".to_owned()][..])
        );
    }

    #[test]
    fn diagnostics_sort_canonically_by_code_then_ids() {
        let records = vec![
            node("n:file", NodeKind::File),
            node("n:import", NodeKind::Import),
            node("n:orphan", NodeKind::Symbol),
            edge("e:kind", EdgeLabel::Defines, "n:file", "n:import"),
            edge("e:gone", EdgeLabel::Calls, "n:file", "n:absent"),
        ];
        let report = validate_records(&records);
        let codes: Vec<_> = report.diagnostics.iter().map(|d| d.code).collect();
        let mut sorted = codes.clone();
        sorted.sort_unstable();
        assert_eq!(codes, sorted);
        assert!(codes.contains(&DANGLING_EDGE_ENDPOINT));
        assert!(codes.contains(&EDGE_TARGET_KIND_VIOLATION));
        assert!(codes.contains(&ORPHAN_NODE));
    }
}
