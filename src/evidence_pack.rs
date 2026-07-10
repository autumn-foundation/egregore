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
    Json {
        /// Parser error message.
        message: String,
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
        }
    }

    /// Builds a redaction-safe JSON envelope for this error.
    ///
    /// The `unknown_schema_version` shape matches the repo-wide reader contract
    /// (`{"code":..,"version":{"domain","kind","version"}}`).
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Json { message } => serde_json::json!({
                "code": self.code(),
                "message": message,
            }),
            Self::UnknownSchemaVersion {
                domain,
                kind,
                version,
            } => serde_json::json!({
                "code": self.code(),
                "version": { "domain": domain, "kind": kind, "version": version },
            }),
            Self::UnknownEvidenceClass { control_id, class } => serde_json::json!({
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
#[derive(Debug, Deserialize)]
struct RawCatalog {
    catalog_id: String,
    schema_version: CatalogSchemaVersion,
    controls: Vec<RawControl>,
}

#[derive(Debug, Deserialize)]
struct RawControl {
    control_id: String,
    title: String,
    evidence_classes: Vec<RawClassRequirement>,
}

#[derive(Debug, Deserialize)]
struct RawClassRequirement {
    class: String,
    requirement: String,
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
/// and [`CatalogError::InvalidRequirement`] for a requirement outside
/// `{required, optional}`.
pub fn parse_catalog(text: &str) -> Result<ControlCatalog, CatalogError> {
    let normalized = text.replace("\r\n", "\n");
    let raw: RawCatalog =
        serde_json::from_str(&normalized).map_err(|error| CatalogError::Json {
            message: error.to_string(),
        })?;

    if !is_known_control_catalog_schema_version(
        &raw.schema_version.domain,
        &raw.schema_version.kind,
        raw.schema_version.version,
    ) {
        return Err(CatalogError::UnknownSchemaVersion {
            domain: raw.schema_version.domain,
            kind: raw.schema_version.kind,
            version: raw.schema_version.version,
        });
    }

    let mut controls = Vec::with_capacity(raw.controls.len());
    for raw_control in raw.controls {
        let mut evidence_classes = Vec::with_capacity(raw_control.evidence_classes.len());
        for raw_class in raw_control.evidence_classes {
            let class = EvidenceClass::from_wire(&raw_class.class).ok_or_else(|| {
                CatalogError::UnknownEvidenceClass {
                    control_id: raw_control.control_id.clone(),
                    class: raw_class.class.clone(),
                }
            })?;
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
        schema_version: raw.schema_version,
        controls,
    })
}

/// Canonical form for hashing: fixed field order, controls sorted by
/// `control_id`, each control's classes sorted by class wire name.
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
/// and each control's evidence classes are sorted by class wire name. The output
/// is byte-identical across runs and independent of the input's control/class
/// ordering.
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
            classes.sort_by(|a, b| a.class.cmp(b.class));
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
    fn malformed_json_is_rejected() {
        let err = parse_catalog("{ not valid json").expect_err("malformed json must fail");
        assert_eq!(err.code(), "malformed_json");
        assert!(matches!(err, CatalogError::Json { .. }));
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
