//! Versioned SOC2 control->evidence-class catalog: loader, validator, and BLAKE3
//! hash-pin (issue #337).
//!
//! This module is **pure and deterministic**: every function here performs no
//! I/O, never prints, and never exits. Parsing, validation, canonical
//! serialization, and hashing all operate on in-memory values and return owned
//! results, so the same input yields byte-identical output across runs.
//!
//! Output is redaction-safe: reports carry only IDs, handles, hashes, and counts
//! — never raw payload. The catalog maps a control ID to Egregore evidence
//! classes; it is **not** an interpretation of the AICPA Trust Services Criteria
//! and **not** legal advice. Inclusion of a control ID asserts nothing about an
//! organization's compliance obligations or the effectiveness of its controls:
//! evidence of process execution, never proof of control effectiveness or
//! compliance.
//!
//! The catalog document format, evidence-class vocabulary, and hash-pin contract
//! are documented in `docs/controls/README.md`; the embedded default catalog is
//! `docs/controls/soc2-v1.json`.

use serde::{Deserialize, Serialize};

pub use crate::schema_version::{
    CONTROL_CATALOG_DOMAIN, CONTROL_CATALOG_KIND, CONTROL_CATALOG_SCHEMA_VERSION,
    is_known_control_catalog_schema_version,
};

/// The embedded default SOC2 control catalog (`docs/controls/soc2-v1.json`).
///
/// This document must always parse; [`load_default_catalog`] relies on it.
pub const DEFAULT_SOC2_CATALOG_JSON: &str = include_str!("../docs/controls/soc2-v1.json");

/// The closed set of Egregore evidence classes a control can require.
///
/// The variant order here is fixed and mirrored by [`EvidenceClass::ALL`]; wire
/// names are `snake_case` and stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceClass {
    /// Git commit records.
    Commits,
    /// Pull-request records.
    PullRequests,
    /// Code-review records.
    Reviews,
    /// Review-coverage measurement over changed surface.
    ReviewCoverage,
    /// Structural (symbol/file) deltas across a change.
    StructuralDeltas,
    /// Public-API surface deltas across a change.
    PublicApiDeltas,
    /// Validation-run records (e.g. `eg validate`).
    ValidationRuns,
    /// Verification-evidence records (command runs, test runs, CI status).
    VerificationEvidence,
    /// Error-signature records.
    ErrorSignatures,
    /// Occurrence-bucket aggregates over error signatures.
    OccurrenceBuckets,
    /// Links from an incident to its remediation.
    RemediationLinks,
}

impl EvidenceClass {
    /// The closed evidence-class set, in fixed order.
    pub const ALL: [Self; 11] = [
        Self::Commits,
        Self::PullRequests,
        Self::Reviews,
        Self::ReviewCoverage,
        Self::StructuralDeltas,
        Self::PublicApiDeltas,
        Self::ValidationRuns,
        Self::VerificationEvidence,
        Self::ErrorSignatures,
        Self::OccurrenceBuckets,
        Self::RemediationLinks,
    ];

    /// Returns the stable `snake_case` wire name for this class.
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Commits => "commits",
            Self::PullRequests => "pull_requests",
            Self::Reviews => "reviews",
            Self::ReviewCoverage => "review_coverage",
            Self::StructuralDeltas => "structural_deltas",
            Self::PublicApiDeltas => "public_api_deltas",
            Self::ValidationRuns => "validation_runs",
            Self::VerificationEvidence => "verification_evidence",
            Self::ErrorSignatures => "error_signatures",
            Self::OccurrenceBuckets => "occurrence_buckets",
            Self::RemediationLinks => "remediation_links",
        }
    }

    /// Parses a wire name into an evidence class, or `None` when unknown.
    #[must_use]
    pub fn from_wire(wire: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_wire() == wire)
    }
}

/// Whether a control requires an evidence class or merely reports it optional.
///
/// Closed two-value vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// The class must be present or the control gate fails.
    Required,
    /// The class is reported when present; its absence never fails the gate.
    Optional,
}

impl Requirement {
    /// Returns the stable wire name for this requirement.
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Optional => "optional",
        }
    }

    /// Parses a wire name into a requirement, or `None` when unknown.
    #[must_use]
    pub fn from_wire(wire: &str) -> Option<Self> {
        match wire {
            "required" => Some(Self::Required),
            "optional" => Some(Self::Optional),
            _ => None,
        }
    }
}

/// The `(domain, kind, version)` schema tuple of a catalog document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSchemaVersion {
    /// Record domain namespace (`control_catalog`).
    pub domain: String,
    /// Record kind (`ControlCatalog`).
    pub kind: String,
    /// Schema version (`1`).
    pub version: u32,
}

/// One evidence class paired with its requirement level in a control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClassRequirement {
    /// The evidence class.
    pub class: EvidenceClass,
    /// Whether the class is required or optional for this control.
    pub requirement: Requirement,
}

/// One control and the evidence classes it maps to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Control {
    /// Stable control identifier (e.g. `CC8.1`).
    pub control_id: String,
    /// Human-readable control statement.
    pub title: String,
    /// The evidence classes this control maps to.
    pub evidence_classes: Vec<ClassRequirement>,
}

/// A parsed, validated control catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ControlCatalog {
    /// Stable catalog identifier (e.g. `soc2-v1`).
    pub catalog_id: String,
    /// The catalog's schema-version tuple.
    pub schema_version: CatalogSchemaVersion,
    /// The controls in this catalog.
    pub controls: Vec<Control>,
}

/// Errors produced while parsing or validating a control catalog.
///
/// Every variant carries a stable [`CatalogError::code`] and a redaction-safe
/// [`CatalogError::to_json`] envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogError {
    /// The document was not well-formed JSON or did not match the catalog shape.
    ///
    /// Carries only value-free diagnostics from the serde error — the 1-based
    /// line and column of the failure and its stable category — never the raw
    /// `serde_json::Error` message, which for a wrong-type field echoes the
    /// offending catalog value (e.g. `invalid type: string "SECRET", expected
    /// u32`) and would violate the module's redaction-safe error contract.
    Json {
        /// 1-based line of the parse failure (`serde_json::Error::line`).
        line: usize,
        /// 1-based column of the parse failure (`serde_json::Error::column`).
        column: usize,
        /// Stable failure category from `serde_json::Error::classify`:
        /// `"io"`, `"syntax"`, `"data"`, or `"eof"`.
        category: &'static str,
    },
    /// The schema-version tuple was not the recognized `(control_catalog, ControlCatalog, 1)`.
    UnknownSchemaVersion {
        /// Declared domain.
        domain: String,
        /// Declared kind.
        kind: String,
        /// Declared version.
        version: u32,
    },
    /// A control named an evidence class outside the closed vocabulary.
    UnknownEvidenceClass {
        /// The offending control's identifier.
        control_id: String,
        /// The unrecognized class wire string.
        class: String,
    },
    /// A control named a requirement outside the closed `{required, optional}` set.
    InvalidRequirement {
        /// The offending control's identifier.
        control_id: String,
        /// The class the invalid requirement was attached to.
        class: String,
        /// The unrecognized requirement wire string.
        requirement: String,
    },
    /// A control listed the same evidence class (by wire name) more than once.
    ///
    /// Duplicate `{class, requirement}` entries would leave the canonical class
    /// sort with ties, so two catalogs differing only in the order of those
    /// duplicates could hash differently — breaking the order-independence
    /// contract. The first offending duplicate in document order is reported.
    DuplicateEvidenceClass {
        /// The offending control's identifier.
        control_id: String,
        /// The duplicated class wire string.
        class: String,
    },
    /// Two controls declared the same `control_id`.
    ///
    /// Controls are sorted by `control_id` in canonical form; duplicate IDs
    /// would tie identically. The first offending duplicate in document order is
    /// reported.
    DuplicateControl {
        /// The duplicated control identifier.
        control_id: String,
    },
}

impl CatalogError {
    /// Returns the stable machine-readable code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Json { .. } => "malformed_json",
            Self::UnknownSchemaVersion { .. } => "unknown_schema_version",
            Self::UnknownEvidenceClass { .. } => "unknown_evidence_class",
            Self::InvalidRequirement { .. } => "invalid_requirement",
            Self::DuplicateEvidenceClass { .. } => "duplicate_evidence_class",
            Self::DuplicateControl { .. } => "duplicate_control_id",
        }
    }

    /// Builds a redaction-safe JSON envelope for this error.
    ///
    /// The `unknown_schema_version` shape matches the repo-wide reader contract
    /// (`{"code":..,"version":{"domain","kind","version"}}`).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Json {
                line,
                column,
                category,
            } => serde_json::json!({
                "code": self.code(),
                "line": line,
                "column": column,
                "category": category,
            }),
            Self::UnknownSchemaVersion {
                domain,
                kind,
                version,
            } => serde_json::json!({
                "code": self.code(),
                "version": { "domain": domain, "kind": kind, "version": version },
            }),
            Self::UnknownEvidenceClass { control_id, class }
            | Self::DuplicateEvidenceClass { control_id, class } => serde_json::json!({
                "code": self.code(),
                "control_id": control_id,
                "class": class,
            }),
            Self::InvalidRequirement {
                control_id,
                class,
                requirement,
            } => serde_json::json!({
                "code": self.code(),
                "control_id": control_id,
                "class": class,
                "requirement": requirement,
            }),
            Self::DuplicateControl { control_id } => serde_json::json!({
                "code": self.code(),
                "control_id": control_id,
            }),
        }
    }
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

impl std::error::Error for CatalogError {}

/// Raw deserialization target: classes and requirements as strings, so unknown
/// values become named [`CatalogError`]s rather than opaque serde failures.
///
/// `deny_unknown_fields` is load-bearing for supported-version catalogs: an
/// unknown/extra key in a custom `--catalog` must fail deserialization (mapped
/// to [`CatalogError::Json`]) rather than being silently dropped before
/// `canonical_bytes` hashes the catalog. Otherwise an off-schema catalog could
/// produce the same `control_catalog:v1:<hash>` pin as the shipped document,
/// breaking the #337 guarantee that the hash-pin ties an evidence pack to exact
/// catalog content. The strict [`RawCatalog`] deserialize runs only after the
/// lenient [`SchemaVersionProbe`] version gate passes, so unknown fields in an
/// *unsupported*-version document are reported as
/// [`CatalogError::UnknownSchemaVersion`], not masked as `malformed_json`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    catalog_id: String,
    schema_version: RawSchemaVersion,
    controls: Vec<RawControl>,
}

/// Raw schema-version target, distinct from the public [`CatalogSchemaVersion`]
/// so `deny_unknown_fields` guards the deserialize path without altering the
/// public model's serde behavior.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchemaVersion {
    domain: String,
    kind: String,
    version: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControl {
    control_id: String,
    title: String,
    evidence_classes: Vec<RawClassRequirement>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawClassRequirement {
    class: String,
    requirement: String,
}

/// Lenient probe extracting only the `schema_version` tuple for the phase-1
/// version gate. Intentionally *without* `deny_unknown_fields`: serde's default
/// ignores every other field, so an unsupported-but-well-formed future catalog
/// (a bumped `version` plus added/renamed fields) still surfaces its tuple and
/// is reported as [`CatalogError::UnknownSchemaVersion`] rather than being
/// masked as `malformed_json` by the strict [`RawCatalog`] deserialize.
#[derive(Debug, Deserialize)]
struct SchemaVersionProbe {
    schema_version: SchemaVersionTupleProbe,
}

/// Lenient schema-version tuple probe. No `deny_unknown_fields`: unknown keys
/// inside `schema_version` are ignored during the version gate; the strict v1
/// shape (phase 2) still rejects them for supported-version catalogs.
#[derive(Debug, Deserialize)]
struct SchemaVersionTupleProbe {
    domain: String,
    kind: String,
    version: u32,
}

/// Maps a `serde_json::Error` to a redaction-safe [`CatalogError::Json`].
///
/// A wrong-type field makes `serde_json::Error::to_string()` embed the offending
/// catalog value (e.g. `invalid type: string "SECRET", expected u32`). Capturing
/// only the 1-based line/column and the stable `classify` category keeps the
/// error envelope value-free, honoring the module's redaction-safe contract.
fn sanitize_json_error(error: &serde_json::Error) -> CatalogError {
    use serde_json::error::Category;
    let category = match error.classify() {
        Category::Io => "io",
        Category::Syntax => "syntax",
        Category::Data => "data",
        Category::Eof => "eof",
    };
    CatalogError::Json {
        line: error.line(),
        column: error.column(),
        category,
    }
}

/// Parses and validates a control catalog document.
///
/// Pure: no I/O. Line endings are normalized (`\r\n` -> `\n`) before parsing as
/// belt-and-suspenders; the hash is over the re-serialized canonical form and so
/// is line-ending independent regardless.
///
/// # Errors
///
/// Returns [`CatalogError::Json`] for malformed JSON,
/// [`CatalogError::UnknownSchemaVersion`] when the schema tuple is not
/// `(control_catalog, ControlCatalog, 1)`, [`CatalogError::UnknownEvidenceClass`]
/// for a class outside the closed vocabulary (first offender in document order),
/// [`CatalogError::InvalidRequirement`] for a requirement outside
/// `{required, optional}`, [`CatalogError::DuplicateEvidenceClass`] when a
/// control lists the same class more than once, and
/// [`CatalogError::DuplicateControl`] when two controls share a `control_id`
/// (both report the first offender in document order). Rejecting duplicates
/// removes the sort ties that would otherwise make `canonical_bytes` depend on
/// input order.
pub fn parse_catalog(text: &str) -> Result<ControlCatalog, CatalogError> {
    let normalized = text.replace("\r\n", "\n");

    // Phase 1 — lenient version gate. Probe only the `schema_version` tuple,
    // ignoring every other field, so an unsupported-but-well-formed future
    // catalog is reported as `unknown_schema_version` (with its tuple) rather
    // than masked as `malformed_json` by the strict `RawCatalog` deserialize.
    // A structurally broken document or one missing `schema_version` fails the
    // probe and maps to `malformed_json`.
    let probe: SchemaVersionProbe =
        serde_json::from_str(&normalized).map_err(|error| sanitize_json_error(&error))?;

    if !is_known_control_catalog_schema_version(
        &probe.schema_version.domain,
        &probe.schema_version.kind,
        probe.schema_version.version,
    ) {
        return Err(CatalogError::UnknownSchemaVersion {
            domain: probe.schema_version.domain,
            kind: probe.schema_version.kind,
            version: probe.schema_version.version,
        });
    }

    // Phase 2 — strict v1 shape. Only after the version gate passes do we hold
    // the document to the exact v1 body; unknown fields here still map to
    // `malformed_json`.
    let raw: RawCatalog =
        serde_json::from_str(&normalized).map_err(|error| sanitize_json_error(&error))?;

    let schema_version = CatalogSchemaVersion {
        domain: raw.schema_version.domain,
        kind: raw.schema_version.kind,
        version: raw.schema_version.version,
    };

    let mut controls = Vec::with_capacity(raw.controls.len());
    let mut seen_control_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for raw_control in raw.controls {
        if !seen_control_ids.insert(raw_control.control_id.clone()) {
            return Err(CatalogError::DuplicateControl {
                control_id: raw_control.control_id,
            });
        }
        let mut evidence_classes = Vec::with_capacity(raw_control.evidence_classes.len());
        let mut seen_classes: std::collections::HashSet<&'static str> =
            std::collections::HashSet::new();
        for raw_class in raw_control.evidence_classes {
            let class = EvidenceClass::from_wire(&raw_class.class).ok_or_else(|| {
                CatalogError::UnknownEvidenceClass {
                    control_id: raw_control.control_id.clone(),
                    class: raw_class.class.clone(),
                }
            })?;
            if !seen_classes.insert(class.as_wire()) {
                return Err(CatalogError::DuplicateEvidenceClass {
                    control_id: raw_control.control_id,
                    class: class.as_wire().to_owned(),
                });
            }
            let requirement = Requirement::from_wire(&raw_class.requirement).ok_or_else(|| {
                CatalogError::InvalidRequirement {
                    control_id: raw_control.control_id.clone(),
                    class: raw_class.class.clone(),
                    requirement: raw_class.requirement.clone(),
                }
            })?;
            evidence_classes.push(ClassRequirement { class, requirement });
        }
        controls.push(Control {
            control_id: raw_control.control_id,
            title: raw_control.title,
            evidence_classes,
        });
    }

    Ok(ControlCatalog {
        catalog_id: raw.catalog_id,
        schema_version,
        controls,
    })
}

/// Canonical form for hashing: fixed field order, controls sorted by
/// `control_id`, each control's classes sorted by `(class wire name,
/// requirement)` — a total order, since parsing rejects duplicate classes and
/// duplicate control IDs.
///
/// Built from `#[derive(Serialize)]` structs whose fields serialize in
/// declaration order, so the output is independent of `serde_json`'s
/// `preserve_order` feature (which makes `Value` maps insertion-ordered).
#[derive(Serialize)]
struct CanonicalCatalog<'a> {
    catalog_id: &'a str,
    schema_version: CanonicalSchemaVersion<'a>,
    controls: Vec<CanonicalControl<'a>>,
}

#[derive(Serialize)]
struct CanonicalSchemaVersion<'a> {
    domain: &'a str,
    kind: &'a str,
    version: u32,
}

#[derive(Serialize)]
struct CanonicalControl<'a> {
    control_id: &'a str,
    title: &'a str,
    evidence_classes: Vec<CanonicalClassRequirement>,
}

#[derive(Serialize)]
struct CanonicalClassRequirement {
    class: &'static str,
    requirement: &'static str,
}

/// Serializes a catalog to its deterministic canonical byte form.
///
/// Object keys are in fixed declared order, controls are sorted by `control_id`,
/// and each control's evidence classes are sorted by `(class wire name,
/// requirement)`. The output is byte-identical across runs and independent of
/// the input's control/class ordering.
///
/// # Panics
///
/// Never in practice: serializing the fixed-shape canonical struct of plain
/// strings and integers to a byte vector is infallible.
#[must_use]
pub fn canonical_bytes(catalog: &ControlCatalog) -> Vec<u8> {
    let mut controls: Vec<CanonicalControl<'_>> = catalog
        .controls
        .iter()
        .map(|control| {
            let mut classes: Vec<CanonicalClassRequirement> = control
                .evidence_classes
                .iter()
                .map(|cr| CanonicalClassRequirement {
                    class: cr.class.as_wire(),
                    requirement: cr.requirement.as_wire(),
                })
                .collect();
            // Total order: parsing already rejects duplicate classes within a
            // control, but the (class, requirement) tie-breaker is
            // belt-and-suspenders so canonical bytes can never depend on input
            // order even if a duplicate somehow slipped through.
            classes.sort_by(|a, b| {
                a.class
                    .cmp(b.class)
                    .then_with(|| a.requirement.cmp(b.requirement))
            });
            CanonicalControl {
                control_id: &control.control_id,
                title: &control.title,
                evidence_classes: classes,
            }
        })
        .collect();
    controls.sort_by(|a, b| a.control_id.cmp(b.control_id));

    let canonical = CanonicalCatalog {
        catalog_id: &catalog.catalog_id,
        schema_version: CanonicalSchemaVersion {
            domain: &catalog.schema_version.domain,
            kind: &catalog.schema_version.kind,
            version: catalog.schema_version.version,
        },
        controls,
    };
    // Serializing a struct with fixed fields is infallible for these plain types.
    serde_json::to_vec(&canonical).expect("canonical catalog serialization is infallible")
}

/// Computes the BLAKE3 hash-pin handle for a catalog.
///
/// The handle shape mirrors `stable_id`'s `domain:vN:<hex>`.
#[must_use]
pub fn catalog_hash(catalog: &ControlCatalog) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&canonical_bytes(catalog));
    format!(
        "{}:v{}:{}",
        CONTROL_CATALOG_DOMAIN,
        CONTROL_CATALOG_SCHEMA_VERSION,
        hasher.finalize().to_hex()
    )
}

/// A hash-pin echoing a catalog's identity and canonical hash.
///
/// This is exactly what a future evidence-pack manifest (issue #338) records so
/// a pack can be tied to the catalog version it was assembled against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogPin {
    /// The catalog's stable identifier.
    pub catalog_id: String,
    /// The catalog's schema-version tuple.
    pub catalog_schema_version: CatalogSchemaVersion,
    /// The catalog's BLAKE3 hash handle.
    pub catalog_hash: String,
}

/// Builds a [`CatalogPin`] for a catalog.
#[must_use]
pub fn pin(catalog: &ControlCatalog) -> CatalogPin {
    CatalogPin {
        catalog_id: catalog.catalog_id.clone(),
        catalog_schema_version: catalog.schema_version.clone(),
        catalog_hash: catalog_hash(catalog),
    }
}

/// Whether an evidence class was found present or is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Availability {
    /// Evidence of this class was found.
    Present,
    /// No evidence of this class was found.
    Unavailable,
}

/// The outcome of evaluating one class requirement against its availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClassOutcome {
    /// The required-or-optional class was present.
    Pass,
    /// A required class was unavailable — the control gate fails.
    GateFail,
    /// An optional class was unavailable — reported, but the gate still passes.
    ReportedOptionalUnavailable,
}

impl ClassOutcome {
    /// Returns true unless this outcome fails the gate.
    #[must_use]
    pub const fn is_gate_pass(&self) -> bool {
        !matches!(self, Self::GateFail)
    }
}

/// The three-way requirement semantics #338 builds on.
///
/// `(_, Present) => Pass`, `(Required, Unavailable) => GateFail`,
/// `(Optional, Unavailable) => ReportedOptionalUnavailable`.
#[must_use]
pub const fn evaluate_requirement(req: Requirement, avail: Availability) -> ClassOutcome {
    match (req, avail) {
        (_, Availability::Present) => ClassOutcome::Pass,
        (Requirement::Required, Availability::Unavailable) => ClassOutcome::GateFail,
        (Requirement::Optional, Availability::Unavailable) => {
            ClassOutcome::ReportedOptionalUnavailable
        }
    }
}

/// Parses the embedded default SOC2 catalog.
///
/// # Panics
///
/// Panics if the embedded `docs/controls/soc2-v1.json` fails to parse; that
/// document is a compile-time constant and is covered by a test.
#[must_use]
pub fn load_default_catalog() -> ControlCatalog {
    parse_catalog(DEFAULT_SOC2_CATALOG_JSON)
        .expect("embedded default SOC2 catalog must always parse")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default() -> ControlCatalog {
        load_default_catalog()
    }

    #[test]
    fn default_catalog_parses_with_expected_identity() {
        let catalog = default();
        assert_eq!(catalog.catalog_id, "soc2-v1");
        assert_eq!(catalog.schema_version.domain, CONTROL_CATALOG_DOMAIN);
        assert_eq!(catalog.schema_version.kind, CONTROL_CATALOG_KIND);
        assert_eq!(
            catalog.schema_version.version,
            CONTROL_CATALOG_SCHEMA_VERSION
        );
        assert_eq!(catalog.controls.len(), 3);
    }

    #[test]
    fn default_catalog_has_expected_controls_and_requirement_split() {
        let catalog = default();
        let ids: Vec<&str> = catalog
            .controls
            .iter()
            .map(|c| c.control_id.as_str())
            .collect();
        assert_eq!(ids, ["CC8.1", "CC7.2", "CC7.3"]);

        let cc81 = &catalog.controls[0];
        let required: Vec<&'static str> = cc81
            .evidence_classes
            .iter()
            .filter(|cr| cr.requirement == Requirement::Required)
            .map(|cr| cr.class.as_wire())
            .collect();
        assert_eq!(
            required,
            ["commits", "pull_requests", "reviews", "review_coverage"]
        );
        let optional: Vec<&'static str> = cc81
            .evidence_classes
            .iter()
            .filter(|cr| cr.requirement == Requirement::Optional)
            .map(|cr| cr.class.as_wire())
            .collect();
        assert_eq!(
            optional,
            [
                "structural_deltas",
                "public_api_deltas",
                "validation_runs",
                "verification_evidence"
            ]
        );

        // CC7.2 and CC7.3 are entirely optional in v1.
        for control in &catalog.controls[1..] {
            assert!(
                control
                    .evidence_classes
                    .iter()
                    .all(|cr| cr.requirement == Requirement::Optional),
                "{} should be all-optional in v1",
                control.control_id
            );
        }
    }

    #[test]
    fn all_evidence_classes_round_trip() {
        assert_eq!(EvidenceClass::ALL.len(), 11);
        for class in EvidenceClass::ALL {
            assert_eq!(EvidenceClass::from_wire(class.as_wire()), Some(class));
        }
        assert_eq!(EvidenceClass::from_wire("not_a_class"), None);
    }

    #[test]
    fn requirement_round_trips() {
        for req in [Requirement::Required, Requirement::Optional] {
            assert_eq!(Requirement::from_wire(req.as_wire()), Some(req));
        }
        assert_eq!(Requirement::from_wire("mandatory"), None);
    }

    #[test]
    fn unknown_evidence_class_is_named() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                    { "class": "bogus_class", "requirement": "required" }
                ] }
            ]
        }"#;
        let err = parse_catalog(json).expect_err("unknown class must fail");
        assert_eq!(err.code(), "unknown_evidence_class");
        assert_eq!(
            err,
            CatalogError::UnknownEvidenceClass {
                control_id: "CC1.1".to_owned(),
                class: "bogus_class".to_owned(),
            }
        );
        let value = err.to_json();
        assert_eq!(value["code"], "unknown_evidence_class");
        assert_eq!(value["control_id"], "CC1.1");
        assert_eq!(value["class"], "bogus_class");
    }

    #[test]
    fn invalid_requirement_is_named() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                    { "class": "commits", "requirement": "mandatory" }
                ] }
            ]
        }"#;
        let err = parse_catalog(json).expect_err("invalid requirement must fail");
        assert_eq!(err.code(), "invalid_requirement");
        assert_eq!(
            err,
            CatalogError::InvalidRequirement {
                control_id: "CC1.1".to_owned(),
                class: "commits".to_owned(),
                requirement: "mandatory".to_owned(),
            }
        );
    }

    #[test]
    fn duplicate_evidence_class_within_control_is_rejected() {
        // Same class twice with the same requirement.
        let same_requirement = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                    { "class": "commits", "requirement": "required" },
                    { "class": "commits", "requirement": "required" }
                ] }
            ]
        }"#;
        let err = parse_catalog(same_requirement).expect_err("duplicate class must fail");
        assert_eq!(err.code(), "duplicate_evidence_class");
        assert_eq!(
            err,
            CatalogError::DuplicateEvidenceClass {
                control_id: "CC1.1".to_owned(),
                class: "commits".to_owned(),
            }
        );
        let value = err.to_json();
        assert_eq!(value["code"], "duplicate_evidence_class");
        assert_eq!(value["control_id"], "CC1.1");
        assert_eq!(value["class"], "commits");

        // Same class twice with conflicting requirements (required + optional):
        // exactly the ambiguity Codex flagged — the canonical sort would have
        // left these tied on class alone.
        let conflicting = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                    { "class": "commits", "requirement": "required" },
                    { "class": "commits", "requirement": "optional" }
                ] }
            ]
        }"#;
        let err = parse_catalog(conflicting).expect_err("conflicting duplicate class must fail");
        assert_eq!(err.code(), "duplicate_evidence_class");
        assert_eq!(
            err,
            CatalogError::DuplicateEvidenceClass {
                control_id: "CC1.1".to_owned(),
                class: "commits".to_owned(),
            }
        );
    }

    #[test]
    fn duplicate_control_id_is_rejected() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "first", "evidence_classes": [
                    { "class": "commits", "requirement": "required" }
                ] },
                { "control_id": "CC1.1", "title": "second", "evidence_classes": [
                    { "class": "reviews", "requirement": "optional" }
                ] }
            ]
        }"#;
        let err = parse_catalog(json).expect_err("duplicate control_id must fail");
        assert_eq!(err.code(), "duplicate_control_id");
        assert_eq!(
            err,
            CatalogError::DuplicateControl {
                control_id: "CC1.1".to_owned(),
            }
        );
        let value = err.to_json();
        assert_eq!(value["code"], "duplicate_control_id");
        assert_eq!(value["control_id"], "CC1.1");
    }

    #[test]
    fn wrong_schema_version_is_rejected_with_tuple() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 2 },
            "controls": []
        }"#;
        let err = parse_catalog(json).expect_err("version 2 must fail");
        assert_eq!(err.code(), "unknown_schema_version");
        assert_eq!(
            err,
            CatalogError::UnknownSchemaVersion {
                domain: "control_catalog".to_owned(),
                kind: "ControlCatalog".to_owned(),
                version: 2,
            }
        );
        let value = err.to_json();
        assert_eq!(value["code"], "unknown_schema_version");
        assert_eq!(value["version"]["domain"], "control_catalog");
        assert_eq!(value["version"]["kind"], "ControlCatalog");
        assert_eq!(value["version"]["version"], 2);
    }

    #[test]
    fn wrong_domain_or_kind_is_rejected() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "codegraph", "kind": "ControlCatalog", "version": 1 },
            "controls": []
        }"#;
        let err = parse_catalog(json).expect_err("wrong domain must fail");
        assert_eq!(err.code(), "unknown_schema_version");
    }

    #[test]
    fn future_version_with_unknown_fields_reports_unknown_schema_version() {
        // A future/third-party catalog that bumps the schema version AND adds or
        // renames fields must still be reported as `unknown_schema_version`
        // (with its tuple), not masked as `malformed_json` by the strict v1
        // shape — the version gate runs first (Codex P2, round 3).
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 2 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                    { "class": "commits", "requirement": "required", "weight": 3 }
                ], "surprise": 7 }
            ],
            "extra_top_level": true
        }"#;
        let err = parse_catalog(json).expect_err("future version with extra fields must fail");
        assert_eq!(err.code(), "unknown_schema_version");
        assert_ne!(err.code(), "malformed_json");
        assert_eq!(
            err,
            CatalogError::UnknownSchemaVersion {
                domain: "control_catalog".to_owned(),
                kind: "ControlCatalog".to_owned(),
                version: 2,
            }
        );
    }

    #[test]
    fn future_version_without_extra_fields_still_unknown_schema_version() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 2 },
            "controls": []
        }"#;
        let err = parse_catalog(json).expect_err("version 2 must fail");
        assert_eq!(err.code(), "unknown_schema_version");
        assert_eq!(
            err,
            CatalogError::UnknownSchemaVersion {
                domain: "control_catalog".to_owned(),
                kind: "ControlCatalog".to_owned(),
                version: 2,
            }
        );
    }

    #[test]
    fn malformed_json_is_rejected() {
        let err = parse_catalog("{ not valid json").expect_err("malformed json must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
    }

    #[test]
    fn malformed_json_error_does_not_leak_field_values() {
        // A wrong-type `schema_version.version` (string, not u32) makes
        // serde name the offending value in its raw message. The sanitized
        // error envelope must expose only line/column/category, never the value.
        let version_json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": "LEAK_SENTINEL_9271" },
            "controls": []
        }"#;
        let err = parse_catalog(version_json).expect_err("wrong-type version must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
        let rendered = serde_json::to_string(&err.to_json()).expect("serialize error envelope");
        assert!(
            rendered.contains("\"code\":\"malformed_json\""),
            "envelope must carry the stable code: {rendered}"
        );
        assert!(
            rendered.contains("\"line\":"),
            "envelope must carry a line field: {rendered}"
        );
        assert!(
            rendered.contains("\"column\":"),
            "envelope must carry a column field: {rendered}"
        );
        assert!(
            rendered.contains("\"category\":"),
            "envelope must carry a category field: {rendered}"
        );
        assert!(
            !rendered.contains("LEAK_SENTINEL_9271"),
            "sanitized error must not echo the catalog field value: {rendered}"
        );

        // A wrong-type `controls` (string, not array) carrying the same sentinel
        // must likewise never appear in the error output.
        let controls_json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": "LEAK_SENTINEL_9271"
        }"#;
        let err = parse_catalog(controls_json).expect_err("wrong-type controls must fail");
        assert_eq!(err.code(), "malformed_json");
        let rendered = serde_json::to_string(&err.to_json()).expect("serialize error envelope");
        assert!(
            rendered.contains("\"code\":\"malformed_json\""),
            "envelope must carry the stable code: {rendered}"
        );
        assert!(
            !rendered.contains("LEAK_SENTINEL_9271"),
            "sanitized error must not echo the catalog field value: {rendered}"
        );
    }

    #[test]
    fn unknown_top_level_field_is_rejected() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [],
            "extra_field": 1
        }"#;
        let err = parse_catalog(json).expect_err("unknown top-level key must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
    }

    #[test]
    fn unknown_schema_version_field_is_rejected() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1, "extra": true },
            "controls": []
        }"#;
        let err = parse_catalog(json).expect_err("unknown schema-version key must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
    }

    #[test]
    fn unknown_control_field_is_rejected() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [], "surprise": 7 }
            ]
        }"#;
        let err = parse_catalog(json).expect_err("unknown control key must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
    }

    #[test]
    fn unknown_evidence_class_field_is_rejected() {
        let json = r#"{
            "catalog_id": "x",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "CC1.1", "title": "t", "evidence_classes": [
                    { "class": "commits", "requirement": "required", "weight": 3 }
                ] }
            ]
        }"#;
        let err = parse_catalog(json).expect_err("unknown evidence-class key must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
    }

    #[test]
    fn default_catalog_still_parses_with_deny_unknown_fields() {
        // The shipped soc2-v1 document must carry no extra keys so the default
        // catalog keeps loading under `deny_unknown_fields`.
        let catalog = load_default_catalog();
        assert_eq!(catalog.catalog_id, "soc2-v1");
    }

    #[test]
    fn canonical_bytes_are_deterministic_and_order_independent() {
        let catalog = default();
        let baseline = canonical_bytes(&catalog);
        for _ in 0..5 {
            assert_eq!(
                canonical_bytes(&catalog),
                baseline,
                "must be byte-identical"
            );
        }

        // A catalog with shuffled controls and shuffled classes canonicalizes
        // identically.
        let shuffled_json = r#"{
            "catalog_id": "shuffle",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "B", "title": "b", "evidence_classes": [
                    { "class": "reviews", "requirement": "required" },
                    { "class": "commits", "requirement": "optional" }
                ] },
                { "control_id": "A", "title": "a", "evidence_classes": [
                    { "class": "pull_requests", "requirement": "optional" },
                    { "class": "commits", "requirement": "required" }
                ] }
            ]
        }"#;
        let ordered_json = r#"{
            "catalog_id": "shuffle",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "A", "title": "a", "evidence_classes": [
                    { "class": "commits", "requirement": "required" },
                    { "class": "pull_requests", "requirement": "optional" }
                ] },
                { "control_id": "B", "title": "b", "evidence_classes": [
                    { "class": "commits", "requirement": "optional" },
                    { "class": "reviews", "requirement": "required" }
                ] }
            ]
        }"#;
        let shuffled = parse_catalog(shuffled_json).expect("parses");
        let ordered = parse_catalog(ordered_json).expect("parses");
        assert_eq!(canonical_bytes(&shuffled), canonical_bytes(&ordered));
        assert_eq!(catalog_hash(&shuffled), catalog_hash(&ordered));
    }

    #[test]
    fn catalog_hash_is_stable_and_prefixed() {
        let catalog = default();
        let hash = catalog_hash(&catalog);
        assert!(
            hash.starts_with("control_catalog:v1:"),
            "unexpected handle: {hash}"
        );
        for _ in 0..5 {
            assert_eq!(catalog_hash(&catalog), hash);
        }
        let pinned = pin(&catalog);
        assert_eq!(pinned.catalog_hash, hash);
        assert_eq!(pinned.catalog_id, "soc2-v1");
        assert_eq!(pinned.catalog_schema_version.version, 1);
    }

    #[test]
    fn default_catalog_hash_is_pinned() {
        // The shipped soc2-v1.json (which has no duplicate controls or classes)
        // must keep hashing to this exact handle. A change here signals either a
        // catalog-content change or a canonicalization regression.
        let catalog = default();
        assert_eq!(
            catalog_hash(&catalog),
            "control_catalog:v1:fb6792a51b5db445392dfe8bcc3e68998299a3f4d975fba424bb01494262016a"
        );
    }

    #[test]
    fn canonical_bytes_order_independent_for_valid_shuffle_without_duplicates() {
        // The scenario Codex described (duplicate {class, requirement} entries
        // within one control differing only in order) can no longer be
        // constructed — parsing rejects it — so order-independence is proven
        // instead over a valid catalog whose classes and controls are shuffled.
        let shuffled = r#"{
            "catalog_id": "s",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "Z", "title": "z", "evidence_classes": [
                    { "class": "reviews", "requirement": "optional" },
                    { "class": "commits", "requirement": "required" },
                    { "class": "pull_requests", "requirement": "required" }
                ] },
                { "control_id": "A", "title": "a", "evidence_classes": [
                    { "class": "validation_runs", "requirement": "optional" },
                    { "class": "commits", "requirement": "optional" }
                ] }
            ]
        }"#;
        let ordered = r#"{
            "catalog_id": "s",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "A", "title": "a", "evidence_classes": [
                    { "class": "commits", "requirement": "optional" },
                    { "class": "validation_runs", "requirement": "optional" }
                ] },
                { "control_id": "Z", "title": "z", "evidence_classes": [
                    { "class": "commits", "requirement": "required" },
                    { "class": "pull_requests", "requirement": "required" },
                    { "class": "reviews", "requirement": "optional" }
                ] }
            ]
        }"#;
        let a = parse_catalog(shuffled).expect("parses");
        let b = parse_catalog(ordered).expect("parses");
        assert_eq!(canonical_bytes(&a), canonical_bytes(&b));
        assert_eq!(catalog_hash(&a), catalog_hash(&b));
    }

    #[test]
    fn catalog_hash_is_stable_across_line_endings() {
        // A catalog checked out with CRLF line endings (e.g. on Windows or via a
        // core.autocrlf clone) must pin to the same hash as its LF form, so the
        // control_catalog:v1:<hex> handle is stable across platforms.
        let lf_json = "{\n\
            \"catalog_id\": \"le\",\n\
            \"schema_version\": { \"domain\": \"control_catalog\", \"kind\": \"ControlCatalog\", \"version\": 1 },\n\
            \"controls\": [\n\
                { \"control_id\": \"CC1.1\", \"title\": \"t\", \"evidence_classes\": [\n\
                    { \"class\": \"commits\", \"requirement\": \"required\" },\n\
                    { \"class\": \"reviews\", \"requirement\": \"optional\" }\n\
                ] }\n\
            ]\n\
        }";
        let crlf_json = lf_json.replace('\n', "\r\n");
        assert!(
            crlf_json.contains("\r\n"),
            "CRLF variant must differ in bytes"
        );

        let lf = parse_catalog(lf_json).expect("LF catalog parses");
        let crlf = parse_catalog(&crlf_json).expect("CRLF catalog parses");

        assert_eq!(catalog_hash(&lf), catalog_hash(&crlf));
        assert_eq!(pin(&lf).catalog_hash, pin(&crlf).catalog_hash);
    }

    #[test]
    fn three_way_requirement_semantics() {
        // required + present => Pass (gate pass)
        let pass = evaluate_requirement(Requirement::Required, Availability::Present);
        assert_eq!(pass, ClassOutcome::Pass);
        assert!(pass.is_gate_pass());

        // required + unavailable => GateFail (not gate pass)
        let fail = evaluate_requirement(Requirement::Required, Availability::Unavailable);
        assert_eq!(fail, ClassOutcome::GateFail);
        assert!(!fail.is_gate_pass());

        // optional + unavailable => ReportedOptionalUnavailable (gate pass)
        let reported = evaluate_requirement(Requirement::Optional, Availability::Unavailable);
        assert_eq!(reported, ClassOutcome::ReportedOptionalUnavailable);
        assert!(reported.is_gate_pass());

        // optional + present => Pass
        assert_eq!(
            evaluate_requirement(Requirement::Optional, Availability::Present),
            ClassOutcome::Pass
        );
    }

    #[test]
    fn toy_catalog_three_way_over_one_control_two_classes() {
        let json = r#"{
            "catalog_id": "toy",
            "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
            "controls": [
                { "control_id": "T1", "title": "toy", "evidence_classes": [
                    { "class": "commits", "requirement": "required" },
                    { "class": "reviews", "requirement": "optional" }
                ] }
            ]
        }"#;
        let catalog = parse_catalog(json).expect("toy parses");
        let control = &catalog.controls[0];
        // required class present -> pass; required class absent -> gate fail;
        // optional class absent -> reported + pass.
        let required = &control.evidence_classes[0];
        assert_eq!(required.class, EvidenceClass::Commits);
        assert_eq!(
            evaluate_requirement(required.requirement, Availability::Present),
            ClassOutcome::Pass
        );
        assert_eq!(
            evaluate_requirement(required.requirement, Availability::Unavailable),
            ClassOutcome::GateFail
        );
        let optional = &control.evidence_classes[1];
        assert_eq!(optional.class, EvidenceClass::Reviews);
        assert_eq!(
            evaluate_requirement(optional.requirement, Availability::Unavailable),
            ClassOutcome::ReportedOptionalUnavailable
        );
    }
}
