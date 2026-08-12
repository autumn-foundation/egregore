//! `AletheiaDB` 0.2.0 schema constraints as a commit-time backstop for the
//! Egregore schema contract (issue #486).
//!
//! # Why
//!
//! `eg validate` (issue #103) gates a graph JSONL file BETWEEN scan and ingest.
//! It cannot see a bad write that reaches an embedded store by another path — a
//! hand-edited store, a foreign writer, a future adapter regression. The
//! adapter's own invariants live only as adapter code, which is exactly the
//! wrong place for a backstop: code that is wrong cannot catch itself being
//! wrong. `AletheiaDB` 0.2.0 ships opt-in per-label schema constraints enforced
//! at the pre-apply commit hook, which puts the check at the write boundary
//! itself.
//!
//! # The label-partition finding
//!
//! The issue's blocking question was whether the store-side label partition is
//! coarse enough to make constraints near-trivial. It is not: the embedded
//! adapter writes ONE store-side node label per [`NodeKind`]
//! (`node_label(kind) == kind.as_str()`), plus the literal [`TOMBSTONE_LABEL`]
//! for tombstone records, and ONE edge type per [`EdgeLabel`]. `Symbol` and
//! `Task` do NOT share a label. The partition therefore lines up exactly with
//! the kinds the schema contract is written against.
//!
//! # What is declarable, and what deliberately is not
//!
//! Every node the adapter writes carries the four `base_properties` keys, and
//! every edge carries four more. Those are the only keys present on EVERY
//! record of EVERY label, so they are the only ones a per-label declaration can
//! require without asserting something about a specific domain's payload.
//!
//! Per-kind payload fields (`name`, `path`, spans, `status`, …) are
//! deliberately NOT declared. A future slice may legitimately make a payload
//! field optional or drop it, and a constraint that fires on a legitimate
//! future write is worse than no constraint (the whole transaction aborts, with
//! zero partial application).
//!
//! # Why this survives schema-version bumps
//!
//! Two bump directions exist, and both are safe for these profiles:
//!
//! * A per-domain `SCHEMA_VERSION` bump changes the `schema_version` VALUE, not
//!   its TYPE. `require_typed("schema_version", Integer)` is invariant under
//!   every bump.
//! * A NEW [`NodeKind`] mints a NEW store-side label, which carries no
//!   declaration and is therefore fully schemaless. Adding kinds can never
//!   break a declared store.
//!
//! The one change that WOULD break a declared store is renaming or dropping a
//! `base_properties` key. That is recorded as a hard prerequisite in
//! `docs/schema/schema-versioning.md`: drop the constraints first.
//!
//! This module is feature-INDEPENDENT — it holds no `AletheiaDB` types, so it
//! compiles and is unit-tested in every feature configuration. The actual
//! `.dry_run()` / `.enable()` calls live behind the embedded adapter boundary.

use std::collections::BTreeSet;

use crate::ir::{EdgeLabel, NodeKind};

/// The store-side node label the adapter writes for tombstone records.
///
/// Tombstones are nodes with their own label rather than a [`NodeKind`], so the
/// inventory must add it explicitly; the `tombstone_label_is_not_a_node_kind`
/// guard pins that it never collides with a kind name.
pub const TOMBSTONE_LABEL: &str = "Tombstone";

/// The stable token describing how the store-side label set is partitioned.
///
/// Emitted verbatim in the report so the finding that decided this issue is
/// machine-readable, not buried in prose.
pub const LABEL_PARTITION: &str = "one_label_per_node_kind";

/// Maximum offending record ids listed per violation row.
///
/// Upstream already bounds its own sample at 16; this is Egregore's own cap on
/// what it renders, so the report stays bounded even if upstream's changes.
pub const MAX_SAMPLE_RECORD_IDS: usize = 8;

/// The subset of `DeclaredType` Egregore actually declares.
///
/// Deliberately narrow: the adapter writes only strings and integers among the
/// universal keys, and a token that cannot be constructed cannot be declared by
/// mistake.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum DeclaredTypeToken {
    /// `PropertyValue::String`.
    String,
    /// `PropertyValue::Int`.
    Integer,
}

impl DeclaredTypeToken {
    /// The stable wire token, matching upstream's `DeclaredType::type_name`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "int",
        }
    }
}

/// One declared property constraint.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct PropertySpec {
    /// The property key.
    pub property: &'static str,
    /// The type the value must hold when present and non-null.
    pub declared_type: DeclaredTypeToken,
    /// Whether the key must be present with a non-null value.
    pub required: bool,
}

impl PropertySpec {
    const fn required(property: &'static str, declared_type: DeclaredTypeToken) -> Self {
        Self {
            property,
            declared_type,
            required: true,
        }
    }

    /// The JSON shape rendered in the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "property": self.property,
            "declared_type": self.declared_type.as_str(),
            "required": self.required,
        })
    }
}

/// Which store-side entity kind a spec set applies to.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum EntityKindToken {
    /// A node label.
    Node,
    /// An edge type.
    Edge,
}

impl EntityKindToken {
    /// The stable wire token, matching upstream's `EntityKind::as_str`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Edge => "edge",
        }
    }
}

/// A candidate constraint profile.
///
/// Two profiles exist so the report can quantify what a stricter declaration
/// would cost against real data before anyone commits to it. Both are
/// declarable; [`ConstraintProfile::Spine`] is the default.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum ConstraintProfile {
    /// Identity + routing only: the keys every read path dispatches on.
    ///
    /// These cannot change without a redesign of the adapter's record
    /// encoding, which makes them the safest possible declaration.
    #[default]
    Spine,
    /// The full `base_properties` set: the spine plus `summary`, and on edges
    /// the write-sequence key.
    ///
    /// Stricter, and equally true of every record the adapter has ever
    /// written — but `summary` is a display string rather than something a read
    /// path dispatches on, so requiring it buys less invariant for the same
    /// future-bump exposure.
    FullBase,
}

impl ConstraintProfile {
    /// Every profile, for enumeration and CLI validation.
    pub const ALL: [Self; 2] = [Self::Spine, Self::FullBase];

    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spine => "spine",
            Self::FullBase => "full-base",
        }
    }

    /// Parses a profile from its wire token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == token)
    }

    /// The property specs this profile declares on `entity_kind`.
    ///
    /// Returned sorted by property name so the declaration — and therefore the
    /// report, the sidecar, and every diff of either — is deterministic.
    #[must_use]
    pub fn specs(self, entity_kind: EntityKindToken) -> Vec<PropertySpec> {
        use DeclaredTypeToken::{Integer, String};

        // The universal `base_properties` spine, present on every node, every
        // tombstone, and every edge the adapter writes.
        let mut specs = vec![
            PropertySpec::required("codegraph_id", String),
            PropertySpec::required("record_type", String),
            PropertySpec::required("schema_version", Integer),
        ];

        if entity_kind == EntityKindToken::Edge {
            // Routing keys the edge read path dispatches on. `label` is
            // duplicated into properties alongside the store-side edge type;
            // the endpoint ids are the graph handles the read path resolves.
            specs.push(PropertySpec::required("label", String));
            specs.push(PropertySpec::required("source_codegraph_id", String));
            specs.push(PropertySpec::required("target_codegraph_id", String));
        }

        if self == Self::FullBase {
            specs.push(PropertySpec::required("summary", String));
            if entity_kind == EntityKindToken::Edge {
                // NOTE: `egregore_seq` holds a write-sequence NUMBER that the
                // adapter inserts as a STRING (`seq.to_string()`). Declaring it
                // Integer would make every future edge write fail, so the
                // profile records what is actually written, not what the name
                // suggests. This mismatch is one of the concrete findings the
                // conformance audit exists to surface.
                specs.push(PropertySpec::required("egregore_seq", String));
            }
        }

        specs.sort_by(|a, b| a.property.cmp(b.property));
        specs
    }
}

/// Every node label the embedded adapter can write, sorted.
///
/// Derived from [`NodeKind::ALL`] (itself pinned exhaustive by a wildcard-free
/// compile-time guard) plus [`TOMBSTONE_LABEL`], so it can never drift from
/// `adapters::aletheiadb::node_label`.
#[must_use]
pub fn writable_node_labels() -> Vec<&'static str> {
    let mut labels: Vec<&'static str> = NodeKind::ALL.iter().map(|kind| kind.as_str()).collect();
    labels.push(TOMBSTONE_LABEL);
    labels.sort_unstable();
    labels
}

/// Every edge type the embedded adapter can write, sorted.
#[must_use]
pub fn writable_edge_types() -> Vec<&'static str> {
    let mut types: Vec<&'static str> = EdgeLabel::ALL.iter().map(|label| label.as_str()).collect();
    types.sort_unstable();
    types
}

/// Whether a label was scanned, and what the scan found.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ConformanceStatus {
    /// The store holds current-state entities of this label and all conform.
    Conforms,
    /// The store holds current-state entities of this label and some do not.
    Violates,
    /// The store holds no current-state entity of this label.
    ///
    /// Reported distinctly rather than as a vacuous `Conforms`: a label with
    /// nothing in it proves nothing about conformance, and calling it
    /// conforming would overstate what the audit actually checked.
    NotPresent,
}

impl ConformanceStatus {
    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conforms => "conforms",
            Self::Violates => "violates",
            Self::NotPresent => "not_present",
        }
    }
}

/// One aggregated violation within a label's conformance row.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ViolationRow {
    /// The offending property key, or `None` when the violation is not
    /// property-specific.
    pub property: Option<String>,
    /// Upstream's human-readable reason (e.g. `missing required key`).
    pub reason: String,
    /// Offending records, cited by their Egregore `codegraph_id` handle.
    ///
    /// Upstream samples engine-internal `u64` entity ids; those are neither
    /// citable nor stable across a re-ingest, so the adapter resolves them to
    /// record handles before they reach the report.
    pub sample_record_ids: Vec<String>,
    /// Sampled entities whose handle could not be resolved.
    ///
    /// Counted rather than leaked: an unresolvable sample is exactly the case
    /// where the record has no `codegraph_id` to cite, and emitting the raw
    /// engine id would put an unstable internal identifier in the report.
    pub unresolved_samples: usize,
}

impl ViolationRow {
    /// The JSON shape rendered in the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "property": self.property,
            "reason": self.reason,
            "sample_record_ids": self.sample_record_ids,
            "unresolved_samples": self.unresolved_samples,
        })
    }
}

/// One label's conformance result.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct LabelConformance {
    /// Whether this row describes a node label or an edge type.
    pub entity_kind: EntityKindToken,
    /// The store-side label.
    pub label: String,
    /// The scan outcome.
    pub status: ConformanceStatus,
    /// Current-state entities of this label the scan checked.
    ///
    /// This is a CURRENT-STATE count: upstream scans the current-state view, so
    /// superseded record versions are not checked and this will be lower than
    /// `eg inspect --data-dir`'s physical record totals on a re-ingested store.
    pub checked: usize,
    /// How many of them do not conform.
    pub non_conforming: usize,
    /// Aggregated violations, sorted.
    pub violations: Vec<ViolationRow>,
}

impl LabelConformance {
    /// A row for a label the store holds nothing of.
    #[must_use]
    pub fn not_present(entity_kind: EntityKindToken, label: &str) -> Self {
        Self {
            entity_kind,
            label: label.to_owned(),
            status: ConformanceStatus::NotPresent,
            checked: 0,
            non_conforming: 0,
            violations: Vec::new(),
        }
    }

    /// The deterministic sort key: entity kind, then label.
    const fn sort_key(&self) -> (EntityKindToken, &str) {
        (self.entity_kind, self.label.as_str())
    }

    /// The JSON shape rendered in the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "entity_kind": self.entity_kind.as_str(),
            "label": self.label,
            "status": self.status.as_str(),
            "checked": self.checked,
            "non_conforming": self.non_conforming,
            "violations": self.violations.iter().map(ViolationRow::to_json).collect::<Vec<_>>(),
        })
    }
}

/// A constraint declaration read back from the store.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeclaredConstraint {
    /// `node` or `edge`.
    pub entity_kind: String,
    /// The label the declaration is scoped to.
    pub label: String,
    /// The declared property keys, sorted.
    pub properties: Vec<String>,
}

impl DeclaredConstraint {
    /// The JSON shape rendered in the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "entity_kind": self.entity_kind,
            "label": self.label,
            "properties": self.properties,
        })
    }
}

/// What the command did.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
pub enum ConstraintAction {
    /// Read-only Phase 1 report.
    #[default]
    Report,
    /// Phase 2 declaration.
    Declare,
    /// Phase 2 retraction.
    Drop,
}

impl ConstraintAction {
    /// The stable wire token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Report => "report",
            Self::Declare => "declare",
            Self::Drop => "drop",
        }
    }
}

/// The verbatim epistemic boundary carried on every report.
pub const DISCLAIMER: &str = "conformance is a structural check of property presence and type on \
current-state entities only; it is never proof that a record's content is correct, that its \
domain schema version is semantically compatible, or that extraction was complete. superseded \
record versions are not scanned. a label reported not_present holds no current-state entity and \
was therefore not checked.";

/// The complete report.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SchemaConstraintReport {
    /// What the command did.
    pub action: ConstraintAction,
    /// The store this ran against.
    pub data_dir: String,
    /// The profile evaluated (or declared).
    pub profile: ConstraintProfile,
    /// Per-label conformance rows, sorted.
    pub rows: Vec<LabelConformance>,
    /// Node labels present in the store that Egregore cannot write, sorted.
    ///
    /// A non-empty list means the store holds entities from a foreign writer or
    /// a newer Egregore — worth knowing before declaring anything, since those
    /// labels are outside the inventory a declaration would cover.
    pub unknown_node_labels: Vec<String>,
    /// Edge types present in the store that Egregore cannot write, sorted.
    pub unknown_edge_types: Vec<String>,
    /// Constraints the store currently has declared, sorted.
    pub declared_constraints: Vec<DeclaredConstraint>,
    /// Labels a `--declare` run declared on.
    pub declared_labels: usize,
    /// Labels a `--drop` run retracted.
    pub dropped_labels: usize,
}

impl SchemaConstraintReport {
    /// Sorts every collection into canonical order.
    ///
    /// Called before rendering so output is byte-identical across runs
    /// regardless of the order the store iterates labels in.
    pub fn canonicalize(&mut self) {
        self.rows.sort_by(|a, b| a.sort_key().cmp(&b.sort_key()));
        for row in &mut self.rows {
            for violation in &mut row.violations {
                violation.sample_record_ids.sort();
                violation.sample_record_ids.truncate(MAX_SAMPLE_RECORD_IDS);
            }
            row.violations
                .sort_by(|a, b| (&a.property, &a.reason).cmp(&(&b.property, &b.reason)));
        }
        self.unknown_node_labels.sort();
        self.unknown_edge_types.sort();
        self.declared_constraints
            .sort_by(|a, b| (&a.entity_kind, &a.label).cmp(&(&b.entity_kind, &b.label)));
    }

    /// Total current-state entities checked across every scanned label.
    #[must_use]
    pub fn entities_checked(&self) -> usize {
        self.rows.iter().map(|row| row.checked).sum()
    }

    /// Total non-conforming current-state entities.
    #[must_use]
    pub fn entities_non_conforming(&self) -> usize {
        self.rows.iter().map(|row| row.non_conforming).sum()
    }

    /// Labels actually scanned (i.e. holding at least one current-state entity).
    #[must_use]
    pub fn labels_scanned(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status != ConformanceStatus::NotPresent)
            .count()
    }

    /// Labels scanned and found conforming.
    #[must_use]
    pub fn labels_conforming(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == ConformanceStatus::Conforms)
            .count()
    }

    /// Whether the gate passes: no scanned entity violates the profile.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.entities_non_conforming() == 0
    }

    /// The deterministic single-line JSON contract documented in
    /// `docs/cli/schema-constraints.md`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let node_labels = writable_node_labels();
        let edge_types = writable_edge_types();
        serde_json::json!({
            "ok": self.ok(),
            "action": self.action.as_str(),
            "data_dir": self.data_dir,
            "profile": self.profile.as_str(),
            "profile_properties": {
                "node": self
                    .profile
                    .specs(EntityKindToken::Node)
                    .iter()
                    .map(PropertySpec::to_json)
                    .collect::<Vec<_>>(),
                "edge": self
                    .profile
                    .specs(EntityKindToken::Edge)
                    .iter()
                    .map(PropertySpec::to_json)
                    .collect::<Vec<_>>(),
            },
            "inventory": {
                "label_partition": LABEL_PARTITION,
                "writable_node_labels": node_labels.len(),
                "writable_edge_types": edge_types.len(),
                "node_labels": node_labels,
                "edge_types": edge_types,
            },
            "observed": {
                "unknown_node_labels": self.unknown_node_labels,
                "unknown_edge_types": self.unknown_edge_types,
            },
            "conformance": {
                "labels_scanned": self.labels_scanned(),
                "labels_conforming": self.labels_conforming(),
                "entities_checked": self.entities_checked(),
                "entities_non_conforming": self.entities_non_conforming(),
                "rows": self.rows.iter().map(LabelConformance::to_json).collect::<Vec<_>>(),
            },
            "declared_constraints": self
                .declared_constraints
                .iter()
                .map(DeclaredConstraint::to_json)
                .collect::<Vec<_>>(),
            "declared_labels": self.declared_labels,
            "dropped_labels": self.dropped_labels,
            "disclaimer": DISCLAIMER,
        })
    }

    /// The human-readable rendering. The JSON remains the complete contract.
    #[must_use]
    pub fn to_text(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(out, "action: {}", self.action.as_str());
        let _ = writeln!(out, "data_dir: {}", self.data_dir);
        let _ = writeln!(out, "profile: {}", self.profile.as_str());
        let _ = writeln!(out, "label_partition: {LABEL_PARTITION}");
        let _ = writeln!(
            out,
            "writable labels: {} node, {} edge",
            writable_node_labels().len(),
            writable_edge_types().len()
        );
        let _ = writeln!(
            out,
            "conformance: {} labels scanned, {} conforming, {} entities checked, {} non-conforming",
            self.labels_scanned(),
            self.labels_conforming(),
            self.entities_checked(),
            self.entities_non_conforming()
        );

        for row in &self.rows {
            if row.status == ConformanceStatus::NotPresent {
                continue;
            }
            let _ = writeln!(
                out,
                "  {} {}: {} ({} checked, {} non-conforming)",
                row.entity_kind.as_str(),
                row.label,
                row.status.as_str(),
                row.checked,
                row.non_conforming
            );
            for violation in &row.violations {
                let _ = writeln!(
                    out,
                    "    {} - {} [{}]",
                    violation.property.as_deref().unwrap_or("(entity)"),
                    violation.reason,
                    violation.sample_record_ids.join(", ")
                );
            }
        }

        if !self.unknown_node_labels.is_empty() {
            let _ = writeln!(
                out,
                "unknown node labels in store: {}",
                self.unknown_node_labels.join(", ")
            );
        }
        if !self.unknown_edge_types.is_empty() {
            let _ = writeln!(
                out,
                "unknown edge types in store: {}",
                self.unknown_edge_types.join(", ")
            );
        }
        let _ = writeln!(
            out,
            "declared constraints: {}",
            self.declared_constraints.len()
        );
        if self.action == ConstraintAction::Declare {
            let _ = writeln!(out, "declared labels: {}", self.declared_labels);
        }
        if self.action == ConstraintAction::Drop {
            let _ = writeln!(out, "dropped labels: {}", self.dropped_labels);
        }
        let _ = writeln!(out, "disclaimer: {DISCLAIMER}");
        out
    }
}

/// Node labels present in `observed` that Egregore cannot write.
#[must_use]
pub fn unknown_node_labels(observed: &[String]) -> Vec<String> {
    let known: BTreeSet<&str> = writable_node_labels().into_iter().collect();
    let mut unknown: Vec<String> = observed
        .iter()
        .filter(|label| !known.contains(label.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Edge types present in `observed` that Egregore cannot write.
#[must_use]
pub fn unknown_edge_types(observed: &[String]) -> Vec<String> {
    let known: BTreeSet<&str> = writable_edge_types().into_iter().collect();
    let mut unknown: Vec<String> = observed
        .iter()
        .filter(|label| !known.contains(label.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown.dedup();
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_node_labels_cover_every_kind_plus_tombstone() {
        let labels = writable_node_labels();
        assert_eq!(labels.len(), NodeKind::ALL.len() + 1);
        for kind in NodeKind::ALL {
            assert!(labels.contains(&kind.as_str()), "{}", kind.as_str());
        }
        assert!(labels.contains(&TOMBSTONE_LABEL));
    }

    #[test]
    fn writable_labels_are_sorted_and_distinct() {
        for labels in [writable_node_labels(), writable_edge_types()] {
            let mut sorted = labels.clone();
            sorted.sort_unstable();
            assert_eq!(labels, sorted, "labels must be emitted sorted");
            let before = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), before, "labels must be distinct");
        }
    }

    /// The tombstone label must not collide with a `NodeKind` name, or the
    /// inventory would double-count it and a declaration would silently cover
    /// two record shapes with one rule.
    #[test]
    fn tombstone_label_is_not_a_node_kind() {
        assert!(
            !NodeKind::ALL
                .iter()
                .any(|kind| kind.as_str() == TOMBSTONE_LABEL)
        );
    }

    #[test]
    fn spine_profile_is_identity_and_routing_only() {
        let node: Vec<&str> = ConstraintProfile::Spine
            .specs(EntityKindToken::Node)
            .iter()
            .map(|s| s.property)
            .collect();
        assert_eq!(node, ["codegraph_id", "record_type", "schema_version"]);

        let edge: Vec<&str> = ConstraintProfile::Spine
            .specs(EntityKindToken::Edge)
            .iter()
            .map(|s| s.property)
            .collect();
        assert_eq!(
            edge,
            [
                "codegraph_id",
                "label",
                "record_type",
                "schema_version",
                "source_codegraph_id",
                "target_codegraph_id",
            ]
        );
    }

    /// A per-domain `SCHEMA_VERSION` bump changes the VALUE, never the TYPE, so
    /// the declaration must be typed `int` for the constraint to survive it.
    #[test]
    fn schema_version_is_declared_integer_in_every_profile() {
        for profile in ConstraintProfile::ALL {
            for entity_kind in [EntityKindToken::Node, EntityKindToken::Edge] {
                let spec = profile
                    .specs(entity_kind)
                    .into_iter()
                    .find(|s| s.property == "schema_version")
                    .expect("schema_version is always declared");
                assert_eq!(spec.declared_type, DeclaredTypeToken::Integer);
                assert!(spec.required);
            }
        }
    }

    /// The adapter writes the edge write-sequence as `seq.to_string()`.
    /// Declaring it `Integer` would reject every future edge write.
    #[test]
    fn egregore_seq_is_declared_string_not_integer() {
        let spec = ConstraintProfile::FullBase
            .specs(EntityKindToken::Edge)
            .into_iter()
            .find(|s| s.property == "egregore_seq")
            .expect("full-base declares egregore_seq");
        assert_eq!(spec.declared_type, DeclaredTypeToken::String);
    }

    /// No profile may declare a per-kind payload field: those are exactly the
    /// keys a future schema slice is free to drop, and a constraint on one
    /// would turn a legitimate future write into a transaction abort.
    #[test]
    fn no_profile_declares_a_per_kind_payload_field() {
        const UNIVERSAL: [&str; 8] = [
            "codegraph_id",
            "record_type",
            "schema_version",
            "summary",
            "label",
            "source_codegraph_id",
            "target_codegraph_id",
            "egregore_seq",
        ];
        for profile in ConstraintProfile::ALL {
            for entity_kind in [EntityKindToken::Node, EntityKindToken::Edge] {
                for spec in profile.specs(entity_kind) {
                    assert!(
                        UNIVERSAL.contains(&spec.property),
                        "{} declares non-universal key {}",
                        profile.as_str(),
                        spec.property
                    );
                }
            }
        }
    }

    #[test]
    fn full_base_is_a_strict_superset_of_spine() {
        for entity_kind in [EntityKindToken::Node, EntityKindToken::Edge] {
            let spine = ConstraintProfile::Spine.specs(entity_kind);
            let full = ConstraintProfile::FullBase.specs(entity_kind);
            for spec in &spine {
                assert!(full.contains(spec), "full-base must keep {}", spec.property);
            }
            assert!(full.len() > spine.len());
        }
    }

    #[test]
    fn specs_are_sorted_by_property() {
        for profile in ConstraintProfile::ALL {
            for entity_kind in [EntityKindToken::Node, EntityKindToken::Edge] {
                let specs = profile.specs(entity_kind);
                let mut sorted = specs.clone();
                sorted.sort_by(|a, b| a.property.cmp(b.property));
                assert_eq!(specs, sorted);
            }
        }
    }

    #[test]
    fn profile_tokens_round_trip() {
        for profile in ConstraintProfile::ALL {
            assert_eq!(ConstraintProfile::parse(profile.as_str()), Some(profile));
        }
        assert_eq!(ConstraintProfile::parse("nope"), None);
        assert_eq!(ConstraintProfile::default(), ConstraintProfile::Spine);
    }

    fn sample_report() -> SchemaConstraintReport {
        SchemaConstraintReport {
            action: ConstraintAction::Report,
            data_dir: "/store".to_owned(),
            profile: ConstraintProfile::Spine,
            rows: vec![
                LabelConformance {
                    entity_kind: EntityKindToken::Edge,
                    label: "CONTAINS".to_owned(),
                    status: ConformanceStatus::Conforms,
                    checked: 3,
                    non_conforming: 0,
                    violations: Vec::new(),
                },
                LabelConformance {
                    entity_kind: EntityKindToken::Node,
                    label: "Symbol".to_owned(),
                    status: ConformanceStatus::Violates,
                    checked: 2,
                    non_conforming: 1,
                    violations: vec![
                        ViolationRow {
                            property: Some("record_type".to_owned()),
                            reason: "missing required key".to_owned(),
                            sample_record_ids: vec!["z".to_owned(), "a".to_owned()],
                            unresolved_samples: 1,
                        },
                        ViolationRow {
                            property: Some("codegraph_id".to_owned()),
                            reason: "missing required key".to_owned(),
                            sample_record_ids: vec!["a".to_owned()],
                            unresolved_samples: 0,
                        },
                    ],
                },
                LabelConformance::not_present(EntityKindToken::Node, "Task"),
            ],
            unknown_node_labels: vec!["Zeta".to_owned(), "Alpha".to_owned()],
            unknown_edge_types: Vec::new(),
            declared_constraints: Vec::new(),
            declared_labels: 0,
            dropped_labels: 0,
        }
    }

    #[test]
    fn canonicalize_sorts_every_collection_deterministically() {
        let mut report = sample_report();
        report.canonicalize();

        // Rows: node before edge would be wrong; the key is (entity_kind, label)
        // and `Node` sorts before `Edge` by declaration order.
        let keys: Vec<(&str, &str)> = report
            .rows
            .iter()
            .map(|r| (r.entity_kind.as_str(), r.label.as_str()))
            .collect();
        assert_eq!(
            keys,
            [("node", "Symbol"), ("node", "Task"), ("edge", "CONTAINS")]
        );

        let symbol = &report.rows[0];
        assert_eq!(
            symbol
                .violations
                .iter()
                .map(|v| v.property.as_deref().unwrap_or(""))
                .collect::<Vec<_>>(),
            ["codegraph_id", "record_type"]
        );
        assert_eq!(symbol.violations[1].sample_record_ids, ["a", "z"]);
        assert_eq!(report.unknown_node_labels, ["Alpha", "Zeta"]);
    }

    #[test]
    fn canonicalize_is_idempotent_and_output_is_stable() {
        let mut once = sample_report();
        once.canonicalize();
        let mut twice = sample_report();
        twice.canonicalize();
        twice.canonicalize();
        assert_eq!(once, twice);
        assert_eq!(once.to_json().to_string(), twice.to_json().to_string());
    }

    #[test]
    fn aggregates_count_only_scanned_labels() {
        let mut report = sample_report();
        report.canonicalize();
        assert_eq!(report.entities_checked(), 5);
        assert_eq!(report.entities_non_conforming(), 1);
        assert_eq!(report.labels_scanned(), 2, "not_present rows are not scans");
        assert_eq!(report.labels_conforming(), 1);
        assert!(!report.ok());
    }

    #[test]
    fn a_store_with_only_absent_labels_passes_the_gate() {
        let report = SchemaConstraintReport {
            rows: vec![LabelConformance::not_present(
                EntityKindToken::Node,
                "Symbol",
            )],
            ..sample_report()
        };
        assert!(report.ok(), "nothing checked means nothing violated");
        assert_eq!(report.labels_scanned(), 0);
    }

    #[test]
    fn sample_record_ids_are_capped() {
        let mut report = sample_report();
        report.rows[1].violations[0].sample_record_ids =
            (0..50).map(|i| format!("id-{i:03}")).collect();
        report.canonicalize();
        let row = report
            .rows
            .iter()
            .find(|r| r.label == "Symbol")
            .expect("symbol row");
        assert_eq!(
            row.violations[1].sample_record_ids.len(),
            MAX_SAMPLE_RECORD_IDS
        );
    }

    #[test]
    fn json_carries_the_inventory_and_the_disclaimer() {
        let mut report = sample_report();
        report.canonicalize();
        let json = report.to_json();
        assert_eq!(json["inventory"]["label_partition"], LABEL_PARTITION);
        assert_eq!(
            json["inventory"]["writable_node_labels"],
            writable_node_labels().len()
        );
        assert_eq!(json["disclaimer"], DISCLAIMER);
        assert_eq!(json["ok"], false);
        assert_eq!(json["action"], "report");
    }

    #[test]
    fn text_rendering_omits_absent_labels_but_keeps_findings() {
        let mut report = sample_report();
        report.canonicalize();
        let text = report.to_text();
        assert!(text.contains("profile: spine"));
        assert!(text.contains(LABEL_PARTITION));
        assert!(text.contains("node Symbol: violates"));
        assert!(!text.contains("Task"), "not_present rows are omitted");
    }

    #[test]
    fn unknown_labels_are_the_store_minus_the_inventory() {
        let observed = vec![
            "Symbol".to_owned(),
            "FutureKind".to_owned(),
            "Tombstone".to_owned(),
            "FutureKind".to_owned(),
        ];
        assert_eq!(unknown_node_labels(&observed), ["FutureKind"]);

        let observed_edges = vec!["CONTAINS".to_owned(), "FUTURE_LABEL".to_owned()];
        assert_eq!(unknown_edge_types(&observed_edges), ["FUTURE_LABEL"]);
    }
}
