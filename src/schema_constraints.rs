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
//! (`node_label(kind) == kind.as_str()`, pinned by a test at the write site),
//! plus the literal [`TOMBSTONE_LABEL`]
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

/// Upstream's own per-violation sample bound
/// (`aletheiadb::core::constraint::MAX_CONFORMANCE_SAMPLE_IDS`).
///
/// Mirrored here because it is load-bearing for DETERMINISM, not merely for
/// output size. See [`ViolationRow::sample_complete`].
pub const ENGINE_SAMPLE_BOUND: usize = 16;

/// Maximum foreign labels enumerated per unknown-label list.
///
/// The lists are built from labels read out of the store, so their length is
/// controlled by whoever wrote to it; without a cap a store carrying many
/// foreign labels would produce an unbounded report.
pub const MAX_UNKNOWN_LABELS: usize = 32;

/// Maximum declarations enumerated in `declared_constraints`.
///
/// Comfortably above the real ceiling (one per writable label), so a normal
/// declared store is never truncated, while a crafted `schema_constraints.dat`
/// sidecar cannot produce an unbounded report.
pub const MAX_DECLARED_CONSTRAINTS: usize = 256;

/// Sanitizes one HANDLE value (a record ID) read back from the store, **without**
/// truncating it.
///
/// Control characters are neutralized — a `codegraph_id` is an ordinary node
/// property that nothing validates on write, so a foreign writer can plant one
/// carrying newlines that forge extra `--format text` rows or the ESC that
/// starts an ANSI sequence. The length cap is deliberately NOT applied: a
/// truncated handle is no longer a citation, and a prefix of a record ID
/// silently reads like a valid one. Mirrors `criteria_coverage::handle_field`
/// and the `evidence_pack` two-tier split.
#[must_use]
pub fn handle_field(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { '.' } else { c })
        .collect()
}

/// Sanitizes and length-caps one FREE-TEXT value read back from the store or its
/// sidecar.
///
/// Delegates to the single hardened #104 implementation
/// ([`crate::embeddings::bounded_identity_field`]) so this lane, the
/// semantic-index refusal path, the control-catalog envelope, and the
/// criteria-coverage census all share one rule.
#[must_use]
pub fn bounded_field(value: &str) -> String {
    crate::embeddings::bounded_identity_field(value)
}

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
    /// Stricter, and true of every record written by CURRENT Egregore — but not
    /// necessarily of an old one: the adapter itself documents legacy edges
    /// predating `egregore_seq` that carry no such property, so declaring
    /// `full-base` on a store holding them is REFUSED. That refusal is the
    /// report doing its job, not a defect.
    ///
    /// `summary` is also a display string rather than something a read path
    /// dispatches on, so requiring it buys less invariant for the same
    /// future-bump exposure. Hence `spine` is the default.
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
/// Derived from [`NodeKind::ALL`] (pinned exhaustive against `serde`'s own
/// variant list) plus [`TOMBSTONE_LABEL`] (which the tombstone write site now
/// uses directly). `node_label_is_exactly_the_kind_string_for_every_kind` pins
/// the remaining link — that the adapter really does write `kind.as_str()` as
/// the store-side label — so this cannot drift from what is written.
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
    /// Whether the engine's sample was the COMPLETE set of offenders for this
    /// violation — and therefore whether citations can be emitted at all.
    ///
    /// Upstream keeps only the first [`ENGINE_SAMPLE_BOUND`] offending ids per
    /// `(property, reason)` group, in the iteration order of a `DashMap` whose
    /// hasher is seeded per PROCESS. When a group has more offenders than that
    /// bound, the SUBSET upstream samples therefore differs between runs — so
    /// sorting it does not rescue determinism, and the resulting handful of ids
    /// would in any case be a misleading citation ("these 8 records" when it is
    /// an arbitrary 8 of thousands).
    ///
    /// So the citations are emitted only when they are provably complete, which
    /// is exactly when the engine returned FEWER ids than its own bound. The
    /// counts (`checked` / `non_conforming`) are unaffected — those are exact
    /// and order-independent either way — so the gate verdict never depends on
    /// this, only the citations do.
    pub sample_complete: bool,
}

impl ViolationRow {
    /// The JSON shape rendered in the report.
    ///
    /// Handles are control-sanitized but never truncated; a truncated record ID
    /// stops being a citation. When the engine's sample was incomplete the ids
    /// are omitted entirely (see [`ViolationRow::sample_complete`]).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "property": self.property.as_deref().map(bounded_field),
            "reason": bounded_field(&self.reason),
            "sample_record_ids": self.citable_record_ids(),
            "unresolved_samples": self.unresolved_samples,
            "sample_complete": self.sample_complete,
        })
    }

    /// The record ids safe to publish: sanitized, and empty when the engine's
    /// sample was not provably complete.
    #[must_use]
    pub fn citable_record_ids(&self) -> Vec<String> {
        if !self.sample_complete {
            return Vec::new();
        }
        self.sample_record_ids
            .iter()
            .map(|id| handle_field(id))
            .collect()
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
    /// Store entities of this label the scan checked.
    ///
    /// Upstream scans its CURRENT-STATE view — the set of live engine entities.
    /// Egregore's embedded adapter is APPEND-ONLY (every record version is its
    /// own `create_node` / `create_edge`; the only `update_node` is the
    /// embedding backfill), so an Egregore-superseded record version is a
    /// distinct live engine entity and IS scanned. This count therefore tracks
    /// `eg inspect --data-dir`'s physical record totals for the label, not a
    /// deduplicated current-record count.
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

/// The result of a `--declare` run, including a refusal's partial progress.
///
/// Declaration is per-label and NOT atomic across labels, so a refusal can leave
/// the store half-constrained. Returning the landed count ALONGSIDE the refusal
/// (rather than an `Err` that discards it) is what lets the CLI print the full
/// report and name the exact state the operator now has to clean up.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct DeclarationOutcome {
    /// How many labels were declared before the run stopped.
    pub declared: usize,
    /// Upstream's refusal, or `None` when every label declared.
    pub refusal: Option<String>,
}

/// The result of a `--drop` run.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct DropOutcome {
    /// Declarations actually retracted, captured before the drop.
    pub dropped: Vec<DeclaredConstraint>,
    /// Declarations deliberately left in place because their label is outside
    /// Egregore's writable inventory (or carries an unrecognized entity kind).
    pub foreign_retained: Vec<DeclaredConstraint>,
    /// The store error that stopped the run, or `None` when every candidate was
    /// processed.
    ///
    /// Retraction is NOT atomic across labels, exactly as declaration is not, so
    /// a failure partway through leaves the store already modified. Returning
    /// this alongside the accumulated `dropped` — rather than discarding the
    /// outcome with `?` — is what keeps the before-image of the labels that DID
    /// drop; upstream rewrites the sidecar atomically, so a discarded outcome
    /// would be the permanent loss of the only record of them.
    pub refusal: Option<String>,
}

/// One property descriptor within a constraint declaration read back from the
/// store.
///
/// Carries the COMPLETE descriptor, not just the key name, because
/// `dropped_constraints` is a before-image an operator must be able to restore
/// from. For a label in Egregore's own inventory the name alone would do — a
/// re-run of `--declare <profile>` regenerates the type and optionality from
/// the profile — but `--drop --include-foreign` can retract a declaration made
/// by another tool, and Egregore's `--declare` can never reconstruct that one.
/// Recording only the key names there would leave the "recoverable" claim
/// false in exactly the case the before-image exists for.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeclaredProperty {
    /// The property key.
    pub property: String,
    /// Upstream's stable type token (`string`, `int`, `float`, `bool`,
    /// `temporal`, `bytes`, `vector`), or `None` when the declaration accepts
    /// any type.
    pub declared_type: Option<String>,
    /// The required vector dimension, when `declared_type` is `vector` and the
    /// declaration pins one. `type_name()` alone collapses every `Vector`
    /// arm to `vector`, so the dimension is captured separately or a restored
    /// declaration would silently widen to accept any dimension.
    pub vector_dim: Option<usize>,
    /// Whether the key must be present with a non-null value.
    pub required: bool,
    /// Whether an explicit null is permitted.
    pub nullable: bool,
}

impl DeclaredProperty {
    /// The JSON shape rendered in the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "property": bounded_field(&self.property),
            "declared_type": self
                .declared_type
                .as_ref()
                .map(|declared| bounded_field(declared)),
            "vector_dim": self.vector_dim,
            "required": self.required,
            "nullable": self.nullable,
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
    /// The declared property descriptors, sorted by key.
    pub properties: Vec<DeclaredProperty>,
}

impl DeclaredConstraint {
    /// The JSON shape rendered in the report.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "entity_kind": bounded_field(&self.entity_kind),
            "label": bounded_field(&self.label),
            "properties": self
                .properties
                .iter()
                .map(DeclaredProperty::to_json)
                .collect::<Vec<_>>(),
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
pub const DISCLAIMER: &str = "conformance is a structural check of property presence and type; it \
is never proof that a record's content is correct, that its domain schema version is semantically \
compatible, or that extraction was complete. only labels egregore itself can write are scanned - \
entities under a label listed in observed.unknown_node_labels or observed.unknown_edge_types were \
NOT checked, so a passing verdict says nothing about them. a label reported not_present holds no \
entity of that label and was therefore not checked. enforcement is forward-only: a declaration \
constrains future writes and never re-validates history.";

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
    /// Node labels dropped from `unknown_node_labels` by the display cap, so a
    /// bounded list is never mistaken for a complete one.
    pub unknown_node_labels_omitted: usize,
    /// Edge types present in the store that Egregore cannot write, sorted.
    pub unknown_edge_types: Vec<String>,
    /// Edge types dropped from `unknown_edge_types` by the display cap.
    pub unknown_edge_types_omitted: usize,
    /// Constraints the store currently has declared, sorted.
    pub declared_constraints: Vec<DeclaredConstraint>,
    /// Declarations dropped from `declared_constraints` by the display cap.
    /// Re-readable from the store at any time, unlike `dropped_constraints`.
    pub declared_constraints_omitted: usize,
    /// Labels a `--declare` run declared on.
    ///
    /// Set even when the run was refused part-way, so a partially-constrained
    /// store is always visible rather than inferred.
    pub declared_labels: usize,
    /// The `(entity_kind, label)` at which a `--declare` run was refused, and
    /// upstream's reason.
    ///
    /// Declaration is per-label and NOT atomic across labels, so a refusal can
    /// leave the store half-constrained. Naming the refusing label makes that
    /// state actionable instead of merely detectable.
    pub declaration_refusal: Option<String>,
    /// Labels a `--drop` run retracted.
    pub dropped_labels: usize,
    /// The declarations a `--drop` run removed, captured BEFORE the drop.
    ///
    /// Upstream atomically rewrites the sidecar on every drop, so without this
    /// before-image a mistaken `--drop` would be unrecoverable by inspection:
    /// you cannot re-declare what you can no longer enumerate.
    pub dropped_constraints: Vec<DeclaredConstraint>,
    /// Declarations a `--drop` run deliberately LEFT in place because their
    /// label is outside Egregore's writable inventory.
    ///
    /// `--drop` is the inverse of `--declare`, and `--declare` only ever touches
    /// Egregore's own labels; silently destroying another tool's constraints
    /// would exceed that inverse.
    pub foreign_constraints_retained: Vec<DeclaredConstraint>,
    /// Declarations dropped from `foreign_constraints_retained` by the display
    /// cap. These were RETAINED in the store, so they remain re-readable.
    pub foreign_constraints_retained_omitted: usize,
    /// The store error that stopped a `--drop` partway, or `None`.
    ///
    /// Set when retraction failed after at least one label had already been
    /// retracted, so the report discloses that the store is now partially
    /// dropped rather than reporting only a bare failure.
    pub drop_refusal: Option<String>,
    /// Whether a conformance scan actually ran.
    ///
    /// `--drop` evaluates no profile, so its zeroed conformance block must not
    /// read as "this store conforms".
    pub conformance_evaluated: bool,
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
                violation.sample_record_ids.dedup();
                // Truncating a COMPLETE sample would make it incomplete without
                // saying so, so the cap also clears the completeness marker.
                if violation.sample_record_ids.len() > MAX_SAMPLE_RECORD_IDS {
                    violation.sample_record_ids.truncate(MAX_SAMPLE_RECORD_IDS);
                    violation.sample_complete = false;
                }
            }
            row.violations
                .sort_by(|a, b| (&a.property, &a.reason).cmp(&(&b.property, &b.reason)));
        }
        // A bounded list that silently loses its tail reads as a COMPLETE list.
        // These arrays name what the scan could not cover, so an undisclosed
        // truncation hides exactly the namespaces the operator needs to see.
        for (labels, omitted) in [
            (
                &mut self.unknown_node_labels,
                &mut self.unknown_node_labels_omitted,
            ),
            (
                &mut self.unknown_edge_types,
                &mut self.unknown_edge_types_omitted,
            ),
        ] {
            labels.sort();
            labels.dedup();
            // ACCUMULATE, never assign: `canonicalize` is public and idempotent,
            // and a second call sees an ALREADY-truncated list whose recomputed
            // overflow is zero. Assigning would reset a real count to 0 and hand
            // the consumer a bounded list that claims to be complete - the exact
            // failure this field exists to prevent. Adding 0 is a no-op.
            *omitted += labels.len().saturating_sub(MAX_UNKNOWN_LABELS);
            labels.truncate(MAX_UNKNOWN_LABELS);
        }
        for (declarations, omitted) in [
            (
                &mut self.declared_constraints,
                &mut self.declared_constraints_omitted,
            ),
            (
                &mut self.foreign_constraints_retained,
                &mut self.foreign_constraints_retained_omitted,
            ),
        ] {
            declarations
                .sort_by(|a, b| (&a.entity_kind, &a.label).cmp(&(&b.entity_kind, &b.label)));
            // Accumulated for the same reason as the unknown-label lists above.
            *omitted += declarations.len().saturating_sub(MAX_DECLARED_CONSTRAINTS);
            declarations.truncate(MAX_DECLARED_CONSTRAINTS);
        }
        // `dropped_constraints` is deliberately NOT capped. The other lists
        // describe current state, which the store can always be re-read for; this
        // one is a RECOVERY RECORD for declarations that no longer exist
        // anywhere. Retraction has already happened by the time this runs and
        // upstream rewrote the sidecar atomically, so a truncated entry is not a
        // shortened report - it is a declaration that can never be rebuilt.
        // Bounding it would cap the report at the cost of the contract it exists
        // to serve.
        self.dropped_constraints
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

    /// Whether the gate passes.
    ///
    /// A run that evaluated no profile (`--drop`) or was refused mid-declaration
    /// does not pass VACUOUSLY: `conformance_evaluated == false` would otherwise
    /// let an empty `rows` read as "this store conforms", which is exactly the
    /// false statement this lane exists to avoid.
    #[must_use]
    pub fn ok(&self) -> bool {
        if self.declaration_refusal.is_some() || self.drop_refusal.is_some() {
            return false;
        }
        if !self.conformance_evaluated {
            // Nothing was checked, so there is nothing to fail - but the report
            // must say so rather than imply conformance. See `to_json`, which
            // omits the conformance block entirely in this case.
            return true;
        }
        self.entities_non_conforming() == 0
    }

    /// Whether the store holds entities under labels this lane did not scan.
    ///
    /// A foreign label is outside Egregore's inventory, so its entities are
    /// never evaluated; a passing verdict says nothing about them.
    #[must_use]
    pub const fn has_unchecked_labels(&self) -> bool {
        !self.unknown_node_labels.is_empty() || !self.unknown_edge_types.is_empty()
    }

    /// The deterministic single-line JSON contract documented in
    /// `docs/cli/schema-constraints.md`.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let node_labels = writable_node_labels();
        let edge_types = writable_edge_types();
        let mut report = serde_json::json!({
            "ok": self.ok(),
            "action": self.action.as_str(),
            "data_dir": self.data_dir,
            "conformance_evaluated": self.conformance_evaluated,
            "unchecked_unknown_labels": self.has_unchecked_labels(),
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
                "unknown_node_labels_omitted": self.unknown_node_labels_omitted,
                "unknown_edge_types": self.unknown_edge_types,
                "unknown_edge_types_omitted": self.unknown_edge_types_omitted,
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
            "declaration_refusal": self.declaration_refusal.as_deref().map(bounded_field),
            "declared_constraints_omitted": self.declared_constraints_omitted,
            "declared_labels": self.declared_labels,
            "dropped_labels": self.dropped_labels,
            // Never truncated, so `dropped_labels` and this array always agree.
            "dropped_constraints": self
                .dropped_constraints
                .iter()
                .map(DeclaredConstraint::to_json)
                .collect::<Vec<_>>(),
            "drop_refusal": self.drop_refusal.as_deref().map(bounded_field),
            "foreign_constraints_retained": self
                .foreign_constraints_retained
                .iter()
                .map(DeclaredConstraint::to_json)
                .collect::<Vec<_>>(),
            "foreign_constraints_retained_omitted": self.foreign_constraints_retained_omitted,
            "disclaimer": DISCLAIMER,
        });

        // A run that evaluated no profile must not ship a zeroed conformance
        // block a consumer could read as "this store conforms", nor a
        // profile_properties block for a profile it never applied.
        if !self.conformance_evaluated
            && let Some(object) = report.as_object_mut()
        {
            object.remove("conformance");
            object.remove("profile_properties");
        }
        report
    }

    /// Renders the foreign-label lists, disclosing any capped tail.
    ///
    /// These name what the scan could NOT cover, so a bounded list rendered
    /// without its omitted count reads as the complete set of unscanned
    /// namespaces.
    fn write_unknown_labels(&self, out: &mut String) {
        use std::fmt::Write as _;

        for (heading, labels, omitted) in [
            (
                "unknown node labels in store",
                &self.unknown_node_labels,
                self.unknown_node_labels_omitted,
            ),
            (
                "unknown edge types in store",
                &self.unknown_edge_types,
                self.unknown_edge_types_omitted,
            ),
        ] {
            if labels.is_empty() {
                continue;
            }
            let rendered = labels
                .iter()
                .map(|label| bounded_field(label))
                .collect::<Vec<_>>()
                .join(", ");
            if omitted > 0 {
                let _ = writeln!(out, "{heading}: {rendered} (+{omitted} omitted)");
            } else {
                let _ = writeln!(out, "{heading}: {rendered}");
            }
        }
    }

    /// Renders the `--drop` before-image in full.
    ///
    /// These declarations no longer exist anywhere else - upstream rewrote the
    /// sidecar atomically - so a text-mode operator shown only a count has lost
    /// them, and the documented "recoverable by inspection" claim would be false
    /// for exactly the people reading this format. Every field needed to
    /// re-declare is emitted: key, type, vector dimension, and optionality.
    fn write_drop_before_image(&self, out: &mut String) {
        use std::fmt::Write as _;

        for declaration in &self.dropped_constraints {
            let properties = declaration
                .properties
                .iter()
                .map(|property| {
                    let mut rendered = bounded_field(&property.property);
                    if let Some(declared_type) = &property.declared_type {
                        let _ = write!(rendered, ":{}", bounded_field(declared_type));
                    }
                    if let Some(dim) = property.vector_dim {
                        let _ = write!(rendered, "[{dim}]");
                    }
                    if property.required {
                        rendered.push_str(" required");
                    }
                    if !property.nullable {
                        rendered.push_str(" non-null");
                    }
                    rendered
                })
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "  dropped {} {}: {properties}",
                bounded_field(&declaration.entity_kind),
                bounded_field(&declaration.label)
            );
        }
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
        // Mirrors `to_json`, which REMOVES the conformance block when no scan
        // ran. Printing "0 labels scanned, 0 non-conforming" for a `--drop` reads
        // as a clean bill of health on a destructive action that checked nothing.
        if self.conformance_evaluated {
            let _ = writeln!(
                out,
                "conformance: {} labels scanned, {} conforming, {} entities checked, {} non-conforming",
                self.labels_scanned(),
                self.labels_conforming(),
                self.entities_checked(),
                self.entities_non_conforming()
            );
        }

        for row in &self.rows {
            if row.status == ConformanceStatus::NotPresent {
                continue;
            }
            let _ = writeln!(
                out,
                "  {} {}: {} ({} checked, {} non-conforming)",
                row.entity_kind.as_str(),
                bounded_field(&row.label),
                row.status.as_str(),
                row.checked,
                row.non_conforming
            );
            for violation in &row.violations {
                // Every value here is either ours or read back from the store,
                // and the store-read ones are operator-controlled. They go
                // through the SAME sanitizers the JSON path uses, so a crafted
                // record can neither forge a report row nor drive the terminal.
                let _ = writeln!(
                    out,
                    "    {} - {} [{}]",
                    violation
                        .property
                        .as_deref()
                        .map_or_else(|| "(entity)".to_owned(), bounded_field),
                    bounded_field(&violation.reason),
                    violation.citable_record_ids().join(", ")
                );
            }
        }

        self.write_unknown_labels(&mut out);
        let _ = writeln!(
            out,
            "declared constraints: {}",
            self.declared_constraints.len()
        );
        if self.declared_constraints_omitted > 0 {
            let _ = writeln!(out, "  (+{} omitted)", self.declared_constraints_omitted);
        }
        if self.action == ConstraintAction::Declare {
            let _ = writeln!(out, "declared labels: {}", self.declared_labels);
        }
        if self.action == ConstraintAction::Drop {
            let _ = writeln!(out, "dropped labels: {}", self.dropped_labels);
            self.write_drop_before_image(&mut out);
        }
        if self.foreign_constraints_retained_omitted > 0 {
            let _ = writeln!(
                out,
                "foreign constraints retained: {} (+{} omitted)",
                self.foreign_constraints_retained.len(),
                self.foreign_constraints_retained_omitted
            );
        }
        // A partial declare/drop exits nonzero. Without these, a text-mode
        // operator is told only how many labels landed - not which one failed or
        // why - on precisely the non-atomic path where the store is now in a
        // half-applied state they have to clean up by hand.
        if let Some(refusal) = &self.declaration_refusal {
            let _ = writeln!(out, "declaration refused: {}", bounded_field(refusal));
        }
        if let Some(refusal) = &self.drop_refusal {
            let _ = writeln!(out, "drop refused: {}", bounded_field(refusal));
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
                            sample_complete: true,
                        },
                        ViolationRow {
                            property: Some("codegraph_id".to_owned()),
                            reason: "missing required key".to_owned(),
                            sample_record_ids: vec!["a".to_owned()],
                            unresolved_samples: 0,
                            sample_complete: true,
                        },
                    ],
                },
                LabelConformance::not_present(EntityKindToken::Node, "Task"),
            ],
            unknown_node_labels: vec!["Zeta".to_owned(), "Alpha".to_owned()],
            unknown_node_labels_omitted: 0,
            unknown_edge_types: Vec::new(),
            unknown_edge_types_omitted: 0,
            declared_constraints: Vec::new(),
            declared_constraints_omitted: 0,
            declared_labels: 0,
            declaration_refusal: None,
            dropped_labels: 0,
            dropped_constraints: Vec::new(),
            drop_refusal: None,
            foreign_constraints_retained: Vec::new(),
            foreign_constraints_retained_omitted: 0,
            conformance_evaluated: true,
        }
    }

    /// Builds `count` distinct declarations, labelled so sort order is stable.
    fn declarations(count: usize, prefix: &str) -> Vec<DeclaredConstraint> {
        (0..count)
            .map(|index| DeclaredConstraint {
                entity_kind: "node".to_owned(),
                label: format!("{prefix}{index:04}"),
                properties: vec![DeclaredProperty {
                    property: "their_key".to_owned(),
                    declared_type: Some("string".to_owned()),
                    vector_dim: None,
                    required: true,
                    nullable: false,
                }],
            })
            .collect()
    }

    /// The before-image must never be capped.
    ///
    /// By the time it is rendered the retraction has already happened and
    /// upstream has rewritten the sidecar, so a truncated entry is not a
    /// shortened report - it is a declaration nothing can rebuild. The other
    /// lists describe current state and stay bounded.
    #[test]
    fn the_drop_before_image_is_never_truncated() {
        let overflow = MAX_DECLARED_CONSTRAINTS + 47;
        let mut report = sample_report();
        report.dropped_constraints = declarations(overflow, "Dropped");
        report.dropped_labels = overflow;
        report.canonicalize();

        assert_eq!(
            report.dropped_constraints.len(),
            overflow,
            "capping the before-image would destroy declarations that no longer \
             exist anywhere else"
        );
        assert_eq!(
            report.dropped_constraints.len(),
            report.dropped_labels,
            "dropped_labels and dropped_constraints must never disagree - a \
             consumer reading a short array against a larger count cannot tell \
             which descriptors were lost"
        );

        // Still sorted, so output stays byte-identical across runs.
        let labels: Vec<&str> = report
            .dropped_constraints
            .iter()
            .map(|declaration| declaration.label.as_str())
            .collect();
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        assert_eq!(labels, sorted);
    }

    /// A bounded list that loses its tail silently reads as a complete one.
    #[test]
    fn bounded_lists_disclose_what_they_omit() {
        let mut report = sample_report();
        report.unknown_node_labels = (0..MAX_UNKNOWN_LABELS + 8)
            .map(|index| format!("Foreign{index:04}"))
            .collect();
        report.unknown_edge_types = (0..MAX_UNKNOWN_LABELS + 3)
            .map(|index| format!("FOREIGN_EDGE_{index:04}"))
            .collect();
        report.declared_constraints = declarations(MAX_DECLARED_CONSTRAINTS + 5, "Declared");
        report.foreign_constraints_retained =
            declarations(MAX_DECLARED_CONSTRAINTS + 2, "Retained");
        report.canonicalize();

        assert_eq!(report.unknown_node_labels.len(), MAX_UNKNOWN_LABELS);
        assert_eq!(report.unknown_node_labels_omitted, 8);
        assert_eq!(report.unknown_edge_types.len(), MAX_UNKNOWN_LABELS);
        assert_eq!(report.unknown_edge_types_omitted, 3);
        assert_eq!(report.declared_constraints.len(), MAX_DECLARED_CONSTRAINTS);
        assert_eq!(report.declared_constraints_omitted, 5);
        assert_eq!(
            report.foreign_constraints_retained.len(),
            MAX_DECLARED_CONSTRAINTS
        );
        assert_eq!(report.foreign_constraints_retained_omitted, 2);

        // And the counts reach the rendered report, not just the struct.
        let json = report.to_json();
        assert_eq!(json["observed"]["unknown_node_labels_omitted"], 8);
        assert_eq!(json["observed"]["unknown_edge_types_omitted"], 3);
        assert_eq!(json["declared_constraints_omitted"], 5);
        assert_eq!(json["foreign_constraints_retained_omitted"], 2);
    }

    /// A list that fits under its cap must report nothing omitted, so a `0` is
    /// meaningful rather than a default nobody maintains.
    #[test]
    fn a_list_within_its_cap_omits_nothing() {
        let mut report = sample_report();
        report.declared_constraints = declarations(3, "Declared");
        report.canonicalize();

        assert_eq!(report.declared_constraints.len(), 3);
        assert_eq!(report.declared_constraints_omitted, 0);
        assert_eq!(report.unknown_node_labels_omitted, 0);
    }

    /// `canonicalize` is public and documented idempotent, so an overflow count
    /// must survive a second call.
    ///
    /// The second call sees an ALREADY-truncated list, so recomputing the
    /// overflow from it yields zero. Assigning that would reset a real count and
    /// hand the consumer a bounded list claiming to be complete - the precise
    /// failure the field was added to prevent.
    #[test]
    fn omitted_counts_survive_a_second_canonicalize() {
        let mut report = sample_report();
        report.unknown_node_labels = (0..MAX_UNKNOWN_LABELS + 8)
            .map(|index| format!("Foreign{index:04}"))
            .collect();
        report.declared_constraints = declarations(MAX_DECLARED_CONSTRAINTS + 5, "Declared");
        report.foreign_constraints_retained =
            declarations(MAX_DECLARED_CONSTRAINTS + 2, "Retained");

        report.canonicalize();
        let first = (
            report.unknown_node_labels_omitted,
            report.declared_constraints_omitted,
            report.foreign_constraints_retained_omitted,
        );
        assert_eq!(first, (8, 5, 2));

        report.canonicalize();
        assert_eq!(
            (
                report.unknown_node_labels_omitted,
                report.declared_constraints_omitted,
                report.foreign_constraints_retained_omitted,
            ),
            first,
            "a second canonicalize must not reset the omitted counts to zero"
        );
        assert_eq!(report.to_json(), {
            let mut again = report.clone();
            again.canonicalize();
            again.to_json()
        });
    }

    /// `--drop` scans nothing, so the text view must not print a conformance
    /// line - `to_json` deliberately removes that block for the same reason.
    #[test]
    fn text_omits_the_conformance_line_when_nothing_was_scanned() {
        let mut report = sample_report();
        report.action = ConstraintAction::Drop;
        report.conformance_evaluated = false;
        report.rows = Vec::new();
        report.canonicalize();

        let text = report.to_text();
        assert!(
            !text.contains("conformance:"),
            "a drop that scanned nothing must not print a conformance verdict, \
             vacuous or otherwise:\n{text}"
        );
        assert!(report.to_json().get("conformance").is_none());
    }

    /// The text view must carry the before-image, not just its count.
    #[test]
    fn text_renders_the_full_drop_before_image() {
        let mut report = sample_report();
        report.action = ConstraintAction::Drop;
        report.conformance_evaluated = false;
        report.rows = Vec::new();
        report.dropped_constraints = vec![DeclaredConstraint {
            entity_kind: "node".to_owned(),
            label: "SomeOtherToolsLabel".to_owned(),
            properties: vec![DeclaredProperty {
                property: "their_embedding".to_owned(),
                declared_type: Some("vector".to_owned()),
                vector_dim: Some(384),
                required: true,
                nullable: false,
            }],
        }];
        report.dropped_labels = 1;
        report.canonicalize();

        let text = report.to_text();
        // Every field needed to re-declare it must be present.
        assert!(text.contains("SomeOtherToolsLabel"), "{text}");
        assert!(text.contains("their_embedding"), "{text}");
        assert!(text.contains("vector"), "{text}");
        assert!(text.contains("384"), "{text}");
        assert!(text.contains("required"), "{text}");
        assert!(text.contains("non-null"), "{text}");
    }

    /// A partial declare/drop must say WHICH label failed, in both formats.
    #[test]
    fn text_renders_the_refusal_reason() {
        let mut report = sample_report();
        report.action = ConstraintAction::Drop;
        report.conformance_evaluated = false;
        report.rows = Vec::new();
        report.dropped_labels = 3;
        report.drop_refusal =
            Some("dropping schema constraints failed at Symbol: disk full".to_owned());
        report.canonicalize();

        let text = report.to_text();
        assert!(text.contains("drop refused"), "{text}");
        assert!(text.contains("Symbol"), "{text}");
        assert!(text.contains("dropped labels: 3"), "{text}");

        let mut declaring = sample_report();
        declaring.action = ConstraintAction::Declare;
        declaring.declaration_refusal = Some("refused at CONTAINS: sidecar unwritable".to_owned());
        declaring.canonicalize();
        assert!(declaring.to_text().contains("declaration refused"));
    }

    /// A partial drop is a failure verdict, not a success with a note.
    #[test]
    fn a_partial_drop_fails_the_report() {
        let mut report = sample_report();
        report.rows = Vec::new();
        report.conformance_evaluated = false;
        assert!(report.ok(), "no refusal, nothing scanned - vacuously fine");

        report.drop_refusal = Some("dropping schema constraints failed at X".to_owned());
        assert!(
            !report.ok(),
            "a store left partially dropped must not exit 0"
        );
        assert!(report.to_json()["drop_refusal"].is_string());
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
