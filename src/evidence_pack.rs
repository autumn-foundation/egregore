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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

// ===========================================================================
// Issue #338 — control-scoped, time-windowed evidence-pack assembly + verify
// ===========================================================================
//
// SPEC. `assemble_pack` composes existing contracts into a deterministic,
// redaction-safe evidence pack scoped to one control (#337 catalog) and one
// half-open valid-time window (`from <= t < to`):
//
//   * Records are selected from a caller-supplied slice (loaded via
//     `read_record_line` upstream, so unknown schema versions never reach us).
//   * Each catalog class the control maps becomes a *section*, always present
//     even when empty. A class is *available* when the full record set holds at
//     least one record of that class (capability), independent of the window;
//     `review_coverage` is available whenever any pull-request record exists.
//     `evaluate_requirement` (#337) yields the three-way outcome:
//       (required, unavailable)  => GateFail  (`required_class_unavailable`)
//       (optional, unavailable)  => degraded  (`evidence_class_unavailable`)
//       (_,         available)   => Pass       (section may be explicitly empty)
//   * In-window records are scrubbed-to-hash (`bundle::scrub_record` + BLAKE3),
//     ordered by `(valid_time, record_id)`; author_email is always redacted
//     (#116). Output is allow-list only — IDs, handles, hashes, bounded labels,
//     valid times, counts — never raw bodies/hunks/payloads.
//   * Per-record valid time resolves `temporal.valid_time -> node valid_time ->
//     executed_at`; a class-relevant record with no resolvable valid time is
//     excluded under a counted `missing_valid_time` diagnostic + gap.
//   * Citation gates reuse #65 exactly (`classify_record_external`): >=95% of
//     code (source_fact) rows carry a record ID + file/span-or-commit handle
//     (or a documented absent-handle rule), and 100% of non-code rows carry a
//     source/evidence/protected handle.
//   * `gaps` are a closed five-class enum; three are fully derived here
//     (`merged_pr_without_approving_review`, `commit_outside_any_pr`,
//     `missing_valid_time`) and two (`review_unanchored_no_commit_sha`,
//     `approval_precedes_final_head`) require issue #334 facts that are not yet
//     merged — until #334 lands they always degrade to a single unconditional
//     `capability_unavailable` diagnostic naming #334 with zero rows (only for
//     controls that require review evidence), never a clean-looking check.
//   * The manifest echoes the #337 catalog pin, the window, and the verbatim
//     disclaimer. Everything is byte-identical across runs; no wall clock is
//     read unless the caller pins `--captured-at`.
//
// `verify_pack` re-checks an assembled pack offline: Integrity (recompute the
// per-record BLAKE3 over the scrubbed record + canonical `(valid_time, id)`
// order), Coverage (the same citation thresholds), Safety (no raw sensitive
// classes via `redaction::detect_secret`, scrubbed prose/handle fields None),
// and Window-consistency (every row's resolved valid time inside the window).

use crate::bundle::{BundleRecord, VerificationVerdict, scrub_record};
use crate::citation_audit::{CitationStatus, citation_trust_class, classify_record_external};
use crate::ir::{GraphRecord, TemporalMetadata};
use std::collections::{BTreeMap, BTreeSet};

/// Verbatim disclaimer carried in every assembled pack manifest (AC10).
pub const PACK_DISCLAIMER: &str = "rows are recorded observations of process execution as imported; never proof of control effectiveness, compliance, or completeness; absence of a record means no imported evidence, not no event; not an auditor opinion";

/// Section-level disclaimer for delta classes, propagated verbatim (#118/#157).
pub const DELTA_SECTION_DISCLAIMER: &str = "rows are observed deltas, never proof of behavior change; absence of a delta is not proof of stability";

/// The closed set of gap classes an evidence pack reports (AC5).
///
/// Two variants (`ReviewUnanchoredNoCommitSha`, `ApprovalPrecedesFinalHead`)
/// require issue #334 facts that are not yet merged; they are kept in the closed
/// enum and populate automatically once those facts appear.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GapClass {
    /// A merged pull request with no linked approving review.
    MergedPrWithoutApprovingReview,
    /// A recorded approval whose commit predates the PR's final head (needs #334).
    ApprovalPrecedesFinalHead,
    /// A review with no anchoring reviewed-commit SHA (needs #334).
    ReviewUnanchoredNoCommitSha,
    /// A commit not claimed by any pull request via `MERGED_AS`.
    CommitOutsideAnyPr,
    /// A class-relevant record with no resolvable valid time.
    MissingValidTime,
}

impl GapClass {
    /// Every gap class, in fixed order.
    pub const ALL: [Self; 5] = [
        Self::MergedPrWithoutApprovingReview,
        Self::ApprovalPrecedesFinalHead,
        Self::ReviewUnanchoredNoCommitSha,
        Self::CommitOutsideAnyPr,
        Self::MissingValidTime,
    ];

    /// Stable `snake_case` wire name.
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::MergedPrWithoutApprovingReview => "merged_pr_without_approving_review",
            Self::ApprovalPrecedesFinalHead => "approval_precedes_final_head",
            Self::ReviewUnanchoredNoCommitSha => "review_unanchored_no_commit_sha",
            Self::CommitOutsideAnyPr => "commit_outside_any_pr",
            Self::MissingValidTime => "missing_valid_time",
        }
    }

    /// Whether deriving this gap class needs issue #334 facts (not yet merged).
    #[must_use]
    pub const fn needs_issue_334(&self) -> bool {
        matches!(
            self,
            Self::ApprovalPrecedesFinalHead | Self::ReviewUnanchoredNoCommitSha
        )
    }
}

/// A half-open valid-time window `from <= t < to` (RFC 3339 bounds).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Inclusive lower bound (RFC 3339).
    pub from: String,
    /// Exclusive upper bound (RFC 3339).
    pub to: String,
}

/// Usage/load errors from `assemble_pack` (all map to CLI exit 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackBuildError {
    /// The requested control is not in the catalog.
    UnknownControl {
        /// The requested control ID.
        control_id: String,
        /// The catalog's known control IDs, sorted.
        known: Vec<String>,
    },
    /// `--from >= --to` — the window is empty or inverted.
    ReversedWindow {
        /// The `from` bound as supplied.
        from: String,
        /// The `to` bound as supplied.
        to: String,
    },
    /// A window bound was not a parseable RFC 3339 timestamp.
    InvalidTimestamp {
        /// Which bound failed (`from` or `to`).
        which: &'static str,
        /// The offending value.
        value: String,
    },
}

impl PackBuildError {
    /// Stable machine-readable code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnknownControl { .. } => "unknown_control",
            Self::ReversedWindow { .. } => "reversed_window",
            Self::InvalidTimestamp { .. } => "invalid_timestamp",
        }
    }

    /// Redaction-safe JSON envelope.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Self::UnknownControl { control_id, known } => serde_json::json!({
                "code": self.code(),
                "control_id": control_id,
                "known_controls": known,
            }),
            Self::ReversedWindow { from, to } => serde_json::json!({
                "code": self.code(),
                "from": from,
                "to": to,
            }),
            Self::InvalidTimestamp { which, value } => serde_json::json!({
                "code": self.code(),
                "which": which,
                "value": value,
            }),
        }
    }
}

impl std::fmt::Display for PackBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

impl std::error::Error for PackBuildError {}

/// A stable, redaction-safe pack diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Evidence class the diagnostic concerns, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_class: Option<String>,
    /// Stable unavailable reason, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Record IDs the diagnostic derives from, sorted.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub record_ids: Vec<String>,
    /// Human-readable, redaction-safe detail.
    pub detail: String,
}

/// One derived gap row, citing the record IDs it derives from (AC5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GapRow {
    /// Gap class wire name.
    pub gap_class: String,
    /// Record IDs this gap derives from, sorted.
    pub record_ids: Vec<String>,
    /// Resolved valid time of the primary cited record, when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<String>,
    /// Redaction-safe detail.
    pub detail: String,
}

/// The computed review-coverage measurement (AC6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewCoverageMeasurement {
    /// Distinct in-window merged pull requests.
    pub merged_pr_count: usize,
    /// Merged pull requests carrying an approving review.
    pub approved_pr_count: usize,
    /// Coverage fraction (`approved / merged`, vacuously 1.0 when none merged).
    pub coverage: f64,
    /// The `--min-review-coverage` threshold in effect.
    pub min_required: f64,
    /// Whether coverage met the threshold.
    pub passed: bool,
    /// IDs of merged PRs lacking an approving review, sorted.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub unapproved_pr_ids: Vec<String>,
    /// Record IDs of the coverage-substantiating `REFERENCES_TASK` edges included
    /// in the `review_coverage` section, sorted (Codex round-11 Finding C). These
    /// are the exact edges linking an included approving review to an included
    /// merged PR, so a consumer can trace `approved_pr_count` to hashed pack
    /// records rather than to a relationship the pack never carries.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub approval_link_edge_ids: Vec<String>,
}

/// One evidence-class section of an assembled pack (AC1/AC7).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceSection {
    /// Evidence-class wire name.
    pub class: String,
    /// `required` or `optional` for the anchoring control.
    pub requirement: String,
    /// `present` or `unavailable`.
    pub status: String,
    /// Three-way class outcome (`pass` / `gate_fail` / `reported_optional_unavailable`).
    pub outcome: ClassOutcome,
    /// Stable unavailable reason, when `status == "unavailable"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Count of included in-window records.
    pub record_count: usize,
    /// Included scrubbed records, each with its BLAKE3 hash, canonically ordered.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub records: Vec<BundleRecord>,
    /// Computed review-coverage measurement (only the `review_coverage` section).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measurement: Option<ReviewCoverageMeasurement>,
    /// Verbatim section-level disclaimer, when the class carries one (#118/#157).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disclaimer: Option<String>,
}

/// The assembled pack's manifest (AC7/AC10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackManifest {
    /// Anchoring control ID.
    pub control_id: String,
    /// Anchoring control title.
    pub control_title: String,
    /// The valid-time window.
    pub window: Window,
    /// Catalog identity + hash pin (#337).
    pub catalog_pin: CatalogPin,
    /// Egregore version that assembled the pack.
    pub egregore_version: String,
    /// Optional pinned capture time (never wall-clock unless the caller pins it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub captured_at: Option<String>,
    /// Per-trust-class counts of included records.
    pub included_record_counts: BTreeMap<String, usize>,
    /// Distinct `(kind, schema_version)` tuple counts of included records (AC8).
    pub tuple_counts: BTreeMap<String, usize>,
    /// Count of class-relevant records excluded for missing valid time.
    pub excluded_missing_valid_time: usize,
    /// The verbatim always-present disclaimer.
    pub disclaimer: String,
}

/// Per-class citation tally at pack level (AC4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClassCitationTally {
    /// Trust class.
    pub trust_class: String,
    /// Total rows.
    pub total: usize,
    /// Rows carrying a required handle (cited or documented-absent).
    pub cited: usize,
    /// Rows missing a required handle.
    pub missing: usize,
    /// Rows excluded as protected/unverified (reported, never counted cited).
    pub excluded: usize,
}

/// The review-coverage gate verdict (Codex round-8 P2 Finding 1).
///
/// Review coverage is only a meaningful gate for a control that requires review
/// evidence (`reviews` or `review_coverage`). For any other control (e.g. the
/// CC7.2/CC7.3 monitoring controls) the verdict is reported as a neutral
/// `not_applicable` status that never contributes to the pack `ok`, so a
/// monitoring pack assembled over a shared store that happens to contain an
/// unapproved in-window merged PR never fails on that unrelated review coverage.
/// The applicability predicate reuses the same control-scoping `control_requires`
/// logic that gates the review gap classes — never a hardcoded control-id list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCoverageVerdict {
    /// Closed status: `gating` for a review-requiring control, `not_applicable`
    /// otherwise.
    pub status: String,
    /// True when the verdict participates in the pack `ok`. A `not_applicable`
    /// verdict is never gating.
    pub applicable: bool,
    /// Whether coverage met the threshold. Vacuously `true` — and therefore never
    /// failing the gate — for a `not_applicable` verdict.
    pub passed: bool,
    /// Machine-readable reason when `not_applicable`; absent when gating.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub not_applicable_reason: Option<String>,
    /// Redaction-safe human-readable detail.
    pub detail: String,
}

/// The pack's assemble-time verdicts (AC6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackVerdicts {
    /// True only when every verdict passes.
    pub ok: bool,
    /// Every required class resolved (populated or explicitly empty).
    pub required_classes: VerificationVerdict,
    /// Citation thresholds met.
    pub citation: VerificationVerdict,
    /// Review coverage — only gating for review-requiring controls, neutral
    /// (`not_applicable`) otherwise.
    pub review_coverage: ReviewCoverageVerdict,
    /// Structural integrity (hashes + canonical order).
    pub integrity: VerificationVerdict,
    /// Safety (no raw sensitive classes; scrubbed fields None).
    pub safety: VerificationVerdict,
    /// Per-trust-class citation tallies, canonically ordered.
    pub citation_tallies: Vec<ClassCitationTally>,
}

/// A complete, self-contained, control-scoped evidence pack (AC1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidencePack {
    /// Manifest metadata.
    pub manifest: PackManifest,
    /// One section per catalog class the control maps.
    pub sections: Vec<EvidenceSection>,
    /// Derived gap rows, canonically ordered.
    pub gaps: Vec<GapRow>,
    /// Assemble-time verdicts.
    pub verdicts: PackVerdicts,
    /// Stable diagnostics, canonically ordered.
    pub diagnostics: Vec<PackDiagnostic>,
}

/// Parses an RFC 3339 timestamp into a comparable instant.
fn parse_rfc3339(value: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(value).ok()
}

/// Resolves a record's valid time in the fixed order
/// `temporal.valid_time -> node valid_time -> executed_at` (AC2).
#[must_use]
pub fn resolve_valid_time(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node {
            temporal,
            valid_time,
            executed_at,
            ..
        } => {
            if let Some(t) = temporal
                && !t.valid_time.is_empty()
            {
                return Some(t.valid_time.clone());
            }
            if let Some(vt) = valid_time
                && !vt.is_empty()
            {
                return Some(vt.clone());
            }
            executed_at.clone().filter(|s| !s.is_empty())
        }
        GraphRecord::Edge { temporal, .. } => temporal
            .as_ref()
            .map(|t| t.valid_time.clone())
            .filter(|s| !s.is_empty()),
        GraphRecord::Tombstone { .. } => None,
    }
}

/// True when a record has no window-resolvable valid time: either no resolved
/// valid time at all, or a resolved value that is not parseable RFC3339. A
/// malformed timestamp is unresolved (routed to `missing_valid_time`), NOT
/// merely out-of-window (Codex round-13 Finding 2), so this predicate is the
/// single source of truth for both the section-windowing exclusion count and the
/// `missing_valid_time` gap derivation.
fn valid_time_unresolved(record: &GraphRecord) -> bool {
    resolve_valid_time(record).is_none_or(|vt| parse_rfc3339(&vt).is_none())
}

/// Returns true when `valid_time` falls in the half-open window `from <= t < to`.
fn in_window(valid_time: &str, window: &Window) -> bool {
    let (Some(t), Some(from), Some(to)) = (
        parse_rfc3339(valid_time),
        parse_rfc3339(&window.from),
        parse_rfc3339(&window.to),
    ) else {
        return false;
    };
    from <= t && t < to
}

/// Genuine code-review `review_kind` values that count as `Reviews`-class
/// evidence.
///
/// GitHub imports emit exactly three `Review` kinds (`src/github/records.rs`):
/// `issue_comment` (a comment on an issue or PR *conversation* — discussion, not
/// a review), `pr_review` (a submitted pull-request review), and
/// `pr_review_comment` (an inline PR review-thread comment). Only the latter two
/// are genuine code-review evidence. This is an allow-list, not a deny-list of
/// `issue_comment`, so any future non-review `Review` kind (or a Review with no
/// recorded kind) is excluded until it is deliberately added here.
const GENUINE_PR_REVIEW_KINDS: [&str; 2] = ["pr_review", "pr_review_comment"];

/// True when a `Review` record is a genuine PR review (by `review_kind`
/// allow-list), not a GitHub issue comment.
#[must_use]
fn is_genuine_pr_review_kind(review_kind: Option<&str>) -> bool {
    review_kind.is_some_and(|k| GENUINE_PR_REVIEW_KINDS.contains(&k))
}

/// Maps a record to its catalog evidence class, when it maps to one.
#[must_use]
pub fn evidence_class_for_record(record: &GraphRecord) -> Option<EvidenceClass> {
    let GraphRecord::Node {
        kind,
        source_kind,
        review_kind,
        ..
    } = record
    else {
        return None;
    };
    match kind.as_str() {
        "Commit" => Some(EvidenceClass::Commits),
        "PR" => Some(EvidenceClass::PullRequests),
        "Task" if source_kind.as_deref() == Some("github_pr") => Some(EvidenceClass::PullRequests),
        // Only genuine PR reviews are review evidence. An `issue_comment`-kind
        // Review (GitHub issue/PR-conversation discussion) — or any Review with
        // no recorded kind — is NOT review evidence (Codex round-4 P2).
        "Review" if is_genuine_pr_review_kind(review_kind.as_deref()) => {
            Some(EvidenceClass::Reviews)
        }
        // Per-file structural deltas: `scan-history` emits one `Change` node per
        // file touched in a commit (`src/history.rs`), each carrying commit valid
        // time. These are the genuine stored backing for structural deltas.
        "Change" => Some(EvidenceClass::StructuralDeltas),
        "Verification" | "CommandRun" | "TestRun" | "CIStatus" | "CommandEvidence"
        | "BenchmarkRun" | "CoverageReport" | "ProofResult" => {
            Some(EvidenceClass::VerificationEvidence)
        }
        "ErrorSignature" => Some(EvidenceClass::ErrorSignatures),
        "LogOccurrenceBucket" => Some(EvidenceClass::OccurrenceBuckets),
        _ => None,
    }
}

/// Whether `record` is an expected row of the `review_coverage` section.
///
/// The section is not class-scoped: its only rows are the substantiating
/// `REFERENCES_TASK` link edges built by `assemble_pack` (the coverage
/// measurement itself rides the section's `measurement` field, never a row).
/// `verify_pack` uses this to BOUND the section's membership exemption (Codex
/// round-15 Finding 1) so no other hashed row can be smuggled in and presented
/// as coverage evidence.
#[must_use]
fn is_expected_review_coverage_row(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Edge { label, .. } if label.as_str() == "REFERENCES_TASK"
    )
}

/// Stable unavailable reason for a class that is not available.
///
/// Three honest families:
/// * `*_domain_absent` — the class has a real stored backing node kind, but no
///   record of that kind exists in the input (`Change` for structural deltas,
///   `Commit`/`Task`/`Review`/verification kinds for the rest).
/// * `derived_class_not_materialized` — the class is a computed/derived surface
///   (`eg query public-api-deltas` #157, `eg validate` #103) with no stored node
///   kind, so it can never be materialized as pack evidence records. Reported
///   honestly rather than mislabeled as a missing domain.
/// * `log_domain_absent` — the log-signature classes (issues #319/#340) whose
///   backing domain is not yet emitted.
#[must_use]
const fn unavailable_reason(class: EvidenceClass) -> &'static str {
    match class {
        EvidenceClass::Commits => "commit_domain_absent",
        EvidenceClass::PullRequests => "pull_request_domain_absent",
        EvidenceClass::Reviews => "review_domain_absent",
        EvidenceClass::ReviewCoverage => "no_pull_requests_to_measure",
        EvidenceClass::StructuralDeltas => "delta_domain_absent",
        EvidenceClass::PublicApiDeltas | EvidenceClass::ValidationRuns => {
            "derived_class_not_materialized"
        }
        EvidenceClass::VerificationEvidence => "verification_domain_absent",
        EvidenceClass::ErrorSignatures
        | EvidenceClass::OccurrenceBuckets
        | EvidenceClass::RemediationLinks => "log_domain_absent",
    }
}

/// The MERGE time of a merged GitHub PR task, when the record is a `github_pr`
/// `Task` carrying an explicit `merged_at` (promoted first-class in #333).
///
/// This is deliberately NOT `resolve_valid_time`: the GitHub importer stamps a PR
/// Task's `valid_time` from `github_updated_at` (`src/github/records.rs`), i.e.
/// the PR's LAST-UPDATE time, which routinely differs from its merge time. The
/// merged-in-window determination for review coverage and the
/// `merged_pr_without_approving_review` gap must window on merge time, so it keys
/// on `merged_at` (Codex round-5 P1). A merged PR with no resolvable `merged_at`
/// (e.g. only a `merge_commit_sha`) has no reliable merge time and is therefore
/// not windowable as merged — it is excluded, never falling back to update time.
fn merged_pr_merge_time(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            merged_at: Some(m),
            source_kind,
            ..
        } if source_kind.as_deref() == Some("github_pr") && !m.is_empty() => Some(m.as_str()),
        _ => None,
    }
}

/// True when the record is a genuine approving pull-request review.
///
/// Only a genuine PR review (`review_kind` allow-list) with an `approved`
/// `review_state` counts. An `issue_comment`-kind Review never approves, even if
/// something forced an `approved` state onto it (Codex round-4 P2). In practice a
/// GitHub issue comment carries no `review_state` at all, so this is a
/// defense-in-depth guard consistent with the classifier's allow-list.
fn is_approving_review(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node {
            kind,
            review_kind,
            review_state: Some(state),
            ..
        } if kind.as_str() == "Review"
            && state == "approved"
            && is_genuine_pr_review_kind(review_kind.as_deref())
    )
}

/// Stamps a coverage-substantiating link edge with the valid time of its
/// approving review (Codex round-11 Finding C).
///
/// A `REFERENCES_TASK` edge carries no intrinsic valid time, so without a stamp
/// it could not be a window-consistent, canonically-ordered pack record. The
/// approval relationship becomes valid the moment the approving review is
/// submitted, so its valid time is the review's own — an in-window instant that
/// is genuine, not fabricated. The stamped edge then flows through the ordinary
/// scrub/hash/section pipeline like any other record, passing `verify_pack`'s
/// window-consistency, integrity, and manifest-count checks with no special case.
fn stamp_edge_valid_time(mut edge: GraphRecord, valid_time: &str) -> GraphRecord {
    if let GraphRecord::Edge { temporal, .. } = &mut edge {
        *temporal = Some(TemporalMetadata {
            git_commit: String::new(),
            git_parent_commits: Vec::new(),
            valid_time: valid_time.to_owned(),
            author_time: None,
            observed_at: valid_time.to_owned(),
            valid_time_source: Some("approving_review_valid_time".to_owned()),
        });
    }
    edge
}

/// Scrubs, hashes, and canonically orders a set of records for a section.
fn build_section_records(records: Vec<GraphRecord>) -> Vec<BundleRecord> {
    let mut rows: Vec<BundleRecord> = records
        .into_iter()
        .map(|r| {
            let scrubbed = scrub_record(r);
            let json = serde_json::to_string(&scrubbed).unwrap_or_default();
            let hash = blake3::hash(json.as_bytes()).to_string();
            BundleRecord {
                record: scrubbed,
                hash,
            }
        })
        .collect();
    rows.sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
    rows
}

/// Canonical `(valid_time_or_empty, record_id)` sort key for a section row.
fn section_sort_key(record: &GraphRecord) -> (String, String) {
    (
        resolve_valid_time(record).unwrap_or_default(),
        record.id().to_owned(),
    )
}

/// The trust-class citation view of a set of section rows, reused for both the
/// assemble-time citation verdict and `verify_pack`'s coverage check (AC4).
fn citation_view(rows: &[&BundleRecord]) -> (Vec<ClassCitationTally>, bool, bool) {
    // (tallies, code_gate_pass, non_code_gate_pass)
    // per_class entry: (total, cited, missing, excluded)
    let mut per_class: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    let mut code_total = 0usize;
    let mut code_cited = 0usize;
    let mut non_code_ok = true;
    for br in rows {
        let classified = classify_record_external(&br.record);
        let trust = classified.trust_class.to_owned();
        // A row satisfies the citation contract only when it carries the handle
        // its trust class requires. Mirror `citation_audit`'s exact satisfying
        // set (`Cited | AbsentHandleDocumented`) so the pack's per-class tallies
        // are byte-identical to `eg audit citations` on the same records. A
        // protected/unverified exclusion (`ExcludedProtected`/`ExcludedUnverified`)
        // is NOT satisfying — it is tallied separately, never counted as cited.
        let satisfied = matches!(
            classified.status,
            CitationStatus::Cited | CitationStatus::AbsentHandleDocumented
        );
        let missing = classified.status == CitationStatus::MissingRequiredHandle;
        let entry = per_class.entry(trust.clone()).or_insert((0, 0, 0, 0));
        entry.0 += 1;
        if satisfied {
            entry.1 += 1;
        } else if missing {
            entry.2 += 1;
        } else {
            entry.3 += 1;
        }
        if classified.trust_class == "source_fact" {
            code_total += 1;
            if satisfied {
                code_cited += 1;
            }
        } else if missing {
            // The non-code (100%) gate fails only on a genuinely missing handle,
            // exactly as `citation_audit` gates it. Protected/unverified
            // exclusions are reported (tallied), never a gate failure.
            non_code_ok = false;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let code_pass = if code_total == 0 {
        true
    } else {
        (code_cited as f64 / code_total as f64) >= 0.95
    };
    let tallies = per_class
        .into_iter()
        .map(
            |(trust_class, (total, cited, missing, excluded))| ClassCitationTally {
                trust_class,
                total,
                cited,
                missing,
                excluded,
            },
        )
        .collect();
    (tallies, code_pass, non_code_ok)
}

/// Assembles a control-scoped, time-windowed evidence pack (AC1-AC10).
///
/// Pure: no I/O, no printing, no process exit. Deterministic and byte-identical
/// across runs; reads no wall clock unless `captured_at` is supplied.
///
/// # Errors
///
/// Returns [`PackBuildError::UnknownControl`] when `control_id` is not in the
/// catalog (naming the known IDs), [`PackBuildError::InvalidTimestamp`] when a
/// window bound is not RFC 3339, and [`PackBuildError::ReversedWindow`] when
/// `from >= to`.
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn assemble_pack(
    records: &[GraphRecord],
    catalog: &ControlCatalog,
    control_id: &str,
    window: &Window,
    min_review_coverage: f64,
    egregore_version: &str,
    captured_at: Option<&str>,
) -> Result<EvidencePack, PackBuildError> {
    // --- validate window bounds ---
    let Some(from_ts) = parse_rfc3339(&window.from) else {
        return Err(PackBuildError::InvalidTimestamp {
            which: "from",
            value: window.from.clone(),
        });
    };
    let Some(to_ts) = parse_rfc3339(&window.to) else {
        return Err(PackBuildError::InvalidTimestamp {
            which: "to",
            value: window.to.clone(),
        });
    };
    if from_ts >= to_ts {
        return Err(PackBuildError::ReversedWindow {
            from: window.from.clone(),
            to: window.to.clone(),
        });
    }

    // --- resolve control ---
    let Some(control) = catalog.controls.iter().find(|c| c.control_id == control_id) else {
        let mut known: Vec<String> = catalog
            .controls
            .iter()
            .map(|c| c.control_id.clone())
            .collect();
        known.sort();
        return Err(PackBuildError::UnknownControl {
            control_id: control_id.to_owned(),
            known,
        });
    };

    let mut diagnostics: Vec<PackDiagnostic> = Vec::new();

    // --- capability availability (whole record set) ---
    let mut any_pr = false;
    let mut present_classes: BTreeSet<&'static str> = BTreeSet::new();
    for record in records {
        if let Some(class) = evidence_class_for_record(record) {
            present_classes.insert(class.as_wire());
            if class == EvidenceClass::PullRequests {
                any_pr = true;
            }
        }
    }
    let class_available = |class: EvidenceClass| -> Availability {
        let present = if class == EvidenceClass::ReviewCoverage {
            any_pr
        } else {
            present_classes.contains(class.as_wire())
        };
        if present {
            Availability::Present
        } else {
            Availability::Unavailable
        }
    };

    // --- per-class in-window records (with missing-valid-time exclusion) ---
    let mut excluded_missing_valid_time = 0usize;
    let mut in_window_by_class: BTreeMap<&'static str, Vec<GraphRecord>> = BTreeMap::new();
    for record in records {
        let Some(class) = evidence_class_for_record(record) else {
            continue;
        };
        // Parse BEFORE deciding in/out of window: a resolved-but-malformed
        // (non-RFC3339) valid time is unresolved, NOT merely out-of-window, and
        // must route to the same `missing_valid_time` path as a truly-absent time
        // — never silently excluded (Codex round-13 Finding 2). `parse_rfc3339`
        // returns `None` for both an absent resolved value and a malformed one.
        match resolve_valid_time(record).and_then(|vt| parse_rfc3339(&vt)) {
            Some(parsed) if from_ts <= parsed && parsed < to_ts => {
                in_window_by_class
                    .entry(class.as_wire())
                    .or_default()
                    .push(record.clone());
            }
            Some(_) => {} // parsed and out of window: excluded, no diagnostic
            None => {
                excluded_missing_valid_time += 1;
                diagnostics.push(PackDiagnostic {
                    code: "missing_valid_time".to_owned(),
                    evidence_class: Some(class.as_wire().to_owned()),
                    unavailable_reason: None,
                    record_ids: vec![record.id().to_owned()],
                    detail: "class-relevant record excluded: no resolvable valid time".to_owned(),
                });
            }
        }
    }

    // --- review coverage measurement over in-window merged PRs ---
    // A PR counts as "merged in window" iff its MERGE time (`merged_at`,
    // first-class since #333) falls in the half-open window — NOT its Task
    // `valid_time`, which the GitHub importer stamps from `github_updated_at`
    // (the PR's last-update time). Keying on the update time would drop a PR
    // merged in-window but updated after it (vacuously passing coverage and
    // suppressing the gap) and wrongly admit a PR merged before the window but
    // updated inside it (Codex round-5 P1). The section windowing of PR evidence
    // records (which keys on `valid_time`, per the general per-class loop above)
    // is a separate concern and is intentionally left unchanged.
    let mut merged_pr_ids: Vec<String> = Vec::new();
    for record in records {
        if let Some(merge_time) = merged_pr_merge_time(record)
            && in_window(merge_time, window)
        {
            merged_pr_ids.push(record.id().to_owned());
        }
    }
    merged_pr_ids.sort();
    merged_pr_ids.dedup();
    // A PR is approved when an approving Review references it via REFERENCES_TASK,
    // that review resolves inside the same half-open pack window, AND its resolved
    // valid time is AT OR BEFORE the referenced PR's merge time (`merged_at`). An
    // approval SUBMITTED AFTER the merge did not gate it — it is post-hoc and must
    // not count (Codex round-9 Finding 1). An approving review whose valid time
    // falls before `from` or at/after `to`, or that has no resolvable valid time,
    // is likewise omitted from the windowed `reviews` section, so it must not
    // count toward approval either — otherwise the pack would suppress the gap
    // while showing zero in-window approval. The at-or-before-merge check here
    // uses fields available today (`merged_at`); it is distinct from the
    // #334-degraded `approval_precedes_final_head` gap, which compares against the
    // final HEAD commit and stays capability-unavailable.
    // In one pass, collect both the approved-PR targets AND the specific edges
    // that substantiate them (Codex round-11 Finding C). An edge substantiates
    // approval when its source is an approving review resolving in-window at or
    // before the referenced PR's merge time. Only edges whose target is an
    // INCLUDED merged-in-window PR (`merged_pr_ids`) — the exact edges backing the
    // coverage count — are captured for inclusion, so the pack carries neither
    // unrelated links nor links to out-of-window PRs.
    let mut approving_targets: BTreeSet<String> = BTreeSet::new();
    let mut coverage_link_edges: Vec<GraphRecord> = Vec::new();
    // The source approving-review NODES behind the coverage link edges, deduped by
    // record ID (a single review approving several PRs sources several edges but is
    // one node). Co-located in the `review_coverage` section so `verify_pack` can
    // resolve every coverage edge's source offline even when the catalog maps no
    // `reviews` section (Codex round-18 Finding 2).
    let mut coverage_review_nodes: BTreeMap<String, GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        if label.as_str() != "REFERENCES_TASK" {
            continue;
        }
        // The referenced PR's merge time. A target with no resolvable `merged_at`
        // is not a windowable merged PR, so no approval can be gated by it.
        let Some(merged_at) = records
            .iter()
            .find(|rec| rec.id() == target)
            .and_then(merged_pr_merge_time)
            .and_then(parse_rfc3339)
        else {
            continue;
        };
        let approving_review = records.iter().find(|rec| {
            rec.id() == source
                && is_approving_review(rec)
                && resolve_valid_time(rec).is_some_and(|vt| {
                    in_window(&vt, window) && parse_rfc3339(&vt).is_some_and(|rt| rt <= merged_at)
                })
        });
        if let Some(review) = approving_review {
            approving_targets.insert(target.clone());
            if merged_pr_ids.contains(target) {
                let review_vt = resolve_valid_time(review).unwrap_or_default();
                coverage_link_edges.push(stamp_edge_valid_time(record.clone(), &review_vt));
                coverage_review_nodes
                    .entry(review.id().to_owned())
                    .or_insert_with(|| (*review).clone());
            }
        }
    }
    let unapproved_pr_ids: Vec<String> = merged_pr_ids
        .iter()
        .filter(|id| !approving_targets.contains(*id))
        .cloned()
        .collect();
    let merged_pr_count = merged_pr_ids.len();
    let approved_pr_count = merged_pr_count - unapproved_pr_ids.len();
    #[allow(clippy::cast_precision_loss)]
    let coverage = if merged_pr_count == 0 {
        1.0
    } else {
        approved_pr_count as f64 / merged_pr_count as f64
    };
    let review_coverage_passed = coverage >= min_review_coverage;

    // Scrub + hash + canonically order the substantiating link edges AND their
    // source approving-review nodes once. Together they populate the
    // `review_coverage` section (co-located with the measurement they back). The
    // review nodes make every coverage edge's source resolvable offline regardless
    // of whether the catalog maps a `reviews` section (Codex round-18 Finding 2);
    // a review that also lands in a mapped `reviews` section appears in both, which
    // the shared manifest-count recompute keeps self-consistent.
    let mut coverage_section_input = coverage_link_edges;
    coverage_section_input.extend(coverage_review_nodes.into_values());
    let coverage_link_rows = build_section_records(coverage_section_input);
    // The measurement cites ONLY the REFERENCES_TASK link edge rows — never the
    // co-located source review nodes.
    let mut approval_link_edge_ids: Vec<String> = coverage_link_rows
        .iter()
        .filter(|br| is_expected_review_coverage_row(&br.record))
        .map(|br| br.record.id().to_owned())
        .collect();
    approval_link_edge_ids.sort();
    approval_link_edge_ids.dedup();

    let review_measurement = ReviewCoverageMeasurement {
        merged_pr_count,
        approved_pr_count,
        coverage,
        min_required: min_review_coverage,
        passed: review_coverage_passed,
        unapproved_pr_ids,
        approval_link_edge_ids,
    };

    // --- build sections in catalog-class order ---
    let mut sections: Vec<EvidenceSection> = Vec::new();
    let mut all_section_rows: Vec<BundleRecord> = Vec::new();
    let mut required_unavailable: Vec<String> = Vec::new();
    for cr in &control.evidence_classes {
        let class = cr.class;
        let availability = class_available(class);
        let outcome = evaluate_requirement(cr.requirement, availability);
        let is_review_coverage = class == EvidenceClass::ReviewCoverage;
        let (status, reason) = match availability {
            Availability::Present => ("present", None),
            Availability::Unavailable => {
                ("unavailable", Some(unavailable_reason(class).to_owned()))
            }
        };
        if outcome == ClassOutcome::GateFail {
            required_unavailable.push(class.as_wire().to_owned());
            diagnostics.push(PackDiagnostic {
                code: "required_class_unavailable".to_owned(),
                evidence_class: Some(class.as_wire().to_owned()),
                unavailable_reason: Some(unavailable_reason(class).to_owned()),
                record_ids: Vec::new(),
                detail: "a required evidence class has no available records".to_owned(),
            });
        }
        if outcome == ClassOutcome::ReportedOptionalUnavailable {
            diagnostics.push(PackDiagnostic {
                code: "evidence_class_unavailable".to_owned(),
                evidence_class: Some(class.as_wire().to_owned()),
                unavailable_reason: Some(unavailable_reason(class).to_owned()),
                record_ids: Vec::new(),
                detail: "an optional evidence class degraded to unavailable".to_owned(),
            });
        }
        let records_for_class = if is_review_coverage {
            // The coverage-substantiating link edges (Codex round-11 Finding C):
            // hashed, citable records backing the coverage measurement.
            coverage_link_rows.clone()
        } else {
            in_window_by_class
                .remove(class.as_wire())
                .map(build_section_records)
                .unwrap_or_default()
        };
        for br in &records_for_class {
            all_section_rows.push(br.clone());
        }
        let disclaimer = matches!(
            class,
            EvidenceClass::StructuralDeltas | EvidenceClass::PublicApiDeltas
        )
        .then(|| DELTA_SECTION_DISCLAIMER.to_owned());
        sections.push(EvidenceSection {
            class: class.as_wire().to_owned(),
            requirement: cr.requirement.as_wire().to_owned(),
            status: status.to_owned(),
            outcome,
            unavailable_reason: reason,
            record_count: records_for_class.len(),
            records: records_for_class,
            measurement: is_review_coverage.then(|| review_measurement.clone()),
            disclaimer,
        });
    }

    // --- gaps ---
    let gaps = derive_gaps(
        records,
        control,
        window,
        &merged_pr_ids,
        &approving_targets,
        &mut diagnostics,
    );

    // --- citation verdict ---
    let row_refs: Vec<&BundleRecord> = all_section_rows.iter().collect();
    let (citation_tallies, code_pass, non_code_pass) = citation_view(&row_refs);
    let citation_ok = code_pass && non_code_pass;

    // --- integrity is structurally guaranteed at assemble time ---
    let integrity = VerificationVerdict {
        passed: true,
        detail: "records hashed over scrubbed form; sections canonically ordered".to_owned(),
    };
    // Safety scans the WHOLE assembled artifact (records AND every non-record text
    // field), identical to `verify_pack` (Codex round-11 Finding A). It cannot run
    // until the pack exists — the scan reads the pack's own fields — so a
    // placeholder holds the slot here and the real verdict replaces it after the
    // pack is constructed, below. This keeps a secret echoed into a non-record
    // field (a malicious `--catalog` control title, a diagnostic detail) from
    // being serialized while the assembled safety verdict wrongly reads `passed`.
    let safety = VerificationVerdict {
        passed: true,
        detail: String::new(),
    };

    let required_passed = required_unavailable.is_empty();
    let required_classes = VerificationVerdict {
        passed: required_passed,
        detail: if required_passed {
            "every required class resolved (populated or explicitly empty)".to_owned()
        } else {
            format!(
                "required classes unavailable: {}",
                required_unavailable.join(", ")
            )
        },
    };
    let citation = VerificationVerdict {
        passed: citation_ok,
        detail: if citation_ok {
            "code rows >=95% cited; non-code rows 100% cited".to_owned()
        } else {
            "citation thresholds not met".to_owned()
        },
    };
    // Review coverage only gates a control that requires review evidence. The
    // predicate REUSES the control-scoping `control_requires` logic that gates the
    // review gap classes in `derive_gaps` (never a hardcoded control-id list), so
    // a monitoring pack (CC7.2/CC7.3) whose control maps no review classes reports
    // a neutral `not_applicable` verdict that never fails the gate (Codex round-8
    // P2 Finding 1).
    let requires_review = control_requires(control, EvidenceClass::Reviews)
        || control_requires(control, EvidenceClass::ReviewCoverage);
    let review_coverage = if requires_review {
        ReviewCoverageVerdict {
            status: "gating".to_owned(),
            applicable: true,
            passed: review_coverage_passed,
            not_applicable_reason: None,
            detail: format!("review coverage {coverage:.4} vs minimum {min_review_coverage:.4}"),
        }
    } else {
        ReviewCoverageVerdict {
            status: "not_applicable".to_owned(),
            applicable: false,
            passed: true,
            not_applicable_reason: Some("control_does_not_require_review".to_owned()),
            detail: "review coverage not applicable: control does not require review coverage or review evidence"
                .to_owned(),
        }
    };
    // A `not_applicable` verdict is vacuously `passed`, so it never fails the gate;
    // the explicit applicability guard keeps that intent legible.
    let review_coverage_gate_ok = !review_coverage.applicable || review_coverage.passed;

    // `ok` is finalized after the whole-artifact safety scan runs on the built
    // pack (below); the non-safety gates are fixed here.
    let gates_ok_without_safety =
        required_passed && citation_ok && review_coverage_gate_ok && integrity.passed;

    let verdicts = PackVerdicts {
        // Placeholder; recomputed once safety is known.
        ok: gates_ok_without_safety,
        required_classes,
        citation,
        review_coverage,
        integrity,
        safety,
        citation_tallies,
    };

    // --- manifest counts (recomputed identically in verify_pack's Integrity
    // verdict via the shared `compute_manifest_counts`) ---
    let (included_record_counts, tuple_counts) = compute_manifest_counts(&all_section_rows);

    diagnostics.sort_by_key(diagnostic_sort_key);

    let manifest = PackManifest {
        control_id: control.control_id.clone(),
        control_title: control.title.clone(),
        window: window.clone(),
        catalog_pin: pin(catalog),
        egregore_version: egregore_version.to_owned(),
        captured_at: captured_at.map(str::to_owned),
        included_record_counts,
        tuple_counts,
        excluded_missing_valid_time,
        disclaimer: PACK_DISCLAIMER.to_owned(),
    };

    let mut pack = EvidencePack {
        manifest,
        sections,
        gaps,
        verdicts,
        diagnostics,
    };

    // --- whole-artifact safety (Codex round-11 Finding A) ---
    // Run the SAME scan `verify_pack` runs, over the fully-constructed pack, so
    // the assembled safety verdict reflects the entire serialized artifact (record
    // rows AND non-record text fields), not just the scrubbed section rows. The
    // scan is computed with the placeholder safety verdict in place; its own
    // detail is a fixed, redaction-safe area/class string that a later re-scan by
    // `verify_pack` sees identically, so assemble and verify agree.
    let (safety_passed, safety_detail) = pack_artifact_safety(&pack, &all_section_rows);
    pack.verdicts.safety = VerificationVerdict {
        passed: safety_passed,
        detail: safety_detail,
    };
    pack.verdicts.ok = gates_ok_without_safety && pack.verdicts.safety.passed;

    Ok(pack)
}

/// A distinct `(kind, schema_version)` tuple key for a record (AC8).
fn tuple_key(record: &GraphRecord) -> String {
    match record {
        GraphRecord::Node {
            kind,
            schema_version,
            ..
        } => format!("{}/v{schema_version}", kind.as_str()),
        GraphRecord::Edge {
            label,
            schema_version,
            ..
        } => format!("{}/v{schema_version}", label.as_str()),
        GraphRecord::Tombstone { schema_version, .. } => format!("Tombstone/v{schema_version}"),
    }
}

/// Recomputes the manifest's aggregate counts from the actual included section
/// rows: per-trust-class `included_record_counts` and per-`(kind,schema_version)`
/// `tuple_counts` (AC8). Shared by `assemble_pack` (the source of truth that
/// populates the manifest) and `verify_pack`'s Integrity re-check so the two can
/// never drift — a tampered pack with rows removed but the manifest aggregates
/// left stale is caught offline.
fn compute_manifest_counts<'a>(
    rows: impl IntoIterator<Item = &'a BundleRecord>,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let mut included_record_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut tuple_counts: BTreeMap<String, usize> = BTreeMap::new();
    for br in rows {
        let tc = citation_trust_class(&br.record).to_owned();
        *included_record_counts.entry(tc).or_insert(0) += 1;
        *tuple_counts.entry(tuple_key(&br.record)).or_insert(0) += 1;
    }
    (included_record_counts, tuple_counts)
}

/// Redaction-safe divergence detail: names the aggregate, the first (sorted) key
/// whose count differs, and the manifest-stored vs recomputed numbers. Keys are
/// trust-class labels or `(kind, schema_version)` tuple keys and counts are
/// integers — never a record payload.
fn manifest_count_divergence_detail(
    which: &str,
    manifest: &BTreeMap<String, usize>,
    recomputed: &BTreeMap<String, usize>,
) -> String {
    let mut keys: BTreeSet<&String> = manifest.keys().collect();
    keys.extend(recomputed.keys());
    for key in keys {
        let stored = manifest.get(key).copied().unwrap_or(0);
        let actual = recomputed.get(key).copied().unwrap_or(0);
        if stored != actual {
            return format!(
                "manifest {which} count diverges from section rows: key '{key}' stored {stored} != recomputed {actual}"
            );
        }
    }
    format!("manifest {which} count diverges from section rows")
}

fn diagnostic_sort_key(d: &PackDiagnostic) -> (String, String, String) {
    (
        d.code.clone(),
        d.evidence_class.clone().unwrap_or_default(),
        d.record_ids.first().cloned().unwrap_or_default(),
    )
}

/// Derives the closed set of gap rows (AC5). Two classes require issue #334
/// facts that are not yet merged; until #334 lands they unconditionally emit a
/// single `capability_unavailable` diagnostic (for review-requiring controls
/// only) and produce zero rows, never a clean-looking check.
/// True when the control marks `class` as [`Requirement::Required`].
///
/// The control-scoping predicate for gap derivation is derived from this over
/// the control's own `evidence_classes`, never a hardcoded control-id list.
fn control_requires(control: &Control, class: EvidenceClass) -> bool {
    control
        .evidence_classes
        .iter()
        .any(|cr| cr.class == class && cr.requirement == Requirement::Required)
}

fn derive_gaps(
    records: &[GraphRecord],
    control: &Control,
    window: &Window,
    merged_pr_ids: &[String],
    approving_targets: &BTreeSet<String>,
    diagnostics: &mut Vec<PackDiagnostic>,
) -> Vec<GapRow> {
    let mut gaps: Vec<GapRow> = Vec::new();
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // Gap classes are control-scoped: a defect over PR/commit/review evidence is
    // only meaningful for a control that actually requires that evidence class.
    // The predicate is derived from the control's own `Requirement::Required`
    // entries, never a hardcoded control-id list, so a CC7.2/CC7.3 monitoring
    // pack never emits change-management (CC8.1) PR/commit/review gaps.
    let requires_commits = control_requires(control, EvidenceClass::Commits);
    let requires_pull_requests = control_requires(control, EvidenceClass::PullRequests);
    let requires_review = control_requires(control, EvidenceClass::Reviews)
        || control_requires(control, EvidenceClass::ReviewCoverage);
    // `merged_pr_without_approving_review`: the PR/review linkage defect.
    let want_merged_pr_gap = requires_pull_requests || requires_review;
    // `commit_outside_any_pr`: the commit/PR provenance defect.
    let want_commit_outside_gap = requires_commits || requires_pull_requests;
    // The #334-degraded pair (`review_unanchored_no_commit_sha`,
    // `approval_precedes_final_head`) and its capability diagnostic.
    let want_review_anchored = requires_review;

    // merged_pr_without_approving_review
    if want_merged_pr_gap {
        for pr_id in merged_pr_ids {
            if !approving_targets.contains(pr_id) {
                // Stamp the gap with the SAME merge time (`merged_at`) used to
                // SELECT the PR as merged-in-window (round-5 `merged_pr_ids`), not
                // the Task `valid_time` (github_updated_at). A PR merged in-window
                // but updated after `to` would otherwise carry an out-of-window
                // timestamp inconsistent with its in-window selection, so a
                // downstream consumer filtering gaps by the manifest window would
                // drop or misplace it (Codex round-8 P2 Finding 2). Every id here
                // came from `merged_pr_ids`, so its merge time always resolves.
                let vt = by_id
                    .get(pr_id.as_str())
                    .and_then(|r| merged_pr_merge_time(r))
                    .map(str::to_owned);
                gaps.push(GapRow {
                    gap_class: GapClass::MergedPrWithoutApprovingReview
                        .as_wire()
                        .to_owned(),
                    record_ids: vec![pr_id.clone()],
                    valid_time: vt,
                    detail: "merged pull request has no linked approving review".to_owned(),
                });
            }
        }
    }

    // commit_outside_any_pr: in-window commit not targeted by any MERGED_AS edge
    if want_commit_outside_gap {
        let merged_commit_targets: BTreeSet<&str> = records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Edge { label, target, .. } if label.as_str() == "MERGED_AS" => {
                    Some(target.as_str())
                }
                _ => None,
            })
            .collect();
        for record in records {
            if record.node_kind_name() == Some("Commit")
                && let Some(vt) = resolve_valid_time(record)
                && in_window(&vt, window)
                && !merged_commit_targets.contains(record.id())
            {
                gaps.push(GapRow {
                    gap_class: GapClass::CommitOutsideAnyPr.as_wire().to_owned(),
                    record_ids: vec![record.id().to_owned()],
                    valid_time: Some(vt),
                    detail: "commit not claimed by any pull request via MERGED_AS".to_owned(),
                });
            }
        }
    }

    // missing_valid_time: class-relevant records with no resolvable valid time.
    // Generic and unconditional — it is about any class-relevant record excluded
    // for lacking valid time, so it applies to every control.
    for record in records {
        if evidence_class_for_record(record).is_some() && valid_time_unresolved(record) {
            gaps.push(GapRow {
                gap_class: GapClass::MissingValidTime.as_wire().to_owned(),
                record_ids: vec![record.id().to_owned()],
                valid_time: None,
                detail: "class-relevant record has no resolvable valid time".to_owned(),
            });
        }
    }

    // #334-dependent classes: only in scope when the control requires review
    // evidence. Issue #334 (the `review_commit_sha` field / `REVIEWS_COMMIT`
    // edge) is NOT merged: there is no such field or edge in the schema and no
    // derivation logic exists, so the two dependent gap classes
    // (`review_unanchored_no_commit_sha`, `approval_precedes_final_head`) cannot
    // be derived. Emit the capability diagnostic UNCONDITIONALLY here so a pack
    // never presents as if these two checks ran cleanly when they were actually
    // skipped (Codex round-7 P2: input that merely resembled the #334 facts must
    // not be mistaken for a real derivation and suppress the signal).
    //
    // WHEN #334 LANDS: replace this unconditional diagnostic with the real
    // derivation of `review_unanchored_no_commit_sha` and
    // `approval_precedes_final_head` from the reviewed-commit facts, emitting the
    // diagnostic only if those facts are genuinely unavailable in the input.
    if want_review_anchored {
        diagnostics.push(PackDiagnostic {
            code: "capability_unavailable".to_owned(),
            evidence_class: None,
            unavailable_reason: Some("issue_334_reviewed_commit_facts_absent".to_owned()),
            record_ids: Vec::new(),
            detail: "gap classes review_unanchored_no_commit_sha and \
                     approval_precedes_final_head require issue #334 reviewed-commit \
                     facts (review_commit_sha / REVIEWS_COMMIT), which are not implemented"
                .to_owned(),
        });
    }

    gaps.sort_by(|a, b| {
        (
            a.gap_class.clone(),
            a.valid_time.clone().unwrap_or_default(),
            a.record_ids.first().cloned().unwrap_or_default(),
        )
            .cmp(&(
                b.gap_class.clone(),
                b.valid_time.clone().unwrap_or_default(),
                b.record_ids.first().cloned().unwrap_or_default(),
            ))
    });
    gaps
}

/// Safety scan over the scrubbed section rows (AC3/AC9). Mirrors the bundle
/// safety contract: no detectable secret, and prose/inline-payload fields None.
fn pack_safety(rows: &[BundleRecord]) -> (bool, String) {
    for br in rows {
        let record_id = br.record.id();
        let serialized = serde_json::to_string(&br.record).unwrap_or_default();
        if let Some((class, _)) = crate::redaction::detect_secret(&serialized) {
            return (
                false,
                format!(
                    "record {record_id} contains unredacted secret class: {}",
                    class.as_str()
                ),
            );
        }
        // Assert every field `scrub_record` clears is actually None. Shared with
        // the #68 bundle verify Safety check (`first_unscrubbed_field`) so the
        // pack and bundle scrub contracts can never drift — this covers the
        // top-level prose, inline handle payloads, AND the nested user_context
        // prose fields (prompt_text, rule_text, decision_rationale, …). A
        // tampered row that restores any such field fails Safety even when its
        // row hash was recomputed so Integrity passes.
        if let Some(field) = crate::bundle::first_unscrubbed_field(&br.record) {
            return (
                false,
                format!("record {record_id} retains scrubbed field '{field}'"),
            );
        }
    }
    (
        true,
        "no raw sensitive classes; scrubbed prose/handle fields are None".to_owned(),
    )
}

/// Safety scan over the ENTIRE assembled pack artifact (Codex round-10 P1).
///
/// [`pack_safety`] only inspects the scrubbed section rows, so a secret injected
/// into a NON-record field — a `gaps[*].detail`, a `diagnostics[*].detail`, a
/// verdict `detail`, an echoed `manifest.control_title` from a malicious
/// `--catalog`, a section disclaimer, the top-level disclaimer — leaves every
/// record hash valid and slips past Safety. Since `eg audit evidence-pack verify`
/// is THE offline assertion that the artifact carries no raw sensitive classes,
/// Safety must scan the whole serialized artifact, not just the rows.
///
/// `detect_secret` is prefix/marker-anchored and empirically flags NONE of the
/// pack's legitimate high-entropy hex (BLAKE3 row/catalog hashes, `codegraph:v5:`
/// / `agent_memory:v1:` record IDs, commit SHAs, `protected:v1:` handles, and
/// `<REDACTED:email:...>` markers), so the clean scrubbed pack still passes. This
/// scan therefore (1) keeps the per-record scrubbed-field + secret contract via
/// [`pack_safety`], (2) scans every enumerated non-record text field so the
/// failure detail can name WHERE precisely, and (3) as a completeness backstop,
/// scans the whole serialized artifact so a secret in any string field not yet
/// enumerated below can never slip through. Details name the area/class only,
/// never the secret value.
fn pack_artifact_safety(pack: &EvidencePack, rows: &[BundleRecord]) -> (bool, String) {
    // (1) Per-record contract: scrubbed fields None + no secret inside a record.
    let (rec_ok, rec_detail) = pack_safety(rows);
    if !rec_ok {
        return (false, rec_detail);
    }
    // (2) Enumerated non-record text fields, for a redaction-safe WHERE detail.
    for (area, text) in nonrecord_text_fields(pack) {
        if let Some((class, _)) = crate::redaction::detect_secret(text) {
            return (
                false,
                format!(
                    "pack field '{area}' contains unredacted secret class: {}",
                    class.as_str()
                ),
            );
        }
    }
    // (3) Completeness backstop: scan the whole serialized artifact so a secret in
    //     ANY string field (including one not enumerated above, e.g. a future
    //     field) still fails Safety.
    let serialized = serde_json::to_string(pack).unwrap_or_default();
    if let Some((class, _)) = crate::redaction::detect_secret(&serialized) {
        return (
            false,
            format!(
                "pack artifact contains unredacted secret class: {}",
                class.as_str()
            ),
        );
    }
    (
        true,
        "no raw sensitive classes in records or pack artifact fields; scrubbed prose/handle fields are None"
            .to_owned(),
    )
}

/// Enumerates every string-bearing NON-record field of a pack as
/// `(area_path, text)` pairs, so [`pack_artifact_safety`] can scan them and name
/// WHERE a secret was found.
///
/// LOCKSTEP: whenever a string-bearing field is added to [`PackManifest`],
/// [`EvidenceSection`], [`GapRow`], [`PackDiagnostic`], or [`PackVerdicts`], add
/// it here so its area path appears in the failure detail. The whole-artifact
/// backstop in [`pack_artifact_safety`] still catches a field missed here, but
/// only this enumeration gives a precise WHERE. Section RECORDS are covered by
/// the per-record [`pack_safety`] scan and are intentionally excluded here.
fn nonrecord_text_fields(pack: &EvidencePack) -> Vec<(String, &str)> {
    let mut out: Vec<(String, &str)> = Vec::new();

    let m = &pack.manifest;
    out.push(("manifest.control_id".to_owned(), m.control_id.as_str()));
    out.push((
        "manifest.control_title".to_owned(),
        m.control_title.as_str(),
    ));
    out.push(("manifest.window.from".to_owned(), m.window.from.as_str()));
    out.push(("manifest.window.to".to_owned(), m.window.to.as_str()));
    out.push((
        "manifest.catalog_pin.catalog_id".to_owned(),
        m.catalog_pin.catalog_id.as_str(),
    ));
    out.push((
        "manifest.catalog_pin.catalog_hash".to_owned(),
        m.catalog_pin.catalog_hash.as_str(),
    ));
    out.push((
        "manifest.egregore_version".to_owned(),
        m.egregore_version.as_str(),
    ));
    if let Some(c) = m.captured_at.as_deref() {
        out.push(("manifest.captured_at".to_owned(), c));
    }
    out.push(("manifest.disclaimer".to_owned(), m.disclaimer.as_str()));

    for (i, s) in pack.sections.iter().enumerate() {
        out.push((format!("sections[{i}].class"), s.class.as_str()));
        out.push((format!("sections[{i}].requirement"), s.requirement.as_str()));
        out.push((format!("sections[{i}].status"), s.status.as_str()));
        if let Some(r) = s.unavailable_reason.as_deref() {
            out.push((format!("sections[{i}].unavailable_reason"), r));
        }
        if let Some(d) = s.disclaimer.as_deref() {
            out.push((format!("sections[{i}].disclaimer"), d));
        }
    }

    for (i, g) in pack.gaps.iter().enumerate() {
        out.push((format!("gaps[{i}].gap_class"), g.gap_class.as_str()));
        out.push((format!("gaps[{i}].detail"), g.detail.as_str()));
        if let Some(vt) = g.valid_time.as_deref() {
            out.push((format!("gaps[{i}].valid_time"), vt));
        }
    }

    for (i, d) in pack.diagnostics.iter().enumerate() {
        out.push((format!("diagnostics[{i}].code"), d.code.as_str()));
        out.push((format!("diagnostics[{i}].detail"), d.detail.as_str()));
        if let Some(ec) = d.evidence_class.as_deref() {
            out.push((format!("diagnostics[{i}].evidence_class"), ec));
        }
        if let Some(ur) = d.unavailable_reason.as_deref() {
            out.push((format!("diagnostics[{i}].unavailable_reason"), ur));
        }
    }

    let v = &pack.verdicts;
    out.push((
        "verdicts.required_classes.detail".to_owned(),
        v.required_classes.detail.as_str(),
    ));
    out.push((
        "verdicts.citation.detail".to_owned(),
        v.citation.detail.as_str(),
    ));
    out.push((
        "verdicts.integrity.detail".to_owned(),
        v.integrity.detail.as_str(),
    ));
    out.push((
        "verdicts.safety.detail".to_owned(),
        v.safety.detail.as_str(),
    ));
    out.push((
        "verdicts.review_coverage.status".to_owned(),
        v.review_coverage.status.as_str(),
    ));
    out.push((
        "verdicts.review_coverage.detail".to_owned(),
        v.review_coverage.detail.as_str(),
    ));
    if let Some(r) = v.review_coverage.not_applicable_reason.as_deref() {
        out.push((
            "verdicts.review_coverage.not_applicable_reason".to_owned(),
            r,
        ));
    }

    out
}

/// Offline re-verification verdicts for an assembled pack (AC9).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackVerifyReport {
    /// True only when every check passes.
    pub ok: bool,
    /// Integrity: recomputed hashes + canonical order.
    pub integrity: VerificationVerdict,
    /// Coverage: citation thresholds.
    pub coverage: VerificationVerdict,
    /// Safety: no raw sensitive classes; scrubbed fields None.
    pub safety: VerificationVerdict,
    /// Window consistency: every row's valid time inside the manifest window.
    pub window_consistency: VerificationVerdict,
}

/// Re-verifies an assembled pack offline and read-only (AC9).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn verify_pack(pack: &EvidencePack) -> PackVerifyReport {
    let mut all_rows: Vec<&BundleRecord> = Vec::new();
    for section in &pack.sections {
        for br in &section.records {
            all_rows.push(br);
        }
    }

    // Node lookup across the whole pack, keyed by record ID. Used below to bind
    // `review_coverage` edge endpoints (source approving review, target PR task)
    // to the nodes the pack actually carries (Codex round-16 P2).
    let node_by_id: BTreeMap<&str, &GraphRecord> = all_rows
        .iter()
        .filter(|br| matches!(br.record, GraphRecord::Node { .. }))
        .map(|br| (br.record.id(), &br.record))
        .collect();

    // Integrity: recompute per-record hash and per-section canonical ordering.
    let mut integrity_passed = true;
    let mut integrity_detail = "recomputed hashes match; sections canonically ordered".to_owned();
    'integrity: for section in &pack.sections {
        if section.record_count != section.records.len() {
            integrity_passed = false;
            integrity_detail = format!(
                "section {} record_count {} != records length {}",
                section.class,
                section.record_count,
                section.records.len()
            );
            break;
        }
        for br in &section.records {
            let serialized = serde_json::to_string(&br.record).unwrap_or_default();
            let computed = blake3::hash(serialized.as_bytes()).to_string();
            if computed != br.hash {
                integrity_passed = false;
                integrity_detail = format!(
                    "record {} hash mismatch in section {}",
                    br.record.id(),
                    section.class
                );
                break 'integrity;
            }
        }
        // Section membership: the per-record hash binds a row's CONTENT but not
        // the SECTION it sits in, so a row moved into the wrong section (with
        // counts fixed) would otherwise verify clean while consumers see it filed
        // under the wrong evidence class (Codex round-14 Finding 1). Every row in
        // a class-scoped section must map to that section's evidence class.
        if section.class == EvidenceClass::ReviewCoverage.as_wire() {
            // `review_coverage` is not class-scoped: it legitimately holds the
            // substantiating `REFERENCES_TASK` link edges (the measurement itself
            // rides the section's `measurement` field, not a row). Those edges map
            // to no evidence class, so the class check above cannot be applied. But
            // the exemption is BOUNDED to that expected shape (Codex round-15
            // Finding 1): a blanket pass would let a tampered pack smuggle an
            // arbitrary hashed row (a `Commit`, a `Symbol`, any node that maps to a
            // real evidence class, or any other edge label) into the section with
            // counts fixed and present unrelated data as coverage evidence. Every
            // review_coverage rows are exactly two shapes (Codex round-18 Finding
            // 2): (a) the cited `REFERENCES_TASK` link edges, and (b) the approving
            // review NODES that are the SOURCES of those edges, co-located so every
            // coverage edge's source resolves offline even when the catalog maps no
            // `reviews` section. A review node is admitted IFF it sources an included
            // coverage edge — no arbitrary review nodes — and anything else (a
            // `Commit`, any node mapping to a real evidence class, a non-approving
            // review, an unrelated edge) still fails Integrity.
            let coverage_edge_sources: BTreeSet<&str> = section
                .records
                .iter()
                .filter_map(|br| match &br.record {
                    GraphRecord::Edge { label, source, .. }
                        if label.as_str() == "REFERENCES_TASK" =>
                    {
                        Some(source.as_str())
                    }
                    _ => None,
                })
                .collect();
            for br in &section.records {
                let allowed = if is_expected_review_coverage_row(&br.record) {
                    true
                } else {
                    is_approving_review(&br.record)
                        && coverage_edge_sources.contains(br.record.id())
                };
                if !allowed {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "record {} is an unexpected row in review_coverage section \
                         (only REFERENCES_TASK link edges and their source approving \
                         reviews are permitted)",
                        br.record.id(),
                    );
                    break 'integrity;
                }
            }
            // Bind the coverage rows to the section's `ReviewCoverageMeasurement`
            // (Codex round-16 P2). The per-row hash proves each row's CONTENT and
            // the check above proves each row is a REFERENCES_TASK edge, but
            // neither binds the rows to the measurement the section presents. A
            // tampered pack could swap the genuine coverage edges for an unrelated
            // in-window REFERENCES_TASK edge (recomputing that row's hash + section
            // + manifest counts) while the measurement's `approved_pr_count` /
            // `approval_link_edge_ids` keep asserting a coverage the actual rows no
            // longer substantiate. Bind three ways so the measurement can only ride
            // the rows that back it.
            // A `review_coverage` section without a measurement is always
            // malformed (Codex round-17 P2 Finding 2). `assemble_pack` ALWAYS
            // emits the measurement — even for a 0%-coverage window with merged
            // PRs but no approving reviews (empty rows). An absent measurement
            // therefore means the coverage result (`merged_pr_count` /
            // `coverage` / threshold outcome) has been stripped from the
            // artifact, regardless of whether rows remain, so fail Integrity in
            // every case rather than only when rows are present.
            let Some(m) = &section.measurement else {
                integrity_passed = false;
                "review_coverage section carries no measurement (required on every \
                 review_coverage section)"
                    .clone_into(&mut integrity_detail);
                break 'integrity;
            };
            {
                // (1) The set of REFERENCES_TASK edge row IDs must EXACTLY
                //     equal the measurement's cited `approval_link_edge_ids`:
                //     no edge the measurement does not cite, no cited edge
                //     missing from the rows. The co-located source review
                //     nodes are NOT approval edges and are excluded here (they
                //     are bound to the edges by the membership check above).
                let row_ids: BTreeSet<&str> = section
                    .records
                    .iter()
                    .filter(|br| is_expected_review_coverage_row(&br.record))
                    .map(|br| br.record.id())
                    .collect();
                let cited: BTreeSet<&str> = m
                    .approval_link_edge_ids
                    .iter()
                    .map(String::as_str)
                    .collect();
                if let Some(unexpected) = row_ids.difference(&cited).next() {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "review_coverage row {unexpected} is not cited by the \
                             section measurement's approval_link_edge_ids"
                    );
                    break 'integrity;
                }
                if let Some(missing) = cited.difference(&row_ids).next() {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "review_coverage measurement cites approval edge {missing} \
                             that is absent from the section rows"
                    );
                    break 'integrity;
                }
                // (2) Each coverage edge must connect an approving review to a
                //     PR task, using the same source=review / target=PR
                //     convention `assemble_pack` used to build the edges.
                for br in &section.records {
                    let GraphRecord::Edge { source, target, .. } = &br.record else {
                        continue; // guaranteed REFERENCES_TASK edges above
                    };
                    // Source must be an approving review present in the pack.
                    // Its node is co-located in this review_coverage section (and
                    // may also ride a mapped `reviews` section); `node_by_id`
                    // spans every section, so its absence or wrong shape is
                    // tampering.
                    match node_by_id.get(source.as_str()) {
                        Some(rec) if is_approving_review(rec) => {}
                        _ => {
                            integrity_passed = false;
                            integrity_detail = format!(
                                "review_coverage edge {} source {source} is not an \
                                     approving review present in the pack",
                                br.record.id(),
                            );
                            break 'integrity;
                        }
                    }
                    // Target must be a PR task. A merged PR whose Task
                    // `valid_time` falls outside the window is legitimately
                    // absent from every section (coverage windows on
                    // `merged_at`, the PR section on `valid_time`), so an
                    // ABSENT target is not a defect (round-16/18) and cannot be
                    // merge-time-checked. A PRESENT target must be a
                    // pull-request task AND additionally satisfy `assemble_pack`'s
                    // exact coverage-edge eligibility (Codex round-19 P1): the
                    // endpoint-shape check alone let a tampered pack re-point a
                    // coverage edge at any approving-review -> PR pair (a
                    // post-merge approval, or a PR present for other reasons but
                    // not merged in-window), recompute hashes + counts, and still
                    // substantiate `approved_pr_count` offline. Mirror assemble:
                    //   (a) the target PR is MERGED IN-WINDOW — it carries a
                    //       `merged_at` (round-5 merge time) whose parsed value
                    //       falls in the half-open manifest window (the
                    //       `merged_pr_ids` selection); and
                    //   (b) the SOURCE review's resolved valid time is AT OR
                    //       BEFORE that `merged_at` (the at-or-before-merge gate,
                    //       round-9), so a post-merge approval cannot count.
                    if let Some(target_rec) = node_by_id.get(target.as_str()) {
                        if evidence_class_for_record(target_rec)
                            != Some(EvidenceClass::PullRequests)
                        {
                            integrity_passed = false;
                            integrity_detail = format!(
                                "review_coverage edge {} target {target} is present in \
                                 the pack but is not a pull-request task",
                                br.record.id(),
                            );
                            break 'integrity;
                        }
                        // (a) present target must be merged in-window: it has a
                        //     `merged_at` whose parsed time is inside the manifest
                        //     window (assemble's `merged_pr_ids` predicate).
                        let Some(merged_at) = merged_pr_merge_time(target_rec)
                            .filter(|mt| in_window(mt, &pack.manifest.window))
                            .and_then(parse_rfc3339)
                        else {
                            integrity_passed = false;
                            integrity_detail = format!(
                                "review_coverage edge {} target {target} is present but is \
                                 not a merged-in-window PR (no merged_at inside the manifest \
                                 window)",
                                br.record.id(),
                            );
                            break 'integrity;
                        };
                        // (b) source review's resolved valid time must be AT OR
                        //     BEFORE the target's `merged_at` (post-merge approval
                        //     does not gate the merge). The source node is present
                        //     and approving per the check above.
                        let source_at_or_before_merge = node_by_id
                            .get(source.as_str())
                            .and_then(|rec| resolve_valid_time(rec))
                            .as_deref()
                            .and_then(parse_rfc3339)
                            .is_some_and(|rt| rt <= merged_at);
                        if !source_at_or_before_merge {
                            integrity_passed = false;
                            integrity_detail = format!(
                                "review_coverage edge {} source {source} review valid time \
                                 is after the target {target} merged_at (post-merge approval \
                                 does not gate the merge)",
                                br.record.id(),
                            );
                            break 'integrity;
                        }
                    }
                }
                // (3) `approved_pr_count` must equal the distinct PR targets
                //     the coverage edges substantiate. A PR approved by
                //     multiple reviews yields multiple edges but is one
                //     approved PR, so the count keys on DISTINCT targets.
                let distinct_targets: BTreeSet<&str> = section
                    .records
                    .iter()
                    .filter_map(|br| match &br.record {
                        GraphRecord::Edge { target, .. } => Some(target.as_str()),
                        _ => None,
                    })
                    .collect();
                if distinct_targets.len() != m.approved_pr_count {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "review_coverage approved_pr_count {} does not equal the {} \
                             distinct approved PR target(s) its coverage edges substantiate",
                        m.approved_pr_count,
                        distinct_targets.len(),
                    );
                    break 'integrity;
                }
                // (4) The remaining measurement fields carry NO hashed row of
                //     their own, so tampering `merged_pr_count`, `coverage`,
                //     `passed`, or `unapproved_pr_ids` leaves every row hash,
                //     the row order, and the manifest counts valid while the
                //     stored coverage result lies (Codex round-17 P2 Finding
                //     1). Recheck each field against the assemble-time
                //     relationships (mirroring `assemble_pack` exactly) so a
                //     falsified result cannot verify clean.
                //
                //     `unapproved_pr_ids` must EXACTLY equal the set of PR ids
                //     the pack's own `merged_pr_without_approving_review` gap
                //     rows cite — BUT ONLY when those gaps can exist (Codex
                //     round-18 Finding 1). Assemble ALWAYS fills
                //     `unapproved_pr_ids` (merged minus approved), while it emits
                //     the `merged_pr_without_approving_review` gaps only for a
                //     control that requires PR/review evidence. The pack's own
                //     discriminator is the round-8 review_coverage verdict's
                //     `applicable` flag: gaps are emitted whenever the verdict is
                //     applicable/gating (a required-review control), so the
                //     `unapproved_pr_ids == gap set` equality holds and is
                //     enforced there. When the verdict is NOT applicable (a
                //     control that maps `review_coverage` merely OPTIONAL, or none
                //     at all), no such gap is emitted even though
                //     `unapproved_pr_ids` may be non-empty, so binding to the
                //     (empty) gap set would wrongly fail a freshly-assembled
                //     pack — skip it. This is a safe subset of the exact
                //     gap-emission condition (`requires_pull_requests ||
                //     requires_review`): whenever `applicable` is true the
                //     equality holds, and skipping only relaxes the check, never
                //     producing a false failure. The arithmetic/coverage/passed
                //     rechecks below still run in EVERY case.
                if pack.verdicts.review_coverage.applicable {
                    let gap_unapproved: BTreeSet<&str> = pack
                        .gaps
                        .iter()
                        .filter(|g| {
                            g.gap_class == GapClass::MergedPrWithoutApprovingReview.as_wire()
                        })
                        .flat_map(|g| g.record_ids.iter().map(String::as_str))
                        .collect();
                    let measured_unapproved: BTreeSet<&str> =
                        m.unapproved_pr_ids.iter().map(String::as_str).collect();
                    if gap_unapproved != measured_unapproved {
                        integrity_passed = false;
                        integrity_detail = format!(
                            "review_coverage unapproved_pr_ids ({} id(s)) does not equal the \
                                 {} PR id(s) of the pack's merged_pr_without_approving_review gaps",
                            measured_unapproved.len(),
                            gap_unapproved.len(),
                        );
                        break 'integrity;
                    }
                }
                // Every merged-in-window PR is either approved or unapproved,
                // so `merged_pr_count == approved_pr_count + unapproved_pr_ids`.
                let expected_merged = m.approved_pr_count + m.unapproved_pr_ids.len();
                if m.merged_pr_count != expected_merged {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "review_coverage merged_pr_count {} does not equal approved_pr_count \
                             {} + unapproved_pr_ids.len() {} = {}",
                        m.merged_pr_count,
                        m.approved_pr_count,
                        m.unapproved_pr_ids.len(),
                        expected_merged,
                    );
                    break 'integrity;
                }
                // `coverage` recomputed with assemble's exact formula and
                // arithmetic (`approved / merged`, vacuously 1.0 when none
                // merged). Compare bit patterns so an identical IEEE-754
                // division matches exactly and no float-epsilon drift is
                // introduced.
                #[allow(clippy::cast_precision_loss)]
                let expected_coverage = if m.merged_pr_count == 0 {
                    1.0_f64
                } else {
                    m.approved_pr_count as f64 / m.merged_pr_count as f64
                };
                if m.coverage.to_bits() != expected_coverage.to_bits() {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "review_coverage coverage {} does not equal the recomputed \
                             approved_pr_count / merged_pr_count = {}",
                        m.coverage, expected_coverage,
                    );
                    break 'integrity;
                }
                // `passed` must equal the coverage-vs-threshold predicate.
                // `min_required` is self-declared (the pack carries no
                // independent source for the `--min-review-coverage` value it
                // was assembled with), so this catches a lie in `passed`
                // alone against the stored coverage/min_required.
                let expected_passed = m.coverage >= m.min_required;
                if m.passed != expected_passed {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "review_coverage passed {} does not equal coverage {} >= \
                             min_required {} ({})",
                        m.passed, m.coverage, m.min_required, expected_passed,
                    );
                    break 'integrity;
                }
            }
        } else {
            for br in &section.records {
                let actual = evidence_class_for_record(&br.record).map(|c| c.as_wire());
                if actual != Some(section.class.as_str()) {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "record {} in section {} maps to evidence class {} (section membership mismatch)",
                        br.record.id(),
                        section.class,
                        actual.unwrap_or("none"),
                    );
                    break 'integrity;
                }
            }
        }
        for pair in section.records.windows(2) {
            if section_sort_key(&pair[0].record) > section_sort_key(&pair[1].record) {
                integrity_passed = false;
                integrity_detail =
                    format!("section {} rows are not canonically ordered", section.class);
                break 'integrity;
            }
        }
    }
    // Recompute the manifest aggregates from the actual included section rows and
    // fail Integrity if they diverge from the stored values. Without this a pack
    // tampered to drop rows (with the section `record_count` adjusted so the
    // per-section checks above still match) but the manifest
    // `included_record_counts` / `tuple_counts` left stale would verify clean
    // (Codex round-9 Finding 2). Mirrors `src/bundle.rs` `verify_bundle`, and
    // recomputes via the SAME `compute_manifest_counts` helper `assemble_pack`
    // populates the manifest with, so the two can never drift.
    if integrity_passed {
        let (recomputed_records, recomputed_tuples) =
            compute_manifest_counts(all_rows.iter().copied());
        if recomputed_records != pack.manifest.included_record_counts {
            integrity_passed = false;
            integrity_detail = manifest_count_divergence_detail(
                "included_record_counts",
                &pack.manifest.included_record_counts,
                &recomputed_records,
            );
        } else if recomputed_tuples != pack.manifest.tuple_counts {
            integrity_passed = false;
            integrity_detail = manifest_count_divergence_detail(
                "tuple_counts",
                &pack.manifest.tuple_counts,
                &recomputed_tuples,
            );
        }
    }
    let integrity = VerificationVerdict {
        passed: integrity_passed,
        detail: integrity_detail,
    };

    // Coverage: same citation thresholds.
    let (_tallies, code_pass, non_code_pass) = citation_view(&all_rows);
    let coverage_ok = code_pass && non_code_pass;
    let coverage = VerificationVerdict {
        passed: coverage_ok,
        detail: if coverage_ok {
            "code rows >=95% cited; non-code rows 100% cited".to_owned()
        } else {
            "citation thresholds not met".to_owned()
        },
    };

    // Safety scans the WHOLE artifact (records AND every non-record text field),
    // not just section rows (Codex round-10 P1).
    let owned_rows: Vec<BundleRecord> = all_rows.iter().map(|br| (*br).clone()).collect();
    let (safety_passed, safety_detail) = pack_artifact_safety(pack, &owned_rows);
    let safety = VerificationVerdict {
        passed: safety_passed,
        detail: safety_detail,
    };

    // Window consistency. First validate the manifest window bounds THEMSELVES —
    // the same parseable + half-open non-empty rule `assemble_pack` enforces (both
    // RFC3339; `from < to`, otherwise `reversed_window`). This runs BEFORE and
    // independent of the row/gap loops, so a vacuous pack (no section rows, no
    // timestamped gaps) carrying a hand-edited reversed or unparseable
    // `manifest.window` fails Window-consistency instead of passing vacuously
    // (Codex round-15 Finding 2). The parsed bounds are then REUSED by the row and
    // gap checks rather than re-parsed per row. Detail is redaction-safe.
    let (mut window_ok, mut window_detail, window_bounds) = match (
        parse_rfc3339(&pack.manifest.window.from),
        parse_rfc3339(&pack.manifest.window.to),
    ) {
        (Some(from), Some(to)) if from < to => (
            true,
            "every row's valid time is inside the manifest window".to_owned(),
            Some((from, to)),
        ),
        (None, _) | (_, None) => (
            false,
            "manifest window bound is not a valid RFC3339 timestamp".to_owned(),
            None,
        ),
        (Some(_), Some(_)) => (
            false,
            "manifest window is reversed or empty (from >= to)".to_owned(),
            None,
        ),
    };
    if let Some((from, to)) = window_bounds {
        'window: for section in &pack.sections {
            for br in &section.records {
                match resolve_valid_time(&br.record).and_then(|vt| parse_rfc3339(&vt)) {
                    Some(t) if from <= t && t < to => {}
                    _ => {
                        window_ok = false;
                        window_detail = format!(
                            "record {} in section {} is outside the manifest window",
                            br.record.id(),
                            section.class
                        );
                        break 'window;
                    }
                }
            }
        }
        // Gaps are timestamped rows in the exported pack and consumers filter them
        // by the same window, so a tampered `gaps[*].valid_time` outside `[from,
        // to)` must also fail Window-consistency (Codex round-13 Finding 1).
        // EXCEPTION: a `missing_valid_time` gap is intentionally untimestamped
        // (`valid_time: None`) and is allowed. A present-but-malformed timestamp is
        // not inside the window and fails, using the same half-open predicate
        // section rows use. The failure detail is redaction-safe: the bounded gap
        // class, which bound was violated, and the gap's own (allow-listed)
        // valid_time — nothing else.
        if window_ok {
            for g in &pack.gaps {
                let Some(vt) = &g.valid_time else {
                    // Only a `missing_valid_time` gap may be untimestamped; it is
                    // intentionally unwindowed. ANY other gap class with a null
                    // `valid_time` fails Window-consistency — consumers filter gaps
                    // by the manifest window and would drop/misplace an untimestamped
                    // one (Codex round-14 Finding 2). Detail is redaction-safe: the
                    // bounded gap class plus the reason, nothing else.
                    if g.gap_class == GapClass::MissingValidTime.as_wire() {
                        continue; // untimestamped (missing_valid_time): allowed
                    }
                    window_ok = false;
                    window_detail =
                        format!("gap (class {}) missing required timestamp", g.gap_class);
                    break;
                };
                let which = parse_rfc3339(vt).map_or(Some("malformed"), |t| {
                    if t < from {
                        Some("before window from")
                    } else if t >= to {
                        Some("at or after window to")
                    } else {
                        None // inside the window
                    }
                });
                if let Some(bound) = which {
                    window_ok = false;
                    window_detail = format!(
                        "gap valid_time {vt} (class {}) is outside the manifest window ({bound})",
                        g.gap_class
                    );
                    break;
                }
            }
        }
    }
    let window_consistency = VerificationVerdict {
        passed: window_ok,
        detail: window_detail,
    };

    let ok = integrity.passed && coverage.passed && safety.passed && window_consistency.passed;
    PackVerifyReport {
        ok,
        integrity,
        coverage,
        safety,
        window_consistency,
    }
}

/// Deterministic seed-fixture builder for issue #338 (shared by the in-crate
/// unit tests and used to regenerate the committed integration fixture).
///
/// Builds >=30 in-window and >=10 out-of-window records spanning commits, PR
/// Tasks (#333 fields), Reviews, and verification runs, including exactly three
/// merged PRs lacking an approving review (the success-metric gaps).
#[cfg(test)]
pub(crate) mod fixture {
    use crate::ir::{
        EdgeLabel, GraphRecord, NodeKind, PROJECT_SCHEMA_VERSION, SCHEMA_VERSION, TemporalMetadata,
        VERIFICATION_SCHEMA_VERSION,
    };

    /// Half-open window covering March 2026.
    pub const WINDOW_FROM: &str = "2026-03-01T00:00:00Z";
    pub const WINDOW_TO: &str = "2026-04-01T00:00:00Z";

    fn march(day: u32, hour: u32) -> String {
        format!("2026-03-{day:02}T{hour:02}:00:00Z")
    }

    fn node(id: &str, kind: NodeKind, schema: u32) -> GraphRecord {
        let mut r = GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            format!("summary for {id}"),
        );
        if let GraphRecord::Node { schema_version, .. } = &mut r {
            *schema_version = schema;
        }
        r
    }

    fn set_temporal_valid_time(mut r: GraphRecord, vt: &str) -> GraphRecord {
        let git_commit = format!("sha-{}", r.id());
        if let GraphRecord::Node { temporal, .. } = &mut r {
            *temporal = Some(TemporalMetadata {
                git_commit,
                git_parent_commits: Vec::new(),
                valid_time: vt.to_owned(),
                author_time: Some(vt.to_owned()),
                observed_at: vt.to_owned(),
                valid_time_source: Some("git_committer".to_owned()),
            });
        }
        r
    }

    fn commit(id: &str, vt: &str) -> GraphRecord {
        let mut r = set_temporal_valid_time(node(id, NodeKind::Commit, SCHEMA_VERSION), vt);
        // Carry raw Git authorship so the #116 email-redaction path is exercised
        // by scrub_record (the assembled pack must never leak the raw address).
        if let GraphRecord::Node {
            author_email,
            author_name,
            ..
        } = &mut r
        {
            *author_email = Some("dev@example.com".to_owned());
            *author_name = Some("Dev Example".to_owned());
        }
        r
    }

    fn commit_no_vt(id: &str) -> GraphRecord {
        node(id, NodeKind::Commit, SCHEMA_VERSION)
    }

    /// A per-file structural-delta `Change` node as emitted by `scan-history`
    /// (`NodeKind::Change`, path-scoped, valid time carried by the commit).
    pub fn change(id: &str, path: &str, vt: &str) -> GraphRecord {
        let r = GraphRecord::node(
            id.to_owned(),
            NodeKind::Change,
            Some(path.to_owned()),
            None,
            Some(format!("M {path}")),
            format!("Git change M to {path}"),
        );
        set_temporal_valid_time(r, vt)
    }

    /// A `Change` (`source_fact`) node whose only citable handle is a protected
    /// raw-artifact handle (`protected:v1:…`) carried in `source_handle`. This is
    /// the `citation_audit` `ExcludedProtected` vector: a code row that must count
    /// AGAINST the code citation gate, never as cited. The handle is placed in a
    /// field `scrub_record` preserves so it survives the pack scrub pipeline.
    pub fn protected_change(id: &str, path: &str, vt: &str) -> GraphRecord {
        let mut r = change(id, path, vt);
        if let GraphRecord::Node { source_handle, .. } = &mut r {
            *source_handle = Some(format!("protected:v1:{}", "0123456789abcdef".repeat(4)));
        }
        r
    }

    pub fn pr(id: &str, vt: &str, merge_commit: &str) -> GraphRecord {
        let mut r = node(id, NodeKind::Task, PROJECT_SCHEMA_VERSION);
        if let GraphRecord::Node {
            source_kind,
            entity_id,
            valid_time,
            merged_at,
            merge_commit_sha,
            head_sha,
            head_ref,
            base_ref,
            ..
        } = &mut r
        {
            *source_kind = Some("github_pr".to_owned());
            *entity_id = Some(format!("pr-entity-{id}"));
            *valid_time = Some(vt.to_owned());
            *merged_at = Some(vt.to_owned());
            *merge_commit_sha = Some(format!("sha-{merge_commit}"));
            *head_sha = Some(format!("head-sha-{id}"));
            *head_ref = Some(format!("feature/{id}"));
            *base_ref = Some("trunk".to_owned());
        }
        r
    }

    /// A merged GitHub-PR `Task` whose merge time (`merged_at`, promoted
    /// first-class in #333) is set INDEPENDENTLY of its `valid_time`. The GitHub
    /// importer stamps a PR Task's `valid_time` from `github_updated_at` (the
    /// PR's last-update time), which routinely differs from its merge time. This
    /// helper reproduces that split so tests can assert the merged-in-window
    /// determination keys on merge time, never update time (Codex round-5 P1).
    pub fn pr_with_merge_time(
        id: &str,
        updated_at: &str,
        merged_at_ts: &str,
        merge_commit: &str,
    ) -> GraphRecord {
        let mut r = pr(id, updated_at, merge_commit);
        if let GraphRecord::Node { merged_at, .. } = &mut r {
            *merged_at = Some(merged_at_ts.to_owned());
        }
        r
    }

    pub fn review(id: &str, vt: &str, state: &str) -> GraphRecord {
        let mut r = node(id, NodeKind::Review, PROJECT_SCHEMA_VERSION);
        if let GraphRecord::Node {
            entity_id,
            valid_time,
            review_kind,
            review_state,
            ..
        } = &mut r
        {
            *entity_id = Some(format!("review-entity-{id}"));
            *valid_time = Some(vt.to_owned());
            *review_kind = Some("pr_review".to_owned());
            *review_state = Some(state.to_owned());
        }
        r
    }

    /// A `Review` node with an explicit `review_kind` and optional `review_state`,
    /// used to exercise the genuine-PR-review classifier filter (GitHub imports
    /// emit `issue_comment` / `pr_review` / `pr_review_comment`).
    pub fn review_with_kind(id: &str, vt: &str, kind: &str, state: Option<&str>) -> GraphRecord {
        let mut r = node(id, NodeKind::Review, PROJECT_SCHEMA_VERSION);
        if let GraphRecord::Node {
            entity_id,
            valid_time,
            review_kind,
            review_state,
            ..
        } = &mut r
        {
            *entity_id = Some(format!("review-entity-{id}"));
            *valid_time = Some(vt.to_owned());
            *review_kind = Some(kind.to_owned());
            *review_state = state.map(str::to_owned);
        }
        r
    }

    fn verification(id: &str, executed: &str) -> GraphRecord {
        let mut r = node(id, NodeKind::CommandRun, VERIFICATION_SCHEMA_VERSION);
        if let GraphRecord::Node {
            executed_at,
            verification_kind,
            status,
            ..
        } = &mut r
        {
            *executed_at = Some(executed.to_owned());
            *verification_kind = Some("command_run".to_owned());
            *status = Some("passed".to_owned());
        }
        r
    }

    fn merged_as(pr_id: &str, commit_id: &str) -> GraphRecord {
        GraphRecord::project_edge(
            EdgeLabel::MergedAs,
            pr_id.to_owned(),
            commit_id.to_owned(),
            None,
            format!("{pr_id} merged as {commit_id}"),
        )
    }

    pub fn references_task(review_id: &str, pr_id: &str) -> GraphRecord {
        GraphRecord::project_edge(
            EdgeLabel::ReferencesTask,
            review_id.to_owned(),
            pr_id.to_owned(),
            None,
            format!("{review_id} references {pr_id}"),
        )
    }

    /// Builds the full deterministic seed record set.
    #[must_use]
    pub fn build_seed_records() -> Vec<GraphRecord> {
        let mut records: Vec<GraphRecord> = Vec::new();

        // In-window commits (14), c01..c06 are PR merge targets.
        for i in 1..=14u32 {
            records.push(commit(&format!("codegraph:v5:c{i:02}"), &march(i + 1, 9)));
        }
        // In-window commit with no resolvable valid time (missing_valid_time).
        records.push(commit_no_vt("codegraph:v5:c15"));

        // Out-of-window commits: Feb (5) + April (3).
        for i in 1..=5u32 {
            records.push(commit(
                &format!("codegraph:v5:cf{i}"),
                &format!("2026-02-{:02}T09:00:00Z", i + 1),
            ));
        }
        for i in 1..=3u32 {
            records.push(commit(
                &format!("codegraph:v5:ca{i}"),
                &format!("2026-04-{:02}T09:00:00Z", i + 1),
            ));
        }

        // In-window PRs (6): pr01..pr03 approved, pr04..pr06 unapproved.
        let pr_ids: Vec<String> = (1..=6u32).map(|i| format!("project:v1:pr{i:02}")).collect();
        for (i, pr_id) in pr_ids.iter().enumerate() {
            let commit_id = format!("codegraph:v5:c{:02}", i + 1);
            records.push(pr(pr_id, &march(3, 12), &commit_id));
            records.push(merged_as(pr_id, &commit_id));
        }

        // Reviews for pr01..pr03 (approved), pr04 (commented), pr06 (changes_requested).
        // pr05 has no review at all. Reviews resolve at 08:00 on merge day, i.e.
        // AT/BEFORE the 12:00 `merged_at` of the PRs they reference, so the three
        // approving reviews gated their merges and genuinely count toward approval
        // (Codex round-9 Finding 1: a post-merge approval does not suppress the
        // gap).
        let reviews = [
            ("project:v1:rv01", "project:v1:pr01", "approved"),
            ("project:v1:rv02", "project:v1:pr02", "approved"),
            ("project:v1:rv03", "project:v1:pr03", "approved"),
            ("project:v1:rv04", "project:v1:pr04", "commented"),
            ("project:v1:rv05", "project:v1:pr06", "changes_requested"),
        ];
        for (rid, pid, state) in reviews {
            records.push(review(rid, &march(3, 8), state));
            records.push(references_task(rid, pid));
        }

        // In-window verification runs (6).
        for i in 1..=6u32 {
            records.push(verification(
                &format!("verification:v1:ver{i:02}"),
                &march(5, 10),
            ));
        }

        // Out-of-window PRs (2), merged, no approving review — must not leak.
        records.push(pr("project:v1:prf1", "2026-02-15T12:00:00Z", "cf1"));
        records.push(merged_as("project:v1:prf1", "codegraph:v5:cf1"));
        records.push(pr("project:v1:pra1", "2026-04-15T12:00:00Z", "ca1"));
        records.push(merged_as("project:v1:pra1", "codegraph:v5:ca1"));

        records
    }

    /// Serializes the seed record set to deterministic JSONL.
    #[must_use]
    pub fn seed_jsonl() -> String {
        let mut lines: Vec<String> = build_seed_records()
            .iter()
            .map(|r| serde_json::to_string(r).expect("record serializes"))
            .collect();
        lines.push(String::new());
        lines.join("\n")
    }
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

#[cfg(test)]
mod pack338_tests {
    use super::fixture::{WINDOW_FROM, WINDOW_TO, build_seed_records, seed_jsonl};
    use super::*;

    fn win() -> Window {
        Window {
            from: WINDOW_FROM.to_owned(),
            to: WINDOW_TO.to_owned(),
        }
    }

    fn assemble_cc81() -> EvidencePack {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        assemble_pack(&records, &catalog, "CC8.1", &win(), 1.0, "test-0.0.0", None)
            .expect("assembles")
    }

    #[test]
    fn valid_time_resolution_order() {
        // temporal.valid_time wins over node valid_time and executed_at.
        let records = build_seed_records();
        let commit = records
            .iter()
            .find(|r| r.id() == "codegraph:v5:c01")
            .expect("commit present");
        assert_eq!(
            resolve_valid_time(commit).as_deref(),
            Some("2026-03-02T09:00:00Z")
        );
        // A verification record resolves through executed_at.
        let ver = records
            .iter()
            .find(|r| r.id() == "verification:v1:ver01")
            .expect("verification present");
        assert_eq!(
            resolve_valid_time(ver).as_deref(),
            Some("2026-03-05T10:00:00Z")
        );
        // A commit with no valid-time source resolves to None.
        let no_vt = records
            .iter()
            .find(|r| r.id() == "codegraph:v5:c15")
            .expect("c15 present");
        assert_eq!(resolve_valid_time(no_vt), None);
    }

    #[test]
    fn recall_is_total_and_leakage_is_zero() {
        let pack = assemble_cc81();
        // Commits section: 14 in-window commits (c15 excluded, out-of-window none).
        let commits = pack
            .sections
            .iter()
            .find(|s| s.class == "commits")
            .expect("commits section");
        assert_eq!(commits.record_count, 14);
        let ids: Vec<&str> = commits.records.iter().map(|r| r.record.id()).collect();
        assert!(ids.contains(&"codegraph:v5:c01"));
        assert!(!ids.iter().any(|id| id.starts_with("codegraph:v5:cf")));
        assert!(!ids.iter().any(|id| id.starts_with("codegraph:v5:ca")));
        assert!(!ids.contains(&"codegraph:v5:c15"));

        // Pull requests: 6 in-window, no out-of-window prf1/pra1.
        let prs = pack
            .sections
            .iter()
            .find(|s| s.class == "pull_requests")
            .expect("pr section");
        assert_eq!(prs.record_count, 6);
        assert!(!prs.records.iter().any(|r| r.record.id().contains("prf")));
        assert!(!prs.records.iter().any(|r| r.record.id().contains("pra")));

        // Rows are ordered by (valid_time, id).
        for pair in commits.records.windows(2) {
            assert!(section_sort_key(&pair[0].record) <= section_sort_key(&pair[1].record));
        }
    }

    #[test]
    fn every_control_class_is_a_section() {
        let pack = assemble_cc81();
        let classes: Vec<&str> = pack.sections.iter().map(|s| s.class.as_str()).collect();
        assert_eq!(
            classes,
            [
                "commits",
                "pull_requests",
                "reviews",
                "review_coverage",
                "structural_deltas",
                "public_api_deltas",
                "validation_runs",
                "verification_evidence",
            ]
        );
    }

    #[test]
    fn three_way_class_semantics_over_seed() {
        let pack = assemble_cc81();
        // Required + present => pass, populated or empty.
        for class in ["commits", "pull_requests", "reviews", "review_coverage"] {
            let s = pack.sections.iter().find(|s| s.class == class).unwrap();
            assert_eq!(s.status, "present", "{class} should be present");
            assert_eq!(s.outcome, ClassOutcome::Pass);
        }
        // Optional + unavailable => degraded marker + diagnostic.
        for class in ["structural_deltas", "public_api_deltas", "validation_runs"] {
            let s = pack.sections.iter().find(|s| s.class == class).unwrap();
            assert_eq!(s.status, "unavailable", "{class} should be unavailable");
            assert_eq!(s.outcome, ClassOutcome::ReportedOptionalUnavailable);
            assert!(s.unavailable_reason.is_some());
        }
        // Optional + present => pass (verification_evidence is populated).
        let ve = pack
            .sections
            .iter()
            .find(|s| s.class == "verification_evidence")
            .unwrap();
        assert_eq!(ve.status, "present");
        assert_eq!(ve.record_count, 6);
        // The degradation diagnostic exists.
        assert!(
            pack.diagnostics
                .iter()
                .any(|d| d.code == "evidence_class_unavailable")
        );
    }

    /// Codex round-4 P2: a GitHub import emits issue comments as `Review`
    /// (`review_kind == "issue_comment"`) records. Only genuine PR reviews
    /// (`pr_review` / `pr_review_comment`) are `Reviews`-class evidence; an
    /// issue comment — and any Review with no/unknown `review_kind` — must not
    /// be classified as review evidence.
    #[test]
    fn only_genuine_pr_reviews_classify_as_reviews() {
        use super::fixture::review_with_kind;
        let genuine = |kind: &str| {
            evidence_class_for_record(&review_with_kind(
                "project:v1:rvx",
                "2026-03-04T08:00:00Z",
                kind,
                None,
            ))
        };
        assert_eq!(genuine("pr_review"), Some(EvidenceClass::Reviews));
        assert_eq!(genuine("pr_review_comment"), Some(EvidenceClass::Reviews));
        // Issue comments are NOT review evidence.
        assert_eq!(genuine("issue_comment"), None);
        // A Review missing its kind is not counted (allow-list, not deny-list).
        assert_eq!(
            evidence_class_for_record(&review_with_kind(
                "project:v1:rvx",
                "2026-03-04T08:00:00Z",
                "some_future_kind",
                None
            )),
            None
        );
    }

    /// Codex round-4 P2: an in-window `issue_comment` Review referencing a PR
    /// task must not leak into the `reviews` section nor pad its record count.
    #[test]
    fn issue_comment_review_does_not_leak_into_reviews_section() {
        use super::fixture::{references_task, review_with_kind};
        let mut records = build_seed_records();
        // An issue comment on pr05 (the PR that has no genuine review at all).
        records.push(review_with_kind(
            "project:v1:ic01",
            "2026-03-04T08:00:00Z",
            "issue_comment",
            None,
        ));
        records.push(references_task("project:v1:ic01", "project:v1:pr05"));

        let catalog = load_default_catalog();
        let pack = assemble_pack(&records, &catalog, "CC8.1", &win(), 1.0, "test-0.0.0", None)
            .expect("assembles");
        let reviews = pack
            .sections
            .iter()
            .find(|s| s.class == "reviews")
            .expect("reviews section");
        // The 5 genuine seed pr_reviews remain; the issue comment is excluded.
        assert_eq!(reviews.record_count, 5);
        assert!(
            !reviews
                .records
                .iter()
                .any(|r| r.record.id() == "project:v1:ic01"),
            "issue_comment review must not appear in reviews section"
        );
    }

    /// Codex round-4 P2: an `issue_comment` Review never counts as an approving
    /// review for gap suppression, even if it carries an `approved` state.
    #[test]
    fn issue_comment_review_never_approves() {
        use super::fixture::review_with_kind;
        let issue_comment = review_with_kind(
            "project:v1:ic02",
            "2026-03-04T08:00:00Z",
            "issue_comment",
            Some("approved"),
        );
        assert!(!is_approving_review(&issue_comment));
        // A genuine approving pr_review still approves.
        let genuine = review_with_kind(
            "project:v1:rvz",
            "2026-03-04T08:00:00Z",
            "pr_review",
            Some("approved"),
        );
        assert!(is_approving_review(&genuine));
    }

    /// Codex round-2 P2: `scan-history` emits per-file structural deltas as
    /// `NodeKind::Change` records. They have a real stored backing kind, so the
    /// `structural_deltas` section must be PRESENT and carry in-window Change
    /// record IDs — never silently `unavailable`/`delta_domain_absent`.
    #[test]
    fn structural_deltas_populated_from_in_window_change_records() {
        use super::fixture::change;
        let records = vec![
            change("codegraph:v5:chg01", "src/lib.rs", "2026-03-05T09:00:00Z"),
            change("codegraph:v5:chg02", "src/main.rs", "2026-03-06T09:00:00Z"),
            // Out-of-window (February) change must not appear.
            change("codegraph:v5:chg99", "src/old.rs", "2026-02-05T09:00:00Z"),
        ];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        let sd = pack
            .sections
            .iter()
            .find(|s| s.class == "structural_deltas")
            .expect("structural_deltas section");
        assert_eq!(sd.status, "present", "structural_deltas must be present");
        assert_eq!(sd.outcome, ClassOutcome::Pass);
        assert_eq!(sd.record_count, 2);
        let ids: Vec<&str> = sd.records.iter().map(|r| r.record.id()).collect();
        assert!(ids.contains(&"codegraph:v5:chg01"));
        assert!(ids.contains(&"codegraph:v5:chg02"));
        assert!(!ids.contains(&"codegraph:v5:chg99"));
        // Rows ordered by (valid_time, id).
        for pair in sd.records.windows(2) {
            assert!(section_sort_key(&pair[0].record) <= section_sort_key(&pair[1].record));
        }
    }

    /// Present-but-empty: Change records exist in the store but none fall in the
    /// window. The section is PRESENT with zero rows (an optional present-empty
    /// class passes), NOT `unavailable`.
    #[test]
    fn structural_deltas_present_but_empty_when_no_in_window_change() {
        use super::fixture::change;
        let records = vec![change(
            "codegraph:v5:chg99",
            "src/old.rs",
            "2026-02-05T09:00:00Z",
        )];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        let sd = pack
            .sections
            .iter()
            .find(|s| s.class == "structural_deltas")
            .expect("structural_deltas section");
        assert_eq!(sd.status, "present");
        assert_eq!(sd.outcome, ClassOutcome::Pass);
        assert_eq!(sd.record_count, 0);
        assert!(sd.unavailable_reason.is_none());
    }

    /// Computed-only classes (#157 public-api deltas, #103 validation) have no
    /// stored backing node kind, so they degrade with the honest
    /// `derived_class_not_materialized` reason — never the misleading
    /// `log_domain_absent`, and never `delta_domain_absent` now that structural
    /// deltas are a genuine stored class.
    #[test]
    fn computed_only_classes_report_derived_not_materialized() {
        let pack = assemble_cc81();
        for class in ["public_api_deltas", "validation_runs"] {
            let s = pack.sections.iter().find(|s| s.class == class).unwrap();
            assert_eq!(s.status, "unavailable", "{class} should be unavailable");
            assert_eq!(
                s.unavailable_reason.as_deref(),
                Some("derived_class_not_materialized"),
                "{class} must use the honest computed-only reason"
            );
        }
    }

    #[test]
    fn three_planted_gaps_surface_with_correct_ids() {
        let pack = assemble_cc81();
        let mut gap_prs: Vec<&str> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "merged_pr_without_approving_review")
            .flat_map(|g| g.record_ids.iter().map(String::as_str))
            .collect();
        gap_prs.sort_unstable();
        assert_eq!(
            gap_prs,
            ["project:v1:pr04", "project:v1:pr05", "project:v1:pr06"]
        );
        // Out-of-window merged-no-review PRs must not surface.
        assert!(
            !gap_prs
                .iter()
                .any(|id| id.contains("prf") || id.contains("pra"))
        );
    }

    /// Codex finding 1: an approving review whose resolved valid time falls
    /// OUTSIDE the pack window must not suppress the
    /// `merged_pr_without_approving_review` gap. The review is omitted from the
    /// windowed `reviews` section, so counting it toward approval overstates
    /// in-window review coverage.
    #[test]
    fn out_of_window_approving_review_does_not_suppress_gap() {
        use super::fixture::{pr, references_task, review};
        // Merged, in-window PR whose ONLY approving review resolves in April,
        // outside the March [from, to) window.
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            review("project:v1:rvX", "2026-04-15T08:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        // The gap must be present.
        assert!(
            pack.gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prX".to_owned())),
            "out-of-window approval must not suppress the gap: gaps={:?}",
            pack.gaps
        );
        // And review-coverage measurement must count the PR as unapproved.
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.merged_pr_count, 1);
        assert_eq!(rc.approved_pr_count, 0);
        assert!(
            rc.unapproved_pr_ids.contains(&"project:v1:prX".to_owned()),
            "PR with only out-of-window approval must be unapproved"
        );
    }

    /// Positive companion: an in-window approving review submitted AT/BEFORE the
    /// PR's merge time DOES suppress the gap.
    ///
    /// (Round-9 Finding 1: the review time was previously `2026-03-16` — AFTER
    /// the `2026-03-15` merge — which encoded the post-hoc-approval bug this fix
    /// corrects. A genuine gate-passing approval must precede the merge, so the
    /// review now resolves BEFORE `merged_at`.)
    #[test]
    fn in_window_approving_review_suppresses_gap() {
        use super::fixture::{pr, references_task, review};
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            review("project:v1:rvX", "2026-03-14T08:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prX".to_owned())),
            "in-window approval must suppress the gap: gaps={:?}",
            pack.gaps
        );
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.merged_pr_count, 1);
        assert_eq!(rc.approved_pr_count, 1);
        assert!(rc.unapproved_pr_ids.is_empty());
    }

    /// Codex round-9 Finding 1: an approving review whose resolved valid time is
    /// AFTER the PR's `merged_at` (yet still inside the pack window) did NOT gate
    /// the merge — it is post-hoc. It must NOT suppress the
    /// `merged_pr_without_approving_review` gap, and the PR must count as
    /// unapproved for review coverage. Before the fix the in-window approval
    /// suppressed the gap regardless of whether it preceded the merge.
    #[test]
    fn approval_after_merge_time_does_not_suppress_gap() {
        use super::fixture::{pr, references_task, review};
        // PR merged EARLY in-window (merged_at == valid_time == 2026-03-05).
        // Its only approving review resolves later in-window (2026-03-20), AFTER
        // the merge.
        let records = vec![
            pr("project:v1:prX", "2026-03-05T00:00:00Z", "cX"),
            review("project:v1:rvX", "2026-03-20T08:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        assert!(
            pack.gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prX".to_owned())),
            "post-merge approval must not suppress the gap: gaps={:?}",
            pack.gaps
        );
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.merged_pr_count, 1);
        assert_eq!(rc.approved_pr_count, 0);
        assert!(
            rc.unapproved_pr_ids.contains(&"project:v1:prX".to_owned()),
            "PR whose only approval is post-merge must be unapproved"
        );
    }

    /// Regression companion to the round-9 fix: an approving review whose valid
    /// time EQUALS the PR's `merged_at` (the at-or-before boundary) still counts
    /// as a gate-passing approval and suppresses the gap.
    #[test]
    fn approval_at_merge_time_suppresses_gap() {
        use super::fixture::{pr, references_task, review};
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            // Exactly at merged_at (== the PR valid_time set by `pr`).
            review("project:v1:rvX", "2026-03-15T12:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prX".to_owned())),
            "approval exactly at merge time must suppress the gap: gaps={:?}",
            pack.gaps
        );
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.approved_pr_count, 1);
        assert!(rc.unapproved_pr_ids.is_empty());
    }

    /// Codex round-5 P1: a PR MERGED inside the window but whose Task
    /// `valid_time` (stamped from `github_updated_at`, the PR's last-update time)
    /// falls AFTER the window must still count as merged-in-window. The
    /// merged-in-window determination for review coverage and the
    /// `merged_pr_without_approving_review` gap keys on `merged_at` (merge time),
    /// never the update-time `valid_time`. Before the fix this PR was dropped from
    /// `merged_pr_ids`, so review coverage vacuously passed and the gap was
    /// suppressed.
    #[test]
    fn merged_in_window_but_updated_after_window_counts_as_merged() {
        use super::fixture::pr_with_merge_time;
        // merged_at inside March window; updated_at (valid_time) in April, after.
        let records = vec![pr_with_merge_time(
            "project:v1:prLate",
            "2026-04-15T08:00:00Z", // updated_at -> Task valid_time (after window)
            "2026-03-15T12:00:00Z", // merged_at -> merge time (in window)
            "cLate",
        )];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(
            rc.merged_pr_count, 1,
            "PR merged in-window must count even when updated after the window"
        );
        assert_eq!(rc.approved_pr_count, 0);
        assert!(
            rc.unapproved_pr_ids
                .contains(&"project:v1:prLate".to_owned())
        );
        assert!(
            pack.gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prLate".to_owned())),
            "merged-in-window PR without approving review must gap: gaps={:?}",
            pack.gaps
        );
    }

    /// Codex round-5 P1 (mirror): a PR merged BEFORE the window but UPDATED inside
    /// it must NOT count as merged-in-window. Before the fix the update-time
    /// `valid_time` wrongly pulled it into `merged_pr_ids`.
    #[test]
    fn merged_before_window_but_updated_in_window_is_not_merged_in_window() {
        use super::fixture::pr_with_merge_time;
        let records = vec![pr_with_merge_time(
            "project:v1:prEarly",
            "2026-03-15T08:00:00Z", // updated_at -> Task valid_time (in window)
            "2026-02-15T12:00:00Z", // merged_at -> merge time (before window)
            "cEarly",
        )];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(
            rc.merged_pr_count, 0,
            "PR merged before the window must not count, even if updated in-window"
        );
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prEarly".to_owned())),
            "PR merged before window must not gap: gaps={:?}",
            pack.gaps
        );
    }

    /// Codex round-8 P2 (Finding 1): a CC7.2 monitoring pack over a shared store
    /// that happens to contain an unapproved in-window merged PR must NOT fail its
    /// gate on unrelated review coverage. CC7.2 requires no review evidence, so
    /// its review-coverage verdict is a neutral `not_applicable` status that never
    /// contributes to the pack `ok` and there is no PR/review gap.
    #[test]
    fn cc72_review_coverage_is_neutral_and_does_not_fail_gate() {
        use super::fixture::pr_with_merge_time;
        let records = vec![pr_with_merge_time(
            "project:v1:prMon",
            "2026-03-15T08:00:00Z", // updated_at -> Task valid_time (in window)
            "2026-03-15T12:00:00Z", // merged_at -> merge time (in window)
            "cMon",
        )];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC7.2",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        // A non-review control never fails the gate on review coverage alone.
        assert!(
            pack.verdicts.ok,
            "CC7.2 must not fail solely on unrelated review coverage: {:?}",
            pack.verdicts
        );
        // The review-coverage verdict is neutral / not-applicable.
        assert!(!pack.verdicts.review_coverage.applicable);
        assert_eq!(pack.verdicts.review_coverage.status, "not_applicable");
        assert_eq!(
            pack.verdicts
                .review_coverage
                .not_applicable_reason
                .as_deref(),
            Some("control_does_not_require_review")
        );
        // No PR/review section and no PR gap for a non-review control.
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"),
            "non-review control emits no PR gap: gaps={:?}",
            pack.gaps
        );
    }

    /// Codex round-8 P2 (Finding 1, regression): a review-requiring control
    /// (CC8.1) over the SAME unapproved in-window merged PR still gates on
    /// coverage — the verdict is `gating`/applicable and the pack fails.
    #[test]
    fn cc81_review_coverage_still_gates_on_same_store() {
        use super::fixture::pr_with_merge_time;
        let records = vec![pr_with_merge_time(
            "project:v1:prMon",
            "2026-03-15T08:00:00Z",
            "2026-03-15T12:00:00Z",
            "cMon",
        )];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        assert!(pack.verdicts.review_coverage.applicable);
        assert_eq!(pack.verdicts.review_coverage.status, "gating");
        assert!(
            pack.verdicts
                .review_coverage
                .not_applicable_reason
                .is_none()
        );
        assert!(!pack.verdicts.review_coverage.passed);
        assert!(
            !pack.verdicts.ok,
            "CC8.1 must still fail on unapproved coverage"
        );
    }

    /// Codex round-8 P2 (Finding 2): a PR MERGED in-window but whose Task
    /// `valid_time` (`github_updated_at`) falls AFTER the window is selected as
    /// merged-in-window by merge time; the emitted
    /// `merged_pr_without_approving_review` gap ROW must be stamped with that same
    /// in-window merge time, not the out-of-window update time. Before the fix the
    /// row was stamped via `resolve_valid_time` = the update-time `valid_time`.
    #[test]
    fn merged_pr_gap_row_is_stamped_with_merge_time() {
        use super::fixture::pr_with_merge_time;
        let records = vec![pr_with_merge_time(
            "project:v1:prLate2",
            "2026-04-15T08:00:00Z", // updated_at -> Task valid_time (after window)
            "2026-03-15T12:00:00Z", // merged_at -> merge time (in window)
            "cLate2",
        )];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        let gap = pack
            .gaps
            .iter()
            .find(|g| {
                g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prLate2".to_owned())
            })
            .expect("merged-in-window unapproved PR gaps");
        assert_eq!(
            gap.valid_time.as_deref(),
            Some("2026-03-15T12:00:00Z"),
            "gap row must be stamped with the in-window merge time, not the out-of-window update time"
        );
    }

    /// Regression: over the seed store the merged-in-window set is exactly the 6
    /// in-window PRs (pr01..pr06); the out-of-window prf1/pra1 never enter, and
    /// review coverage counts all six so the default gate still fails (3 of 6
    /// unapproved).
    #[test]
    fn seed_merged_pr_window_is_exactly_six_and_gate_fails() {
        let pack = assemble_cc81();
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.merged_pr_count, 6);
        assert_eq!(rc.approved_pr_count, 3);
        assert!(!rc.passed, "3-of-6 coverage must fail the default 1.0 gate");
        let mut unapproved = rc.unapproved_pr_ids.clone();
        unapproved.sort();
        assert_eq!(
            unapproved,
            ["project:v1:pr04", "project:v1:pr05", "project:v1:pr06"]
        );
    }

    #[test]
    fn missing_valid_time_is_counted_and_gapped() {
        let pack = assemble_cc81();
        assert_eq!(pack.manifest.excluded_missing_valid_time, 1);
        assert!(
            pack.diagnostics
                .iter()
                .any(|d| d.code == "missing_valid_time"
                    && d.record_ids.contains(&"codegraph:v5:c15".to_owned()))
        );
        assert!(pack.gaps.iter().any(|g| g.gap_class == "missing_valid_time"
            && g.record_ids.contains(&"codegraph:v5:c15".to_owned())));
    }

    /// Codex round-13 Finding 2: a class-relevant record whose resolved valid
    /// time is present but NON-RFC3339 (malformed) must be routed to the same
    /// `missing_valid_time` path as a truly-absent valid time — counted,
    /// diagnosed, and gapped — never silently excluded as merely out-of-window.
    /// A required class whose only evidence has a malformed timestamp must not
    /// look present-but-empty.
    #[test]
    fn malformed_valid_time_is_treated_as_missing_not_silently_dropped() {
        let mut records = build_seed_records();
        let target = "codegraph:v5:c01";
        let mut hit = false;
        for r in &mut records {
            if r.id() == target
                && let GraphRecord::Node { temporal, .. } = r
                && let Some(t) = temporal.as_mut()
            {
                t.valid_time = "not-a-timestamp".to_owned();
                hit = true;
            }
        }
        assert!(hit, "corrupted the target commit's valid time");

        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        // Counted under missing_valid_time alongside the absent-vt c15 (2 total),
        // never silently dropped as out-of-window.
        assert_eq!(
            pack.manifest.excluded_missing_valid_time, 2,
            "malformed-vt record must be counted as missing, not silently dropped"
        );
        assert!(
            pack.diagnostics.iter().any(
                |d| d.code == "missing_valid_time" && d.record_ids.contains(&target.to_owned())
            ),
            "missing_valid_time diagnostic must cite the malformed-vt record: {:?}",
            pack.diagnostics
        );
        assert!(
            pack.gaps.iter().any(|g| g.gap_class == "missing_valid_time"
                && g.record_ids.contains(&target.to_owned())),
            "missing_valid_time gap must cite the malformed-vt record: {:?}",
            pack.gaps
        );

        // It must NOT be silently placed in the commits section on the basis of a
        // garbage timestamp.
        let commits = pack
            .sections
            .iter()
            .find(|s| s.class == "commits")
            .expect("commits section");
        assert!(
            !commits.records.iter().any(|r| r.record.id() == target),
            "malformed-vt commit must not appear in the section"
        );
    }

    #[test]
    fn commit_outside_any_pr_gaps_are_non_merge_commits() {
        let pack = assemble_cc81();
        let outside: Vec<&str> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "commit_outside_any_pr")
            .flat_map(|g| g.record_ids.iter().map(String::as_str))
            .collect();
        // c07..c14 (8) are not merge targets; c01..c06 are.
        assert_eq!(outside.len(), 8);
        assert!(!outside.contains(&"codegraph:v5:c01"));
        assert!(outside.contains(&"codegraph:v5:c07"));
    }

    #[test]
    fn issue_334_gap_classes_degrade_without_facts() {
        let pack = assemble_cc81();
        assert!(
            pack.diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent"))
        );
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "review_unanchored_no_commit_sha")
        );
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "approval_precedes_final_head")
        );
    }

    /// Codex round-7 P2: the #334-dependent capability diagnostic must be
    /// UNCONDITIONAL for a review-requiring control. The previous code probed
    /// the input for the reviewed-commit facts #334 will add (a
    /// `review_commit_sha` field / `REVIEWS_COMMIT` edge) and SUPPRESSED the
    /// diagnostic when it saw them — but #334's derivation is unmerged, so no
    /// `review_unanchored_no_commit_sha` / `approval_precedes_final_head` rows
    /// were ever produced. An input that merely RESEMBLED the probed facts thus
    /// made the pack look as if the two checks ran cleanly: a false all-clear.
    /// Until #334 lands the diagnostic must always fire and the two gap classes
    /// must stay empty, regardless of what the input happens to contain.
    #[test]
    fn issue_334_capability_diagnostic_fires_even_when_input_resembles_probed_facts() {
        let mut records = build_seed_records();
        // A record whose serialized JSON contains the `review_commit_sha` token
        // the old probe keyed on — this would have tripped the suppression
        // branch. There is no real #334 field to set, so we plant the token in
        // an ordinary node field; the point is that resemblance must NOT be
        // mistaken for a derivation that never ran.
        records.push(GraphRecord::node(
            "codegraph:v5:probe".to_owned(),
            crate::ir::NodeKind::Change,
            Some("src/probe.rs".to_owned()),
            None,
            Some("review_commit_sha".to_owned()),
            "review_commit_sha".to_owned(),
        ));
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        assert!(
            pack.diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent")),
            "the #334 capability diagnostic must fire unconditionally, even when \
             the input resembles the probed reviewed-commit facts: diagnostics={:?}",
            pack.diagnostics
        );
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "review_unanchored_no_commit_sha"),
            "no #334 gap rows can be derived until #334 lands"
        );
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "approval_precedes_final_head"),
            "no #334 gap rows can be derived until #334 lands"
        );
    }

    /// Codex round-3 finding A: gap derivation is control-scoped. A CC7.2
    /// (monitoring) pack over a store rich in merged-PR-without-review and
    /// commit-outside-PR facts must emit NONE of the change-management PR/commit
    /// gap classes — those evidence classes are not required by CC7.2 — nor the
    /// #334 review-anchored capability diagnostic. Only the generic
    /// `missing_valid_time` gap (about class-relevant records excluded for
    /// lacking valid time) may appear.
    #[test]
    fn cc72_pack_emits_no_pr_commit_review_gaps() {
        let records = build_seed_records();
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC7.2",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        for class in [
            "merged_pr_without_approving_review",
            "commit_outside_any_pr",
            "review_unanchored_no_commit_sha",
            "approval_precedes_final_head",
        ] {
            assert!(
                !pack.gaps.iter().any(|g| g.gap_class == class),
                "CC7.2 must not emit gap class {class}: gaps={:?}",
                pack.gaps
            );
        }
        // The #334 capability diagnostic is scoped to the review-anchored pair,
        // so it must not appear for a control that requires no review evidence.
        assert!(
            !pack
                .diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent")),
            "CC7.2 must not emit the #334 review-anchored capability diagnostic"
        );
        // The generic missing_valid_time gap stays unconditional: c15 is a
        // class-relevant Commit with no resolvable valid time.
        assert!(
            pack.gaps.iter().any(|g| g.gap_class == "missing_valid_time"
                && g.record_ids.contains(&"codegraph:v5:c15".to_owned())),
            "missing_valid_time must remain generic across controls"
        );
    }

    /// Regression guard for finding A: CC8.1 over the SAME store still emits the
    /// change-management PR/commit gaps (it requires those classes), including
    /// the three planted `merged_pr_without_approving_review` gaps and the
    /// commit-outside-PR gaps, plus the #334 review-anchored diagnostic.
    #[test]
    fn cc81_pack_still_emits_pr_commit_gaps_over_same_store() {
        let records = build_seed_records();
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        let mut planted: Vec<&str> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "merged_pr_without_approving_review")
            .flat_map(|g| g.record_ids.iter().map(String::as_str))
            .collect();
        planted.sort_unstable();
        assert_eq!(
            planted,
            ["project:v1:pr04", "project:v1:pr05", "project:v1:pr06"]
        );
        assert!(
            pack.gaps
                .iter()
                .any(|g| g.gap_class == "commit_outside_any_pr"),
            "CC8.1 must still emit commit_outside_any_pr gaps"
        );
        assert!(
            pack.diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent")),
            "CC8.1 must still emit the #334 review-anchored capability diagnostic"
        );
    }

    /// Codex round-3 finding B: a `source_fact` code row whose only handle is a
    /// protected raw-artifact handle is `ExcludedProtected` — it must count
    /// AGAINST the pack code citation gate (excluded, not cited), byte-identical
    /// to how `eg audit citations` classifies the same records. Before the fix
    /// the pack treated every non-`MissingRequiredHandle` status as cited, so a
    /// protected-only code row passed the gate while `citation_audit` failed it.
    #[test]
    fn protected_only_code_row_counts_excluded_not_cited_parity_with_citation_audit() {
        use super::fixture::{change, protected_change};
        let records = vec![
            change("codegraph:v5:chg01", "src/a.rs", "2026-03-10T09:00:00Z"),
            protected_change("codegraph:v5:chgP", "src/b.rs", "2026-03-11T09:00:00Z"),
        ];
        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        // Pack source_fact tally: one cited (documented-absent Change) + one
        // excluded (protected), zero missing.
        let sf = pack
            .verdicts
            .citation_tallies
            .iter()
            .find(|t| t.trust_class == "source_fact")
            .expect("source_fact tally present");
        assert_eq!(sf.total, 2, "two source_fact rows");
        assert_eq!(sf.cited, 1, "only the documented-absent Change is cited");
        assert_eq!(
            sf.excluded, 1,
            "the protected-only row is excluded, not cited"
        );
        assert_eq!(sf.missing, 0);

        // The excluded protected row drives code completeness below 0.95, so the
        // pack citation verdict now fails — matching citation_audit's gate.
        assert!(
            !pack.verdicts.citation.passed,
            "protected-only code row must fail the pack code citation gate"
        );

        // Byte-for-byte parity: recompute the per-class tally the way
        // citation_audit classifies rows over the SAME scrubbed section rows.
        let structural = pack
            .sections
            .iter()
            .find(|s| s.class == "structural_deltas")
            .expect("structural_deltas section");
        let mut audit_total = 0usize;
        let mut audit_cited = 0usize;
        let mut audit_excluded = 0usize;
        let mut audit_missing = 0usize;
        for br in &structural.records {
            let row = classify_record_external(&br.record);
            assert_eq!(row.trust_class, "source_fact");
            audit_total += 1;
            match row.status {
                CitationStatus::Cited | CitationStatus::AbsentHandleDocumented => {
                    audit_cited += 1;
                }
                CitationStatus::MissingRequiredHandle => audit_missing += 1,
                CitationStatus::ExcludedProtected | CitationStatus::ExcludedUnverified => {
                    audit_excluded += 1;
                }
            }
        }
        assert_eq!(audit_total, sf.total, "tally parity: total");
        assert_eq!(audit_cited, sf.cited, "tally parity: cited");
        assert_eq!(audit_excluded, sf.excluded, "tally parity: excluded");
        assert_eq!(audit_missing, sf.missing, "tally parity: missing");
    }

    #[test]
    fn review_coverage_gate_fails_at_default_threshold() {
        let pack = assemble_cc81();
        assert!(!pack.verdicts.ok);
        assert!(!pack.verdicts.review_coverage.passed);
        // But required classes and citation pass.
        assert!(pack.verdicts.required_classes.passed);
        assert!(pack.verdicts.citation.passed);
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .unwrap()
            .measurement
            .clone()
            .unwrap();
        assert_eq!(rc.merged_pr_count, 6);
        assert_eq!(rc.approved_pr_count, 3);
        assert!((rc.coverage - 0.5).abs() < 1e-9);
    }

    #[test]
    fn assembly_is_deterministic() {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        let a = assemble_pack(&records, &catalog, "CC8.1", &win(), 1.0, "v", None).unwrap();
        let b = assemble_pack(&records, &catalog, "CC8.1", &win(), 1.0, "v", None).unwrap();
        let ja = serde_json::to_string(&a).unwrap();
        let jb = serde_json::to_string(&b).unwrap();
        assert_eq!(ja, jb);
    }

    #[test]
    fn unknown_control_names_known_ids() {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        let err = assemble_pack(&records, &catalog, "ZZ9.9", &win(), 1.0, "v", None)
            .expect_err("unknown");
        assert_eq!(err.code(), "unknown_control");
        assert_eq!(
            err,
            PackBuildError::UnknownControl {
                control_id: "ZZ9.9".to_owned(),
                known: vec!["CC7.2".to_owned(), "CC7.3".to_owned(), "CC8.1".to_owned()],
            }
        );
    }

    #[test]
    fn reversed_and_invalid_windows_are_rejected() {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        let reversed = Window {
            from: WINDOW_TO.to_owned(),
            to: WINDOW_FROM.to_owned(),
        };
        assert_eq!(
            assemble_pack(&records, &catalog, "CC8.1", &reversed, 1.0, "v", None)
                .unwrap_err()
                .code(),
            "reversed_window"
        );
        let bad = Window {
            from: "not-a-time".to_owned(),
            to: WINDOW_TO.to_owned(),
        };
        assert_eq!(
            assemble_pack(&records, &catalog, "CC8.1", &bad, 1.0, "v", None)
                .unwrap_err()
                .code(),
            "invalid_timestamp"
        );
    }

    #[test]
    fn empty_window_is_vacuous_success() {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        // A window before any record: all required classes still resolve as
        // Present (capability), sections empty, review coverage vacuously 1.0.
        let empty = Window {
            from: "2026-01-01T00:00:00Z".to_owned(),
            to: "2026-01-02T00:00:00Z".to_owned(),
        };
        let pack = assemble_pack(&records, &catalog, "CC8.1", &empty, 1.0, "v", None).unwrap();
        assert!(pack.verdicts.ok, "empty window should be vacuous success");
        for s in &pack.sections {
            assert_eq!(s.record_count, 0);
        }
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .unwrap()
            .measurement
            .clone()
            .unwrap();
        assert_eq!(rc.merged_pr_count, 0);
        assert!((rc.coverage - 1.0).abs() < 1e-9);
    }

    #[test]
    fn verify_passes_clean_pack_and_fails_tamper() {
        let pack = assemble_cc81();
        let report = verify_pack(&pack);
        assert!(report.ok, "clean pack verifies: {report:?}");

        // Tamper: flip a single byte in a stored hash.
        let mut tampered = pack;
        let section = tampered
            .sections
            .iter_mut()
            .find(|s| !s.records.is_empty())
            .unwrap();
        let h = &mut section.records[0].hash;
        let last = h.pop().unwrap();
        h.push(if last == 'a' { 'b' } else { 'a' });
        let report = verify_pack(&tampered);
        assert!(!report.ok);
        assert!(!report.integrity.passed);
    }

    /// Codex round-9 Finding 2: a pack tampered by removing a section row and
    /// decrementing THAT section's `record_count` (so per-section length still
    /// matches, and remaining rows keep valid hashes + canonical order) while
    /// leaving `manifest.included_record_counts` / `manifest.tuple_counts` STALE
    /// must FAIL Integrity. Before the fix, verify never recomputed the manifest
    /// aggregates from the actual rows, so this self-inconsistent artifact
    /// verified clean.
    #[test]
    fn verify_fails_when_manifest_counts_diverge_from_rows() {
        let pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "clean pack integrity passes"
        );

        let mut tampered = pack;
        // Drop the LAST row of the first non-empty section: canonical ordering is
        // preserved (sorted list, tail removed) and every remaining row's hash is
        // unchanged, so the ONLY inconsistency left is the stale manifest total.
        let section = tampered
            .sections
            .iter_mut()
            .find(|s| !s.records.is_empty())
            .unwrap();
        section.records.pop();
        section.record_count -= 1;

        let report = verify_pack(&tampered);
        assert!(
            !report.integrity.passed,
            "stale manifest included/tuple counts must fail Integrity"
        );
        assert!(!report.ok);
        // Redaction-safe detail: names the divergent aggregate and the numbers,
        // never a payload.
        assert!(
            report.integrity.detail.contains("count"),
            "detail names the divergent count: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_fails_on_restored_user_context_prose() {
        // A tampered pack whose section row has nested user_context prose
        // restored (prompt_text / rule_text) with its row hash recomputed so
        // Integrity still passes must FAIL the Safety verdict. This mirrors the
        // #68 bundle scrub/safety contract: every field `scrub_record` clears —
        // including nested user_context prose — must be asserted None by verify.
        for field in ["prompt_text", "rule_text"] {
            let mut pack = assemble_cc81();
            // Baseline: a properly scrubbed pack passes Safety.
            assert!(verify_pack(&pack).safety.passed, "clean pack passes safety");

            // Restore benign (non-secret) nested prose on a Node section row.
            let br = pack
                .sections
                .iter_mut()
                .flat_map(|s| s.records.iter_mut())
                .find(|br| matches!(br.record, GraphRecord::Node { .. }))
                .expect("a node section row exists");
            if let GraphRecord::Node { user_context, .. } = &mut br.record {
                match field {
                    "prompt_text" => {
                        user_context.prompt_text = Some("benign restored prompt".to_owned());
                    }
                    "rule_text" => {
                        user_context.rule_text = Some("benign restored rule".to_owned());
                    }
                    _ => unreachable!(),
                }
            }
            // Recompute the row hash so Integrity still passes.
            let serialized = serde_json::to_string(&br.record).unwrap();
            br.hash = blake3::hash(serialized.as_bytes()).to_string();
            let record_id = br.record.id().to_owned();

            let report = verify_pack(&pack);
            assert!(
                report.integrity.passed,
                "integrity still passes after hash recompute: {}",
                report.integrity.detail
            );
            assert!(!report.ok, "overall verdict fails on restored prose");
            assert!(
                !report.safety.passed,
                "safety must fail on restored user_context.{field}"
            );
            // Redaction-safe detail: names the field + record id, never the value.
            assert!(
                report.safety.detail.contains(field),
                "detail names the restored field: {}",
                report.safety.detail
            );
            assert!(
                report.safety.detail.contains(&record_id),
                "detail names the record id: {}",
                report.safety.detail
            );
            assert!(
                !report.safety.detail.contains("benign restored"),
                "detail must never leak the restored value: {}",
                report.safety.detail
            );
        }
    }

    #[test]
    fn no_raw_payload_or_email_in_serialized_pack() {
        let pack = assemble_cc81();
        let json = serde_json::to_string(&pack).unwrap();
        // The raw author email (#116) is never present; the redaction marker is.
        assert!(!json.contains("dev@example.com"));
        assert!(
            json.contains("<REDACTED:email:"),
            "commit author emails must be redacted, exercising the #116 path"
        );
        // The disclaimer is present verbatim.
        assert_eq!(pack.manifest.disclaimer, PACK_DISCLAIMER);
    }

    #[test]
    fn captured_at_is_only_present_when_pinned() {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        let without = assemble_pack(&records, &catalog, "CC8.1", &win(), 1.0, "v", None).unwrap();
        assert!(without.manifest.captured_at.is_none());
        let with = assemble_pack(
            &records,
            &catalog,
            "CC8.1",
            &win(),
            1.0,
            "v",
            Some("2026-05-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            with.manifest.captured_at.as_deref(),
            Some("2026-05-01T00:00:00Z")
        );
    }

    /// A secret string `detect_secret` reliably flags (`cloud_credential`) and
    /// which contains none of the pack's legitimate hex — so a hit is
    /// unambiguously the injected secret, not a false positive on a hash/handle.
    const INJECTED_SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

    /// Codex round-10 P1: the clean scrubbed seed pack must still PASS Safety
    /// once Safety scans the whole artifact. Guards against `detect_secret`
    /// false-positives on the pack's legitimate high-entropy hex (BLAKE3 hashes,
    /// record IDs, catalog/protected handles, `<REDACTED:email:...>` markers).
    #[test]
    fn verify_clean_pack_passes_whole_artifact_safety() {
        let pack = assemble_cc81();
        let report = verify_pack(&pack);
        assert!(
            report.safety.passed,
            "clean scrubbed pack must pass whole-artifact Safety: {}",
            report.safety.detail
        );
        assert!(report.ok, "clean pack verifies clean: {report:?}");
    }

    /// Codex round-10 P1: a secret injected into a non-record field
    /// (`manifest.control_title`, as a malicious `--catalog` would echo) must
    /// FAIL Safety even though every record hash stays valid so Integrity passes.
    #[test]
    fn verify_fails_on_secret_in_manifest_control_title() {
        let mut pack = assemble_cc81();
        assert!(verify_pack(&pack).safety.passed, "baseline passes");

        pack.manifest.control_title = format!("Change Management {INJECTED_SECRET}");

        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "record hashes untouched so Integrity still passes: {}",
            report.integrity.detail
        );
        assert!(
            !report.safety.passed,
            "Safety must fail on a secret in manifest.control_title"
        );
        assert!(!report.ok, "overall verdict fails");
        assert!(
            report.safety.detail.contains("control_title"),
            "detail names WHERE: {}",
            report.safety.detail
        );
        assert!(
            !report.safety.detail.contains(INJECTED_SECRET),
            "detail must never leak the secret value: {}",
            report.safety.detail
        );
    }

    /// Codex round-10 P1: a secret injected into a `gaps[*].detail` (a non-record
    /// field) must FAIL Safety while Integrity stays green.
    #[test]
    fn verify_fails_on_secret_in_gap_detail() {
        let mut pack = assemble_cc81();
        assert!(verify_pack(&pack).safety.passed, "baseline passes");

        pack.gaps.push(GapRow {
            gap_class: "missing_valid_time".to_owned(),
            record_ids: Vec::new(),
            valid_time: None,
            detail: format!("tampered gap note {INJECTED_SECRET}"),
        });

        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "Integrity still passes: {}",
            report.integrity.detail
        );
        assert!(
            !report.safety.passed,
            "Safety must fail on a secret in gaps[*].detail"
        );
        assert!(!report.ok, "overall verdict fails");
        assert!(
            report.safety.detail.contains("gaps"),
            "detail names WHERE: {}",
            report.safety.detail
        );
        assert!(
            !report.safety.detail.contains(INJECTED_SECRET),
            "detail must never leak the secret value: {}",
            report.safety.detail
        );
    }

    /// Codex round-13 Finding 1: Window-consistency must also reject a
    /// `gaps[*].valid_time` present but outside the half-open manifest window.
    /// Gaps are timestamped rows consumers filter by the same window; a tampered
    /// gap time must not verify clean while section rows are untouched. Gaps carry
    /// no integrity hash, so Integrity stays green with no recomputation.
    #[test]
    fn verify_fails_when_gap_valid_time_is_outside_window() {
        let mut pack = assemble_cc81();
        let report = verify_pack(&pack);
        assert!(
            report.window_consistency.passed,
            "clean pack passes window-consistency: {}",
            report.window_consistency.detail
        );
        assert!(
            pack.gaps.iter().any(|g| g.valid_time.is_none()),
            "fixture carries an untimestamped missing_valid_time gap"
        );

        let idx = pack
            .gaps
            .iter()
            .position(|g| g.valid_time.is_some())
            .expect("a timestamped gap exists");
        let tampered_class = pack.gaps[idx].gap_class.clone();
        pack.gaps[idx].valid_time = Some("2026-05-01T00:00:00Z".to_owned());

        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "Integrity still passes: {}",
            report.integrity.detail
        );
        assert!(
            !report.window_consistency.passed,
            "Window-consistency must reject a gap valid_time outside [from, to)"
        );
        assert!(
            report.window_consistency.detail.contains(&tampered_class),
            "detail names the gap class: {}",
            report.window_consistency.detail
        );
        assert!(
            report
                .window_consistency
                .detail
                .contains("2026-05-01T00:00:00Z"),
            "detail echoes the gap's own valid_time: {}",
            report.window_consistency.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-13 Finding 1 (exception): an untimestamped
    /// `missing_valid_time` gap (`valid_time: None`) is intentionally
    /// unwindowed and must PASS Window-consistency, never be flagged.
    #[test]
    fn verify_allows_untimestamped_missing_valid_time_gap() {
        let mut pack = assemble_cc81();
        pack.gaps.retain(|g| g.valid_time.is_none());
        assert!(
            !pack.gaps.is_empty(),
            "at least one untimestamped gap remains after the retain"
        );
        let report = verify_pack(&pack);
        assert!(
            report.window_consistency.passed,
            "untimestamped (None) gaps must pass Window-consistency: {}",
            report.window_consistency.detail
        );
    }

    /// Codex round-14 P2 (Finding 1): Integrity binds each row to its section's
    /// evidence class. A row moved into the wrong section — with both sections'
    /// `record_count` fixed and hashes/manifest counts left valid (they are
    /// content-addressed / content-keyed, not section-keyed) — passes every other
    /// Integrity check yet is filed under the wrong evidence class. Membership must
    /// fail Integrity naming the record and both classes.
    #[test]
    fn verify_fails_when_row_is_filed_under_wrong_section_class() {
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline pack passes Integrity"
        );

        // Move a Commit row out of `commits` into `reviews`.
        let commit_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "commits")
            .expect("commits section");
        assert!(
            !pack.sections[commit_idx].records.is_empty(),
            "commits section has rows to move"
        );
        let moved = pack.sections[commit_idx].records.remove(0);
        pack.sections[commit_idx].record_count -= 1;
        let moved_id = moved.record.id().to_owned();

        let reviews_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "reviews")
            .expect("reviews section");
        pack.sections[reviews_idx].records.push(moved);
        // Keep the reviews section canonically ordered so the ordering check is
        // not what fails — only section membership is wrong.
        pack.sections[reviews_idx]
            .records
            .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
        pack.sections[reviews_idx].record_count += 1;

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a mis-filed row must fail Integrity via section membership"
        );
        assert!(
            report.integrity.detail.contains(&moved_id),
            "detail names the mis-filed record: {}",
            report.integrity.detail
        );
        assert!(
            report.integrity.detail.contains("reviews"),
            "detail names the section it sits in: {}",
            report.integrity.detail
        );
        assert!(
            report.integrity.detail.contains("commits"),
            "detail names the class the record actually maps to: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-14 P2 (Finding 1, exception): the `review_coverage` section
    /// legitimately holds the `ReviewCoverageMeasurement` and `REFERENCES_TASK`
    /// link edges, both of which map to `None` from `evidence_class_for_record`.
    /// The membership check must exempt `review_coverage` so a clean pack passes.
    #[test]
    fn verify_allows_review_coverage_section_membership() {
        let pack = assemble_cc81();
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        assert!(
            rc.records
                .iter()
                .any(|br| evidence_class_for_record(&br.record).is_none()),
            "review_coverage section carries rows that map to no evidence class"
        );
        assert!(
            verify_pack(&pack).integrity.passed,
            "review_coverage membership must not fail Integrity: {}",
            verify_pack(&pack).integrity.detail
        );
    }

    /// Codex round-14 P2 (Finding 2): only a `missing_valid_time` gap may be
    /// untimestamped. A `merged_pr_without_approving_review` gap whose timestamp
    /// was stripped must FAIL Window-consistency — consumers filter gaps by the
    /// manifest window and would drop/misplace an untimestamped one.
    #[test]
    fn verify_fails_when_non_missing_valid_time_gap_lacks_timestamp() {
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).window_consistency.passed,
            "clean pack passes Window-consistency"
        );
        let idx = pack
            .gaps
            .iter()
            .position(|g| g.gap_class == "merged_pr_without_approving_review")
            .expect("fixture carries a merged_pr_without_approving_review gap");
        pack.gaps[idx].valid_time = None;

        let report = verify_pack(&pack);
        assert!(
            !report.window_consistency.passed,
            "a non-missing_valid_time gap with null valid_time must fail Window-consistency"
        );
        assert!(
            report
                .window_consistency
                .detail
                .contains("merged_pr_without_approving_review"),
            "detail names the gap class: {}",
            report.window_consistency.detail
        );
        assert!(
            report
                .window_consistency
                .detail
                .contains("missing required timestamp"),
            "detail states the reason: {}",
            report.window_consistency.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-15 P2 (Finding 1): the `review_coverage` section membership
    /// exemption must be CONSTRAINED to the section's expected rows (the stamped
    /// `REFERENCES_TASK` link edges), not a blanket pass. A tampered pack that
    /// drops an arbitrary hashed row — a `Commit` — into `review_coverage`, fixes
    /// the section counts, and keeps hashes + canonical order valid must FAIL
    /// Integrity naming the record. Otherwise unrelated data is presented as
    /// coverage evidence and offline verify certifies it clean.
    #[test]
    fn verify_fails_when_review_coverage_holds_unexpected_row() {
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline pack passes Integrity"
        );

        // Move a Commit row out of `commits` into `review_coverage`.
        let commit_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "commits")
            .expect("commits section");
        assert!(
            !pack.sections[commit_idx].records.is_empty(),
            "commits section has rows to move"
        );
        let moved = pack.sections[commit_idx].records.remove(0);
        pack.sections[commit_idx].record_count -= 1;
        let moved_id = moved.record.id().to_owned();

        let rc_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        pack.sections[rc_idx].records.push(moved);
        // Keep the section canonically ordered so ordering is not what fails —
        // only the unexpected-row membership rule is violated.
        pack.sections[rc_idx]
            .records
            .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
        pack.sections[rc_idx].record_count += 1;

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "an unexpected row in review_coverage must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains(&moved_id),
            "detail names the offending record: {}",
            report.integrity.detail
        );
        assert!(
            report.integrity.detail.contains("review_coverage"),
            "detail names the review_coverage section: {}",
            report.integrity.detail
        );
        assert!(
            report.integrity.detail.contains("unexpected row"),
            "detail explains the offense: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-15 P2 (Finding 1, positive) + round-18 Finding 2: the
    /// untampered pack's genuine `review_coverage` rows — the stamped
    /// `REFERENCES_TASK` link edges AND the co-located source approving-review
    /// nodes — still pass the constrained membership check.
    #[test]
    fn verify_allows_expected_review_coverage_link_edge_rows() {
        let pack = assemble_cc81();
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        assert!(
            !rc.records.is_empty(),
            "review_coverage carries the substantiating link edges"
        );
        // The edge sources present in the section (round-18: co-located review
        // nodes must each source one of these edges).
        let edge_sources: std::collections::BTreeSet<&str> = rc
            .records
            .iter()
            .filter_map(|br| match &br.record {
                GraphRecord::Edge { label, source, .. } if label.as_str() == "REFERENCES_TASK" => {
                    Some(source.as_str())
                }
                _ => None,
            })
            .collect();
        for br in &rc.records {
            let is_edge = matches!(&br.record, GraphRecord::Edge { label, .. } if label.as_str() == "REFERENCES_TASK");
            let is_source_review =
                is_approving_review(&br.record) && edge_sources.contains(br.record.id());
            assert!(
                is_edge || is_source_review,
                "every expected review_coverage row is a REFERENCES_TASK edge or a \
                 source approving-review node: {:?}",
                br.record
            );
        }
        assert!(
            verify_pack(&pack).integrity.passed,
            "expected review_coverage rows must pass Integrity: {}",
            verify_pack(&pack).integrity.detail
        );
    }

    /// Replaces one `review_coverage` LINK EDGE row with `edge` (an already
    /// stamped `REFERENCES_TASK` edge), recomputing the section `record_count`
    /// and manifest counts so every pre-existing Integrity check still passes.
    /// Returns `(replaced_old_id, new_id)`. The measurement is left untouched so
    /// the caller can decide whether to re-cite the new edge.
    ///
    /// Round-18: the section now also co-locates the source approving-review
    /// nodes. Swapping an edge can orphan its source review (no remaining edge
    /// sources it); to keep the section otherwise-consistent so ONLY the intended
    /// tamper differs, any co-located review node no longer sourcing a
    /// `REFERENCES_TASK` edge is pruned before counts are recomputed.
    fn swap_one_coverage_row(pack: &mut EvidencePack, edge: GraphRecord) -> (String, String) {
        let rc_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        let edge_pos = pack.sections[rc_idx]
            .records
            .iter()
            .position(|br| matches!(&br.record, GraphRecord::Edge { label, .. } if label.as_str() == "REFERENCES_TASK"))
            .expect("review_coverage carries a link edge to swap");
        let old_id = pack.sections[rc_idx].records[edge_pos]
            .record
            .id()
            .to_owned();
        let new_row = build_section_records(vec![edge]).remove(0);
        let new_id = new_row.record.id().to_owned();
        pack.sections[rc_idx].records[edge_pos] = new_row;
        // Prune any co-located review node no longer sourcing a link edge so the
        // section stays self-consistent apart from the intended tamper.
        let sources: BTreeSet<String> = pack.sections[rc_idx]
            .records
            .iter()
            .filter_map(|br| match &br.record {
                GraphRecord::Edge { label, source, .. } if label.as_str() == "REFERENCES_TASK" => {
                    Some(source.clone())
                }
                _ => None,
            })
            .collect();
        pack.sections[rc_idx].records.retain(|br| match &br.record {
            GraphRecord::Node { .. } => sources.contains(br.record.id()),
            _ => true,
        });
        // Keep the section canonically ordered so ordering is never what fails.
        pack.sections[rc_idx]
            .records
            .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
        pack.sections[rc_idx].record_count = pack.sections[rc_idx].records.len();
        // Recompute the manifest aggregates from the swapped rows so the
        // manifest-count check still passes and the new binding is the only thing
        // that can fail.
        let all: Vec<&BundleRecord> = pack
            .sections
            .iter()
            .flat_map(|s| s.records.iter())
            .collect();
        let (records, tuples) = compute_manifest_counts(all.iter().copied());
        pack.manifest.included_record_counts = records;
        pack.manifest.tuple_counts = tuples;
        (old_id, new_id)
    }

    /// Codex round-16 P2: the described attack. A tampered pack REPLACES a real
    /// coverage row with an unrelated in-window `REFERENCES_TASK` edge and
    /// recomputes that row's hash + section `record_count` + manifest counts, but
    /// leaves the section's `ReviewCoverageMeasurement.approval_link_edge_ids`
    /// citing the ORIGINAL edges. The rows no longer match the measurement they
    /// substantiate, so verify must FAIL Integrity naming the unbound id.
    #[test]
    fn verify_fails_when_coverage_rows_do_not_match_cited_approval_edges() {
        use super::fixture::references_task;
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline pack passes Integrity"
        );

        // An unrelated in-window REFERENCES_TASK edge (present in the seed graph
        // but NOT a coverage-substantiating edge). Stamp it in-window so it clears
        // Window-consistency; measurement is deliberately left untouched.
        let bad = stamp_edge_valid_time(
            references_task("project:v1:rv04", "project:v1:pr04"),
            "2026-03-03T08:00:00Z",
        );
        let (old_id, new_id) = swap_one_coverage_row(&mut pack, bad);

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "coverage rows unbound from the measurement must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains(&new_id) || report.integrity.detail.contains(&old_id),
            "detail names the unbound edge id: {}",
            report.integrity.detail
        );
        assert!(
            report.integrity.detail.contains("review_coverage"),
            "detail names the section: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-16 P2: a smarter attacker also re-cites the swapped edge in
    /// `approval_link_edge_ids` (so the id-set check passes) but the substituted
    /// `REFERENCES_TASK` edge's SOURCE is a non-approving (commented) review. The
    /// edge does not connect an approving review to a PR, so verify must FAIL.
    #[test]
    fn verify_fails_when_coverage_edge_source_is_not_approving_review() {
        use super::fixture::references_task;
        let mut pack = assemble_cc81();

        // rv04 is a genuine PR review present in the reviews section but its state
        // is `commented`, so it is not an approving review.
        let bad = stamp_edge_valid_time(
            references_task("project:v1:rv04", "project:v1:pr04"),
            "2026-03-03T08:00:00Z",
        );
        let (old_id, new_id) = swap_one_coverage_row(&mut pack, bad);

        // Re-cite: swap old_id for new_id in the measurement so the id-set check
        // passes and the endpoint-shape check is the only thing that can fail.
        let rc_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "review_coverage")
            .unwrap();
        let m = pack.sections[rc_idx].measurement.as_mut().unwrap();
        m.approval_link_edge_ids.retain(|id| id != &old_id);
        m.approval_link_edge_ids.push(new_id.clone());
        m.approval_link_edge_ids.sort();

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a coverage edge whose source is not an approving review must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains(&new_id)
                && report.integrity.detail.contains("project:v1:rv04"),
            "detail names the offending edge and its source: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-16 P2: the swapped edge's SOURCE is a genuine approving review
    /// but its TARGET is a Commit present in the pack, not a PR task. A coverage
    /// edge must connect an approving review to a pull-request task, so verify
    /// must FAIL Integrity.
    #[test]
    fn verify_fails_when_coverage_edge_target_is_not_pr_task() {
        use super::fixture::references_task;
        let mut pack = assemble_cc81();

        // Approving review rv01 -> a Commit (present in the commits section),
        // which is not a pull-request task.
        let bad = stamp_edge_valid_time(
            references_task("project:v1:rv01", "codegraph:v5:c01"),
            "2026-03-03T08:00:00Z",
        );
        let (old_id, new_id) = swap_one_coverage_row(&mut pack, bad);

        let rc_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "review_coverage")
            .unwrap();
        let m = pack.sections[rc_idx].measurement.as_mut().unwrap();
        m.approval_link_edge_ids.retain(|id| id != &old_id);
        m.approval_link_edge_ids.push(new_id.clone());
        m.approval_link_edge_ids.sort();

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a coverage edge whose target is not a PR task must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains(&new_id)
                && report.integrity.detail.contains("codegraph:v5:c01"),
            "detail names the offending edge and its target: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-16 P2: `approved_pr_count` must stay consistent with the
    /// distinct PR targets the coverage edges substantiate. Inflating the count
    /// alone (rows and cited ids untouched) must FAIL Integrity.
    #[test]
    fn verify_fails_when_approved_pr_count_mismatches_coverage_edges() {
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline pack passes Integrity"
        );
        let rc_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "review_coverage")
            .unwrap();
        let m = pack.sections[rc_idx].measurement.as_mut().unwrap();
        m.approved_pr_count += 2; // 3 distinct targets, now claims 5

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "an approved_pr_count that overstates the coverage edges must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains("approved_pr_count"),
            "detail names the mismatch: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-16 P2 (positive): the untampered pack's genuine coverage rows
    /// stay bound to the measurement — the new binding must not reject a clean
    /// pack.
    #[test]
    fn verify_allows_coverage_rows_bound_to_measurement() {
        let pack = assemble_cc81();
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        let m = rc.measurement.as_ref().expect("measurement present");
        assert!(!m.approval_link_edge_ids.is_empty(), "cites the link edges");
        assert_eq!(m.approved_pr_count, 3);
        assert!(
            verify_pack(&pack).integrity.passed,
            "clean bound coverage rows must pass Integrity: {}",
            verify_pack(&pack).integrity.detail
        );
    }

    /// Codex round-17 P2 (Finding 1): tampering `measurement.passed` to the
    /// opposite (threshold-passing) value without touching any hashed row must
    /// FAIL Integrity. `passed` must equal `coverage >= min_required`.
    #[test]
    fn verify_fails_when_measurement_passed_flipped() {
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline pack passes Integrity"
        );
        let m = pack
            .sections
            .iter_mut()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_mut())
            .expect("review_coverage measurement");
        assert!(!m.passed, "baseline 3-of-6 coverage did not pass the gate");
        m.passed = true; // lie: claim the threshold was met

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a passed flag inconsistent with coverage/min_required must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains("passed"),
            "detail names the field: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-17 P2 (Finding 1): tampering `measurement.coverage` to a
    /// threshold-passing value without touching any hashed row must FAIL
    /// Integrity. `coverage` is recomputed from the bound counts with the exact
    /// assemble formula.
    #[test]
    fn verify_fails_when_measurement_coverage_tampered() {
        let mut pack = assemble_cc81();
        let m = pack
            .sections
            .iter_mut()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_mut())
            .expect("review_coverage measurement");
        m.coverage = 1.0; // lie: claim full coverage (real is 3/6 = 0.5)

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a coverage inconsistent with the bound counts must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains("coverage"),
            "detail names the field: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-17 P2 (Finding 1): tampering `merged_pr_count` (every merged
    /// PR is either approved or unapproved) without touching any hashed row must
    /// FAIL Integrity.
    #[test]
    fn verify_fails_when_merged_pr_count_tampered() {
        let mut pack = assemble_cc81();
        let m = pack
            .sections
            .iter_mut()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_mut())
            .expect("review_coverage measurement");
        m.merged_pr_count = 3; // lie: 3 approved + 3 unapproved != 3

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "merged_pr_count != approved_pr_count + unapproved_pr_ids.len() must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains("merged_pr_count"),
            "detail names the field: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-17 P2 (Finding 1): tampering `unapproved_pr_ids` — the list
    /// must EXACTLY equal the PR ids of the pack's own
    /// `merged_pr_without_approving_review` gap rows.
    #[test]
    fn verify_fails_when_unapproved_pr_ids_tampered() {
        let mut pack = assemble_cc81();
        let m = pack
            .sections
            .iter_mut()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_mut())
            .expect("review_coverage measurement");
        // Drop one genuinely-unapproved PR and substitute an approved one, keeping
        // the length (and thus merged_pr_count arithmetic) intact so only the
        // gap-set binding can catch the lie.
        m.unapproved_pr_ids = vec![
            "project:v1:pr01".to_owned(),
            "project:v1:pr05".to_owned(),
            "project:v1:pr06".to_owned(),
        ];

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "unapproved_pr_ids not bound to the merged-PR gap set must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains("unapproved_pr_ids"),
            "detail names the field: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-17 P2 (Finding 1, positive): the untampered pack's genuine
    /// measurement fields all reconcile — the new arithmetic/list checks must not
    /// reject a clean pack.
    #[test]
    fn verify_allows_consistent_measurement_fields() {
        let pack = assemble_cc81();
        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "clean measurement fields must pass Integrity: {}",
            report.integrity.detail
        );
    }

    /// Codex round-17 P2 (Finding 2): a `review_coverage` section with ZERO
    /// approval-link rows (merged PRs, no approving reviews → 0% coverage) still
    /// carries the measurement when assembled; deleting it (`measurement: null`)
    /// must FAIL Integrity — an absent measurement is a defect on EVERY
    /// `review_coverage` section, not only when rows are present.
    #[test]
    fn verify_fails_when_zero_coverage_measurement_absent() {
        use super::fixture::pr_with_merge_time;
        // One PR merged in-window with no approving review → empty coverage rows,
        // measurement present (coverage 0/1 = 0.0), one merged-PR gap.
        let records = vec![pr_with_merge_time(
            "project:v1:prZero",
            "2026-03-15T08:00:00Z", // updated_at -> Task valid_time (in window)
            "2026-03-15T12:00:00Z", // merged_at -> merge time (in window)
            "cZero",
        )];
        let mut pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");

        // Baseline: the 0%-coverage pack has an empty coverage row set, a present
        // measurement, and a merged-PR gap — and it passes Integrity.
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        assert!(
            rc.records.is_empty(),
            "0% coverage has no approval-link rows"
        );
        let m = rc.measurement.as_ref().expect("measurement present");
        assert_eq!(m.merged_pr_count, 1);
        assert_eq!(m.approved_pr_count, 0);
        assert!(
            pack.gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"
                    && g.record_ids.contains(&"project:v1:prZero".to_owned())),
            "the merged-unapproved PR gap is present: gaps={:?}",
            pack.gaps
        );
        assert!(
            verify_pack(&pack).integrity.passed,
            "the untampered 0%-coverage pack passes Integrity: {}",
            verify_pack(&pack).integrity.detail
        );

        // Delete the measurement (and clear its record_count, which is already 0)
        // to simulate an artifact stripped of its coverage result.
        let rc_mut = pack
            .sections
            .iter_mut()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        rc_mut.measurement = None;

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a review_coverage section with no measurement must fail Integrity even with no rows"
        );
        assert!(
            report.integrity.detail.contains("measurement"),
            "detail names the missing measurement: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-15 P2 (Finding 2): Window-consistency must validate the
    /// manifest window bounds THEMSELVES — the same parseable + non-reversed rule
    /// `assemble_pack` enforces — even for a vacuous pack with no section rows and
    /// no timestamped gaps. A hand-edited reversed manifest window (`from` >= `to`)
    /// that `assemble_pack` would reject must FAIL verify's Window-consistency, not
    /// pass vacuously.
    #[test]
    fn verify_fails_when_manifest_window_is_reversed_even_when_empty() {
        let records = build_seed_records();
        let catalog = load_default_catalog();
        // Empty window: no in-window section rows.
        let empty = Window {
            from: "2026-01-01T00:00:00Z".to_owned(),
            to: "2026-01-02T00:00:00Z".to_owned(),
        };
        let mut pack = assemble_pack(&records, &catalog, "CC8.1", &empty, 1.0, "v", None).unwrap();
        // Isolate the new check: no section rows, no gaps at all, so only the
        // manifest-window validity check can fail Window-consistency.
        for s in &mut pack.sections {
            assert!(s.records.is_empty(), "empty window has no section rows");
        }
        pack.gaps.clear();

        // Baseline: the valid empty window still passes verify's Window-consistency.
        assert!(
            verify_pack(&pack).window_consistency.passed,
            "a valid vacuous empty-window pack passes Window-consistency: {}",
            verify_pack(&pack).window_consistency.detail
        );

        // Reverse the manifest window (from >= to): assemble would reject this.
        pack.manifest.window = Window {
            from: "2026-01-02T00:00:00Z".to_owned(),
            to: "2026-01-01T00:00:00Z".to_owned(),
        };
        let report = verify_pack(&pack);
        assert!(
            !report.window_consistency.passed,
            "a reversed manifest window must fail Window-consistency even with no rows"
        );
        assert!(!report.ok, "overall verdict fails");

        // An unparseable window bound must also fail.
        let mut bad = assemble_pack(&records, &catalog, "CC8.1", &empty, 1.0, "v", None).unwrap();
        bad.gaps.clear();
        bad.manifest.window.from = "not-a-time".to_owned();
        assert!(
            !verify_pack(&bad).window_consistency.passed,
            "an unparseable manifest window bound must fail Window-consistency"
        );
    }

    /// Codex round-10 P1: a secret injected into a `diagnostics[*].detail` (a
    /// non-record field) must FAIL Safety while Integrity stays green.
    #[test]
    fn verify_fails_on_secret_in_diagnostic_detail() {
        let mut pack = assemble_cc81();
        assert!(verify_pack(&pack).safety.passed, "baseline passes");
        assert!(!pack.diagnostics.is_empty(), "seed pack has diagnostics");

        pack.diagnostics[0].detail = format!("tampered diagnostic {INJECTED_SECRET}");

        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "Integrity still passes: {}",
            report.integrity.detail
        );
        assert!(
            !report.safety.passed,
            "Safety must fail on a secret in diagnostics[*].detail"
        );
        assert!(!report.ok, "overall verdict fails");
        assert!(
            report.safety.detail.contains("diagnostics"),
            "detail names WHERE: {}",
            report.safety.detail
        );
        assert!(
            !report.safety.detail.contains(INJECTED_SECRET),
            "detail must never leak the secret value: {}",
            report.safety.detail
        );
    }

    /// Codex round-11 Finding A: `assemble_pack` must run the SAME whole-artifact
    /// safety scan as `verify_pack`, so a secret echoed into a NON-record field —
    /// a malicious `--catalog` control title copied into `manifest.control_title`
    /// — fails the ASSEMBLED safety verdict, not only the per-record scan. Before
    /// the fix `assemble_pack` scanned only scrubbed section rows, so the secret
    /// was serialized to stdout with `verdicts.safety.passed == true`.
    #[test]
    fn assemble_runs_whole_artifact_safety_over_control_title() {
        let records = build_seed_records();
        let mut catalog = load_default_catalog();
        catalog.controls[0].title = format!("Change Management {INJECTED_SECRET}");
        let pack = assemble_pack(&records, &catalog, "CC8.1", &win(), 1.0, "test-0.0.0", None)
            .expect("assembles");
        assert!(
            !pack.verdicts.safety.passed,
            "assembled safety must fail on a secret in manifest.control_title"
        );
        assert!(
            !pack.verdicts.ok,
            "overall assemble verdict fails on the secret"
        );
        assert!(
            pack.verdicts.safety.detail.contains("control_title"),
            "detail names WHERE: {}",
            pack.verdicts.safety.detail
        );
        assert!(
            !pack.verdicts.safety.detail.contains(INJECTED_SECRET),
            "detail must never leak the secret value: {}",
            pack.verdicts.safety.detail
        );
    }

    /// Codex round-11 Finding A (positive): the clean scrubbed seed pack still
    /// passes the whole-artifact safety scan at ASSEMBLE time (no false positive
    /// on legitimate high-entropy hex).
    #[test]
    fn assemble_clean_pack_passes_whole_artifact_safety() {
        let pack = assemble_cc81();
        assert!(
            pack.verdicts.safety.passed,
            "clean assembled pack passes whole-artifact safety: {}",
            pack.verdicts.safety.detail
        );
    }

    /// Codex round-11 Finding C + round-18 Finding 2: a merged PR approved solely
    /// via a `REFERENCES_TASK` edge must carry that edge AND its source approving
    /// review as PRESENT, hashed, citable pack content backing the coverage
    /// measurement — not an absent relationship offline verify and consumers
    /// cannot substantiate. The three seed approving links (rv01->pr01,
    /// rv02->pr02, rv03->pr03) and their three source review nodes land in the
    /// `review_coverage` section, are scrubbed + hashed, counted in the manifest,
    /// canonically ordered, and (edges only) referenced by the measurement.
    #[test]
    fn coverage_link_edges_are_included_hashed_and_substantiated() {
        let pack = assemble_cc81();
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");

        // The three approving REFERENCES_TASK edges AND their three source review
        // nodes are present as section rows (round-18: source nodes co-located).
        assert_eq!(
            rc.record_count, 6,
            "the three approving link edges plus their three source review nodes are included"
        );
        assert_eq!(rc.records.len(), 6);
        let mut edge_ids: Vec<String> = Vec::new();
        let mut node_ids: Vec<String> = Vec::new();
        for br in &rc.records {
            match &br.record {
                GraphRecord::Edge {
                    label,
                    source,
                    target,
                    ..
                } => {
                    assert_eq!(label.as_str(), "REFERENCES_TASK");
                    assert!(
                        ["project:v1:rv01", "project:v1:rv02", "project:v1:rv03"]
                            .contains(&source.as_str()),
                        "source is an approving review: {source}"
                    );
                    assert!(
                        ["project:v1:pr01", "project:v1:pr02", "project:v1:pr03"]
                            .contains(&target.as_str()),
                        "target is an included merged PR: {target}"
                    );
                    edge_ids.push(br.record.id().to_owned());
                }
                GraphRecord::Node { .. } => {
                    assert!(
                        is_approving_review(&br.record),
                        "co-located node is an approving review: {:?}",
                        br.record
                    );
                    assert!(
                        ["project:v1:rv01", "project:v1:rv02", "project:v1:rv03"]
                            .contains(&br.record.id()),
                        "co-located review node is a seed approving review: {}",
                        br.record.id()
                    );
                    node_ids.push(br.record.id().to_owned());
                }
                GraphRecord::Tombstone { .. } => panic!("no tombstones in review_coverage"),
            }
            // The row hash matches a recompute over its scrubbed form (hashed).
            let recomputed =
                blake3::hash(serde_json::to_string(&br.record).unwrap().as_bytes()).to_string();
            assert_eq!(br.hash, recomputed, "row is hashed over scrubbed form");
        }
        assert_eq!(edge_ids.len(), 3, "three link edges");
        assert_eq!(node_ids.len(), 3, "three source review nodes");

        // Canonically ordered by (valid_time, record_id).
        for pair in rc.records.windows(2) {
            assert!(section_sort_key(&pair[0].record) <= section_sort_key(&pair[1].record));
        }

        // The measurement references ONLY the included link EDGES (never the source
        // review nodes) so a consumer can trace approved_pr_count to hashed records.
        let m = rc.measurement.as_ref().expect("measurement present");
        edge_ids.sort();
        assert_eq!(
            m.approval_link_edge_ids, edge_ids,
            "measurement cites exactly the included coverage-link edge IDs"
        );
        assert_eq!(m.approved_pr_count, 3);

        // Manifest counts include the edges (trust class `other`, REFERENCES_TASK
        // tuple) and the co-located review nodes (trust class `project_state`,
        // Review tuple; the three approving reviews also appear in the mapped
        // `reviews` section, so their project_state/Review counts legitimately
        // include both appearances — round-18 Finding 2).
        assert_eq!(
            pack.manifest.included_record_counts.get("other").copied(),
            Some(3),
            "manifest included_record_counts counts the 3 link edges"
        );
        assert_eq!(
            pack.manifest
                .tuple_counts
                .get("REFERENCES_TASK/v1")
                .copied(),
            Some(3),
            "manifest tuple_counts counts the 3 REFERENCES_TASK edges"
        );

        // Offline verify substantiates coverage: integrity re-hashes the edges,
        // window-consistency accepts them (stamped with the review's valid time),
        // safety passes.
        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "integrity: {}",
            report.integrity.detail
        );
        assert!(
            report.window_consistency.passed,
            "window: {}",
            report.window_consistency.detail
        );
        assert!(report.safety.passed, "safety: {}", report.safety.detail);
        assert!(report.ok, "verify substantiates the pack: {report:?}");
    }

    /// Codex round-18 P2 (Finding 1): a custom catalog mapping `review_coverage`
    /// as OPTIONAL emits NO `merged_pr_without_approving_review` gaps (they are
    /// gated on required review/PR evidence), yet `assemble_pack` still fills the
    /// measurement's `unapproved_pr_ids`. The round-17 `unapproved_pr_ids == gap
    /// set` binding must therefore be gated on the `review_coverage` verdict being
    /// applicable; an optional-coverage pack with an unapproved in-window merged PR
    /// must verify clean against its OWN `verify_pack` instead of failing Integrity.
    #[test]
    fn optional_review_coverage_pack_with_unapproved_pr_self_verifies() {
        use super::fixture::pr;
        let catalog = parse_catalog(
            r#"{
                "catalog_id": "custom",
                "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
                "controls": [
                    { "control_id": "OPTCOV", "title": "optional coverage", "evidence_classes": [
                        { "class": "review_coverage", "requirement": "optional" }
                    ] }
                ]
            }"#,
        )
        .expect("custom catalog parses");
        // One merged, in-window, UNAPPROVED PR.
        let records = vec![pr("project:v1:prU", "2026-03-15T12:00:00Z", "cU")];
        let pack =
            assemble_pack(&records, &catalog, "OPTCOV", &win(), 1.0, "v", None).expect("assembles");

        // The verdict is neutral (optional coverage never gates), and NO
        // merged_pr_without_approving_review gap exists.
        assert!(!pack.verdicts.review_coverage.applicable);
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "merged_pr_without_approving_review"),
            "optional-coverage control emits no merged-PR gap: {:?}",
            pack.gaps
        );
        // Yet the measurement still records the unapproved PR.
        let m = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("measurement");
        assert_eq!(m.unapproved_pr_ids, vec!["project:v1:prU".to_owned()]);

        // The freshly assembled pack must verify clean against its OWN verify_pack.
        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "optional-coverage pack integrity: {}",
            report.integrity.detail
        );
        assert!(
            report.ok,
            "optional-coverage pack must self-verify: {report:?}"
        );
    }

    /// Codex round-18 P2 (Finding 2): a custom catalog mapping `review_coverage`
    /// (required) WITHOUT a `reviews` section still emits the coverage
    /// `REFERENCES_TASK` link edges; their source approving-review NODES must be
    /// co-located in the `review_coverage` section so verify can resolve every edge
    /// endpoint offline. A freshly assembled approved-PR pack must verify clean.
    #[test]
    fn review_coverage_without_reviews_section_includes_source_review_and_self_verifies() {
        use super::fixture::{pr, references_task, review};
        let catalog = parse_catalog(
            r#"{
                "catalog_id": "custom",
                "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
                "controls": [
                    { "control_id": "RCOV", "title": "coverage only", "evidence_classes": [
                        { "class": "review_coverage", "requirement": "required" }
                    ] }
                ]
            }"#,
        )
        .expect("custom catalog parses");
        // Merged in-window PR approved by an in-window review submitted before merge.
        let records = vec![
            pr("project:v1:prA", "2026-03-15T12:00:00Z", "cA"),
            review("project:v1:rvA", "2026-03-15T08:00:00Z", "approved"),
            references_task("project:v1:rvA", "project:v1:prA"),
        ];
        let pack =
            assemble_pack(&records, &catalog, "RCOV", &win(), 1.0, "v", None).expect("assembles");

        // No reviews section is mapped by this control.
        assert!(
            pack.sections.iter().all(|s| s.class != "reviews"),
            "catalog maps no reviews section"
        );

        // The approving review NODE is co-located in the review_coverage section
        // alongside the cited link edge.
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        assert!(
            rc.records
                .iter()
                .any(|br| br.record.id() == "project:v1:rvA"
                    && matches!(&br.record, GraphRecord::Node { .. })),
            "the source approving review node is included in review_coverage: {:?}",
            rc.records
                .iter()
                .map(|br| br.record.id().to_owned())
                .collect::<Vec<_>>()
        );
        assert!(
            rc.records
                .iter()
                .any(|br| matches!(&br.record, GraphRecord::Edge { label, .. }
                if label.as_str() == "REFERENCES_TASK")),
            "the cited coverage link edge is present"
        );

        // The freshly assembled pack must verify clean against its OWN verify_pack.
        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "coverage-only pack integrity: {}",
            report.integrity.detail
        );
        assert!(report.ok, "coverage-only pack must self-verify: {report:?}");
    }

    /// Codex round-19 P1: `assemble_pack` counts a coverage edge only when the
    /// SOURCE approving review's resolved valid time is AT OR BEFORE the target
    /// PR's `merged_at` (the at-or-before-merge gate, round-9). `verify_pack`'s
    /// endpoint check only proved a PRESENT target maps to `PullRequests`, so a
    /// tampered pack could move an included review's valid time to AFTER the
    /// merge (still in-window), recompute its row hash, and turn a post-merge
    /// approval into apparent coverage. verify must re-enforce the gate for
    /// PRESENT targets.
    #[test]
    fn verify_fails_when_coverage_source_review_is_post_merge() {
        let mut pack = assemble_cc81();
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline pack passes Integrity"
        );
        // pr01 merged_at == 2026-03-03T12:00:00Z; rv01 approves it at 08:00
        // (before merge). Move rv01's valid time to 20:00 the same day: still
        // in-window, but now AFTER pr01's merge time — a post-merge approval.
        let post_merge = "2026-03-03T20:00:00Z";
        let mut touched = false;
        for section in &mut pack.sections {
            for br in &mut section.records {
                if br.record.id() == "project:v1:rv01" {
                    if let GraphRecord::Node { valid_time, .. } = &mut br.record {
                        *valid_time = Some(post_merge.to_owned());
                    }
                    br.hash = blake3::hash(serde_json::to_string(&br.record).unwrap().as_bytes())
                        .to_string();
                    touched = true;
                }
            }
            section
                .records
                .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
        }
        assert!(touched, "rv01 is present as a pack row to tamper");

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a coverage edge whose source review approves AFTER the target's merge \
             must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains("project:v1:rv01")
                && report.integrity.detail.contains("project:v1:pr01"),
            "detail names the offending edge endpoints: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-19 P1: assemble counts a coverage edge only when its target is
    /// a PR MERGED IN-WINDOW (`merged_pr_ids`, keyed on `merged_at`). A PR whose
    /// `valid_time` is in-window (so it rides the `pull_requests` section) but
    /// whose `merged_at` is OUT of window is present yet not merged in-window; a
    /// tampered pack could re-point a coverage edge at it and recompute
    /// hashes/counts. verify must fail such a PRESENT-but-not-merged-in-window
    /// target.
    #[test]
    fn verify_fails_when_coverage_target_present_but_not_merged_in_window() {
        use super::fixture::{pr, pr_with_merge_time, references_task, review};
        let catalog = parse_catalog(
            r#"{
                "catalog_id": "custom",
                "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
                "controls": [
                    { "control_id": "RCOV", "title": "pr coverage", "evidence_classes": [
                        { "class": "pull_requests", "requirement": "required" },
                        { "class": "review_coverage", "requirement": "required" }
                    ] }
                ]
            }"#,
        )
        .expect("custom catalog parses");
        let records = vec![
            // Approved, merged in-window PR -> yields one coverage edge.
            pr("project:v1:pr01", "2026-03-15T12:00:00Z", "c01"),
            review("project:v1:rv01", "2026-03-15T08:00:00Z", "approved"),
            references_task("project:v1:rv01", "project:v1:pr01"),
            // PRESENT in the pack (valid_time in-window) but merged OUT of window
            // (merged_at in February): not a merged-in-window PR.
            pr_with_merge_time(
                "project:v1:prZ",
                "2026-03-20T12:00:00Z",
                "2026-02-15T12:00:00Z",
                "cZ",
            ),
        ];
        let mut pack =
            assemble_pack(&records, &catalog, "RCOV", &win(), 1.0, "v", None).expect("assembles");
        assert!(
            verify_pack(&pack).integrity.passed,
            "baseline custom pack passes Integrity: {}",
            verify_pack(&pack).integrity.detail
        );
        assert!(
            pack.sections.iter().any(|s| s.class == "pull_requests"
                && s.records
                    .iter()
                    .any(|br| br.record.id() == "project:v1:prZ")),
            "prZ is present in the pull_requests section"
        );

        // Re-point the sole coverage edge's target to the present-but-not-
        // merged-in-window prZ, recompute row hash + section + manifest counts.
        let bad = stamp_edge_valid_time(
            references_task("project:v1:rv01", "project:v1:prZ"),
            "2026-03-15T08:00:00Z",
        );
        let (old_id, new_id) = swap_one_coverage_row(&mut pack, bad);
        let rc_idx = pack
            .sections
            .iter()
            .position(|s| s.class == "review_coverage")
            .unwrap();
        let m = pack.sections[rc_idx].measurement.as_mut().unwrap();
        m.approval_link_edge_ids.retain(|id| id != &old_id);
        m.approval_link_edge_ids.push(new_id.clone());
        m.approval_link_edge_ids.sort();

        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a coverage edge whose present target is not merged in-window must fail Integrity"
        );
        assert!(
            report.integrity.detail.contains(&new_id)
                && report.integrity.detail.contains("project:v1:prZ"),
            "detail names the offending edge and its target: {}",
            report.integrity.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    /// Codex round-19 P1 (positive): the round-16/18 ABSENT-target allowance must
    /// survive. A PR merged IN-window (`merged_at`) but whose Task `valid_time`
    /// is OUT of window is legitimately absent from every section (coverage
    /// windows on `merged_at`, the PR section on `valid_time`); its coverage
    /// edge's target is therefore absent, and an absent node cannot be
    /// merge-time-checked. verify must keep allowing it.
    #[test]
    fn verify_allows_coverage_edge_with_legitimately_absent_target() {
        use super::fixture::{pr_with_merge_time, references_task, review};
        let catalog = parse_catalog(
            r#"{
                "catalog_id": "custom",
                "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
                "controls": [
                    { "control_id": "RCOV", "title": "coverage only", "evidence_classes": [
                        { "class": "review_coverage", "requirement": "required" }
                    ] }
                ]
            }"#,
        )
        .expect("custom catalog parses");
        let records = vec![
            // merged_at in-window (March) but valid_time out of window (April):
            // legitimately absent target.
            pr_with_merge_time(
                "project:v1:prAbsent",
                "2026-04-20T12:00:00Z",
                "2026-03-15T12:00:00Z",
                "cAbs",
            ),
            review("project:v1:rvAbs", "2026-03-15T08:00:00Z", "approved"),
            references_task("project:v1:rvAbs", "project:v1:prAbsent"),
        ];
        let pack =
            assemble_pack(&records, &catalog, "RCOV", &win(), 1.0, "v", None).expect("assembles");

        // The target PR node is absent from every section.
        assert!(
            pack.sections.iter().all(|s| s
                .records
                .iter()
                .all(|br| br.record.id() != "project:v1:prAbsent")),
            "the out-of-window-valid_time PR is absent from every section"
        );
        // Yet its coverage edge is present.
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        assert!(
            rc.records.iter().any(|br| matches!(&br.record,
                GraphRecord::Edge { target, .. } if target == "project:v1:prAbsent")),
            "the coverage edge to the absent merged-in-window PR is present"
        );

        let report = verify_pack(&pack);
        assert!(
            report.integrity.passed,
            "a coverage edge with a legitimately absent target must stay allowed: {}",
            report.integrity.detail
        );
        assert!(
            report.ok,
            "absent-target coverage edge self-verifies: {report:?}"
        );
    }

    /// Regenerates the committed integration fixture. Runs only when the
    /// `EG_REGEN_EVIDENCE_PACK_FIXTURE` env var is set; otherwise it is a no-op.
    #[test]
    fn regenerate_committed_fixture() {
        if std::env::var_os("EG_REGEN_EVIDENCE_PACK_FIXTURE").is_none() {
            return;
        }
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/evidence_pack");
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        std::fs::write(dir.join("seed.graph.jsonl"), seed_jsonl()).expect("write fixture");
    }
}
