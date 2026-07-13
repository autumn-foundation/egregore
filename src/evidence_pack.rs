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

/// Section-level disclaimer for the log-evidence classes (issue #340).
///
/// For the `error_signatures` / `occurrence_buckets` runtime classes: occurrence
/// counts reflect the scanned log sources as ingested, never guaranteed-complete
/// telemetry.
pub const LOG_SECTION_DISCLAIMER: &str = "rows are runtime observations parsed from ingested logs, never verified; occurrence counts reflect only the scanned log sources as recorded, not guaranteed-complete telemetry; absence of a signature is not proof the error did not occur";

/// Section-level disclaimer for the derived `remediation_links` class (#340).
///
/// Every link is a review LEAD, never a causal claim that the named commit fixed
/// the named error.
pub const REMEDIATION_SECTION_DISCLAIMER: &str = "rows are derived remediation LEADS (error signature -> resolved frame symbol -> changing commit), never proof that the commit fixed the error; a frame binding proves only that the frame NAMES the symbol, never fault";

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

// ─────────────────────────────────────────────────────────────────────────────
// Shared review-coverage derivation (issue #339)
//
// The single source of truth for per-PR review-coverage classification, consumed
// by BOTH `eg audit review-coverage` (#339) and #338's evidence-pack
// `review_coverage` section + `merged_pr_without_approving_review` gap. The two
// surfaces MUST NOT fork this logic (AC7): they call `derive_review_coverage`
// with the strictness options each surface documents. The evidence pack uses the
// lenient options (any approving in-window at-or-before-merge review counts,
// matching its established #338 semantics); the audit lane defaults to the strict
// options below.
// ─────────────────────────────────────────────────────────────────────────────

/// Strictness knobs for review-coverage classification (issue #339, AC).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewCoverageOptions {
    /// Require an approving review from a non-author identity (default on). With
    /// #335 identity nodes unavailable this compares the review's `author` login
    /// against the PR Task's `author` login; a missing login on either side
    /// degrades to an `identity_unavailable` sub-label and never fabricates a
    /// self-approval.
    pub require_non_author: bool,
    /// Require the approving review to be anchored at the PR's final head commit
    /// (default on). A review whose `review_commit_sha` differs from the Task's
    /// `head_sha` is `approval_stale_head` rather than covered; a review with no
    /// `review_commit_sha` (issue #334 anchor absent) degrades to an
    /// `approval_unanchored` sub-label and is never guessed to be stale. An
    /// anchored review whose PR carries no `head_sha` cannot be confirmed as
    /// reviewing the final head, so it degrades to `approval_stale_head` +
    /// a `head_sha_unavailable` sub-label rather than silently passing the check.
    pub require_final_head: bool,
}

impl Default for ReviewCoverageOptions {
    fn default() -> Self {
        Self {
            require_non_author: true,
            require_final_head: true,
        }
    }
}

/// The closed per-PR verdict-class set (issue #339, AC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReviewVerdict {
    /// An approving review from a non-author identity, anchored at the final head
    /// (subject to the enabled strictness knobs), valid at-or-before merge.
    Covered,
    /// Approved, but the approval's `review_commit_sha` differs from the PR's
    /// final `head_sha` (approved-then-force-pushed); only under
    /// `require_final_head`.
    ApprovalStaleHead,
    /// The only approving review(s) come from the PR author identity; only under
    /// `require_non_author` and only when both logins are present.
    SelfApprovedOnly,
    /// Merged with zero approving reviews.
    Uncovered,
}

impl ReviewVerdict {
    /// Every verdict class, in fixed order.
    pub const ALL: [Self; 4] = [
        Self::Covered,
        Self::ApprovalStaleHead,
        Self::SelfApprovedOnly,
        Self::Uncovered,
    ];

    /// Stable `snake_case` wire name.
    #[must_use]
    pub const fn as_wire(&self) -> &'static str {
        match self {
            Self::Covered => "covered",
            Self::ApprovalStaleHead => "approval_stale_head",
            Self::SelfApprovedOnly => "self_approved_only",
            Self::Uncovered => "uncovered",
        }
    }
}

/// One classified merged-in-window PR (issue #339, AC).
///
/// Carries its full citation set (PR Task ID + `system_native_id` +
/// `merge_commit_sha`; for covered rows also the approving review ID +
/// `review_commit_sha` + approver login/identity).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewCoverageRow {
    /// The PR Task record ID.
    pub pr_task_id: String,
    /// The source-system-native PR handle (`system_native_id`), when recorded.
    pub system_native_id: Option<String>,
    /// The PR's merge commit SHA (`merge_commit_sha`), when recorded.
    pub merge_commit_sha: Option<String>,
    /// The resolved merge time (`merged_at`) used to window the PR.
    pub merged_at: Option<String>,
    /// The closed verdict class.
    pub verdict: ReviewVerdict,
    /// Sorted, closed-set sub-labels (`identity_unavailable`, `approval_unanchored`).
    pub sub_labels: Vec<String>,
    /// The deciding approving review record ID (covered rows; else `None`).
    pub approving_review_id: Option<String>,
    /// The deciding approving review's anchored `review_commit_sha` (covered rows).
    pub review_commit_sha: Option<String>,
    /// The deciding approving reviewer login (covered rows).
    pub approver_login: Option<String>,
    /// The deciding approver `ExternalIdentity` record ID — always `None` until
    /// issue #335 lands identity nodes; the login above is the available signal.
    pub approver_identity_id: Option<String>,
}

/// One coverage-substantiating `REFERENCES_TASK` edge linking an approving review
/// to a covered merged-in-window PR. Consumed by the evidence pack to build its
/// `review_coverage` section rows (AC7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageSubstantiation {
    /// The `REFERENCES_TASK` edge record ID.
    pub edge_id: String,
    /// The approving review (edge source) record ID.
    pub review_id: String,
    /// The merged-in-window PR (edge target) record ID.
    pub pr_id: String,
    /// The approving review's resolved valid time. This is a CITED field —
    /// legitimate evidence of WHEN the review was submitted — but it may be
    /// BEFORE the reporting window `from` (a pre-window approval that gated a
    /// merge inside the window, Codex Finding A). It is therefore NOT used to
    /// stamp the coverage edge's window-relevant valid time; it is preserved as
    /// the edge's cited `author_time`.
    pub review_valid_time: String,
    /// The covered PR's `merged_at` — guaranteed parseable and inside the
    /// reporting window (the PR is in the merged-in-window set). This is the
    /// window-relevant valid time stamped onto the coverage edge / section row so
    /// `verify_pack`'s Window-consistency check holds even when the approving
    /// review was submitted before the window (Codex Finding A).
    pub merged_at: String,
}

/// The shared review-coverage derivation output (issue #339, AC7).
#[derive(Debug, Clone)]
pub struct ReviewCoverageDerivation {
    /// One classified row per merged-in-window PR, sorted by PR Task record ID.
    pub rows: Vec<ReviewCoverageRow>,
    /// Distinct merged-in-window PR Task record IDs, sorted.
    pub merged_pr_ids: Vec<String>,
    /// PR Task IDs whose verdict is `covered` under the supplied options.
    pub covered_pr_ids: BTreeSet<String>,
    /// PR Task IDs with at least one approving review resolving in-window at or
    /// before merge — the OPTION-INDEPENDENT lenient set the
    /// `merged_pr_without_approving_review` gap keys on (never the strict covered
    /// set, so a self-approved or stale-head PR is never mislabeled as having no
    /// approving review at all).
    pub any_approving_pr_ids: BTreeSet<String>,
    /// Coverage-substantiating edges for covered PRs, sorted by edge ID.
    pub coverage_substantiations: Vec<CoverageSubstantiation>,
    /// PR Task IDs that look merged (a `github_pr` Task carrying a
    /// `merge_commit_sha`) but have no window-resolvable `merged_at`, excluded
    /// from the merged set under a counted diagnostic, sorted.
    pub excluded_unresolvable_merge_time: Vec<String>,
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
    /// Derived, redaction-safe summary for the log-graph evidence classes
    /// (issue #340: `error_signatures` / `occurrence_buckets` /
    /// `remediation_links`). Absent on every other section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_summary: Option<LogEvidenceSummary>,
    /// BLAKE3 hash binding the derived `log_summary` into Integrity (issue #340).
    /// Present iff `log_summary` is present. `verify_pack` recomputes it and fails
    /// Integrity on any divergence, so a tampered summary value — an inflated
    /// `in_window_occurrences`, a swapped `template_hash`/`frame_chain_hash`, a
    /// forged remediation `commit_id`, or a rewritten exemplar handle — cannot ride
    /// the pack undetected. This mirrors how `review_coverage`'s `measurement` is
    /// bound: the recomputable fields (`error_signatures` template/frame-chain
    /// hashes + clipped span, `occurrence_buckets` totals) are additionally bound to
    /// their backing hashed section rows, while this whole-summary hash is the ONLY
    /// binding surface for the derived `remediation_links` join, which by design
    /// carries no backing hashed row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_summary_hash: Option<String>,
    /// Verbatim section-level disclaimer, when the class carries one (#118/#157).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disclaimer: Option<String>,
}

/// Derived, redaction-safe summary rows for a log-graph evidence section (#340).
///
/// Carried on the section alongside the scrubbed backing rows, analogous to
/// `review_coverage`'s `measurement`. Every field is an ID, a hash, a bounded
/// label, a clipped RFC3339 instant, or a count — never raw log or exemplar text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "log_section", rename_all = "snake_case")]
pub enum LogEvidenceSummary {
    /// `error_signatures`: one row per in-window `ErrorSignature`.
    ErrorSignatures {
        /// Signature rows, ordered by `signature_id`.
        signatures: Vec<ErrorSignatureRow>,
    },
    /// `occurrence_buckets`: per-signature in-window occurrence totals.
    OccurrenceBuckets {
        /// Per-signature totals, ordered by `signature_id`; each row's buckets
        /// ordered by hour (AC2 `(signature record_id, hour)`).
        signature_totals: Vec<SignatureOccurrenceTotal>,
    },
    /// `remediation_links`: derived `ErrorSignature -> Symbol -> Commit` leads.
    RemediationLinks {
        /// Link rows, ordered by `(signature_id, symbol_id, commit_id)`.
        links: Vec<RemediationLinkRow>,
    },
}

/// One `error_signatures` summary row (AC1): fingerprint identity, window-clipped
/// activity span, exemplar handles, and `FRAME_RESOLVES_TO` joins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorSignatureRow {
    /// Stable `log:v1:` signature record ID.
    pub signature_id: String,
    /// Closed severity class (`fatal` / `error` / `warn`).
    pub severity: String,
    /// BLAKE3 of the normalized template excerpt — a redaction-safe fingerprint,
    /// never the raw template text.
    pub template_hash: String,
    /// BLAKE3 of the captured backtrace frame chain, when the signature carried
    /// parseable frames; absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_chain_hash: Option<String>,
    /// First-seen valid time, clipped to the window lower bound.
    pub first_seen_in_window: String,
    /// Last-seen valid time, clipped to the window upper bound.
    pub last_seen_in_window: String,
    /// Content-addressed exemplar references (handle + hash only, never text),
    /// ordered by `(source_line, content_hash)`.
    pub exemplars: Vec<ExemplarHandle>,
    /// `FRAME_RESOLVES_TO` joins carrying the verbatim resolution label
    /// (issues #152/#134), ordered by `(frame_index, target_id)`.
    pub frame_resolutions: Vec<FrameResolutionJoin>,
}

/// A content-addressed exemplar reference (AC1): a `protected:v1:` handle plus
/// its BLAKE3 content hash.
///
/// NEVER carries exemplar/log text — the raw bytes are retrievable only from the
/// #60 protected store when captured; absence there is documented, never
/// fabricated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExemplarHandle {
    /// Content-addressed `protected:v1:<blake3>` handle for the exemplar bytes.
    pub protected_handle: String,
    /// BLAKE3 hex of the normalized exemplar content.
    pub content_hash: String,
    /// One-based source line the exemplar began on.
    pub source_line: u64,
}

/// A `FRAME_RESOLVES_TO` join propagated verbatim onto a signature row (AC1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameResolutionJoin {
    /// Zero-based backtrace frame index the edge resolved, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_index: Option<u32>,
    /// Verbatim closed-set resolution label (`resolved` / `ambiguous` /
    /// `path_only` / `unresolved`), when the edge carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Stable record ID the frame resolved to (Symbol / File / Diagnostic).
    pub target_id: String,
}

/// Per-signature in-window occurrence total (AC2): the sum over ONLY the buckets
/// whose hour intersects the window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureOccurrenceTotal {
    /// Stable `log:v1:` signature record ID.
    pub signature_id: String,
    /// Sum of `occurrence_count` over the in-window buckets only.
    pub in_window_occurrences: u64,
    /// The contributing buckets, ordered by hour.
    pub buckets: Vec<BucketCount>,
}

/// One occurrence bucket contributing to a signature's in-window total (AC2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BucketCount {
    /// Stable `log:v1:` bucket record ID.
    pub bucket_id: String,
    /// Hour-aligned bucket start (RFC3339 UTC).
    pub hour: String,
    /// Occurrences in this bucket.
    pub occurrence_count: u64,
}

/// A derived remediation LEAD (AC3): `ErrorSignature --FRAME_RESOLVES_TO-->
/// Symbol --CHANGED_IN--> Commit`, with any linked verification evidence. Never
/// a causal claim that the commit fixed the error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemediationLinkRow {
    /// Stable `log:v1:` signature record ID.
    pub signature_id: String,
    /// Stable record ID of the frame-resolved symbol.
    pub symbol_id: String,
    /// Stable record ID of the changing commit.
    pub commit_id: String,
    /// Valid time of the changing commit (>= the signature's window activity).
    pub commit_valid_time: String,
    /// Zero-based backtrace frame index the resolving edge named, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_index: Option<u32>,
    /// Verbatim `FRAME_RESOLVES_TO` resolution label, propagated (issues
    /// #152/#134), when the edge carried one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_resolution: Option<String>,
    /// Verification record IDs linked to the changing commit, sorted; empty when
    /// none linked.
    pub verification_ids: Vec<String>,
    /// Verbatim lead-not-proof disclaimer.
    pub disclaimer: String,
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
/// Stamps a coverage `REFERENCES_TASK` edge with its window-relevant valid time.
///
/// The edge represents the COVERAGE of a PR that merged inside the reporting
/// window, so its window-relevant `valid_time` is the PR's in-window `merged_at`
/// — never the approving review's own valid time, which for a pre-window approval
/// (Codex Finding A) is BEFORE the window `from` and would push the section row
/// outside `[from, to)`, failing `verify_pack`'s Window-consistency check. The
/// approving review's valid time is legitimate evidence and is preserved as the
/// edge's cited `author_time`.
fn stamp_edge_valid_time(
    mut edge: GraphRecord,
    merged_at: &str,
    review_valid_time: &str,
) -> GraphRecord {
    if let GraphRecord::Edge { temporal, .. } = &mut edge {
        *temporal = Some(TemporalMetadata {
            git_commit: String::new(),
            git_parent_commits: Vec::new(),
            valid_time: merged_at.to_owned(),
            author_time: Some(review_valid_time.to_owned()),
            observed_at: merged_at.to_owned(),
            valid_time_source: Some("pr_merged_at".to_owned()),
        });
    }
    edge
}

/// The PR Task's final head commit SHA (`head_sha`), when recorded.
const fn pr_head_sha(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            head_sha: Some(h), ..
        } if !h.is_empty() => Some(h.as_str()),
        _ => None,
    }
}

/// The PR Task's source-native handle (`system_native_id`), when recorded.
fn pr_system_native_id(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node {
            system_native_id: Some(s),
            ..
        } if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// The PR Task's merge commit SHA (`merge_commit_sha`), when recorded.
fn pr_merge_commit_sha(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node {
            merge_commit_sha: Some(m),
            ..
        } if !m.is_empty() => Some(m.clone()),
        _ => None,
    }
}

/// The GitHub author login (`author`) recorded on a Task or Review, when present.
const fn author_login(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            author: Some(a), ..
        } if !a.is_empty() => Some(a.as_str()),
        _ => None,
    }
}

/// A Review's anchored reviewed-commit SHA (`review_commit_sha`, issue #334),
/// when present.
const fn review_commit_sha_of(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node {
            review_commit_sha: Some(s),
            ..
        } if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// True when a record LOOKS like a merged GitHub PR: a `github_pr` Task carrying
/// a `merge_commit_sha`. Used to count PRs excluded from the merged set for
/// lacking a window-resolvable `merged_at` (never falling back to update time).
fn pr_looks_merged(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node {
            source_kind: Some(sk),
            merge_commit_sha: Some(mc),
            ..
        } if sk == "github_pr" && !mc.is_empty()
    )
}

/// Derives the shared per-PR review-coverage classification (issue #339, AC7).
///
/// Pure, deterministic, and byte-stable: no I/O, no wall clock, no network. This
/// is the single implementation BOTH `eg audit review-coverage` and #338's
/// evidence-pack `review_coverage` section / `merged_pr_without_approving_review`
/// gap call — the pack with lenient options, the audit lane with its strict
/// defaults — so the two surfaces can never diverge.
///
/// A PR is "merged in window" iff its MERGE time (`merged_at`, first-class since
/// #333) falls in the half-open window (never its Task `valid_time`, which the
/// importer stamps from `github_updated_at`). An approving review counts only when
/// it is a genuine approving PR review (`review_kind` allow-list + `approved`
/// state), references the PR via `REFERENCES_TASK`, and is valid AT OR BEFORE the
/// PR's merge time (a post-merge approval did not gate the merge). The review's
/// OWN position relative to the window does not gate it: an approval submitted
/// before the window `from` but at or before merge still counts (Codex Finding 2).
///
/// A merged PR whose `merged_at` is present but unparseable (or absent while the
/// PR otherwise looks merged) has no window-resolvable merge time and is excluded
/// under the counted `excluded_unresolvable_merge_time` diagnostic — never
/// silently dropped and never windowed on update time (Codex Finding 1).
#[must_use]
#[allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::struct_excessive_bools
)]
pub fn derive_review_coverage(
    records: &[GraphRecord],
    window: &Window,
    options: ReviewCoverageOptions,
) -> ReviewCoverageDerivation {
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // Merged-in-window PRs, keyed on `merged_at` (round-5 #333 merge-time gate).
    let mut merged_pr_ids: Vec<String> = Vec::new();
    let mut excluded_unresolvable_merge_time: Vec<String> = Vec::new();
    for record in records {
        match merged_pr_merge_time(record) {
            Some(merge_time) if in_window(merge_time, window) => {
                merged_pr_ids.push(record.id().to_owned());
            }
            // A RESOLVABLE (parseable) merge time that falls outside the window is
            // correctly excluded and NOT diagnosed.
            Some(merge_time) if parse_rfc3339(merge_time).is_some() => {}
            // A merged github_pr whose `merged_at` is present but UNPARSEABLE has
            // no window-resolvable merge time. It is excluded under a counted
            // diagnostic — never silently dropped as if it were merely out of
            // window (Codex Finding 1). `merged_pr_merge_time` only returns Some
            // for a github_pr with a non-empty `merged_at`, so this record already
            // qualifies as a merged PR with an unresolvable merge time.
            Some(_) => {
                excluded_unresolvable_merge_time.push(record.id().to_owned());
            }
            None => {
                // A PR that LOOKS merged (carries a merge_commit_sha) but has no
                // `merged_at` at all is excluded under the same counted diagnostic
                // — never windowed on update time (round-5 P1).
                if pr_looks_merged(record) {
                    excluded_unresolvable_merge_time.push(record.id().to_owned());
                }
            }
        }
    }
    merged_pr_ids.sort();
    merged_pr_ids.dedup();
    excluded_unresolvable_merge_time.sort();
    excluded_unresolvable_merge_time.dedup();
    let merged_set: BTreeSet<&str> = merged_pr_ids.iter().map(String::as_str).collect();

    // Gather every approving-review -> merged-PR REFERENCES_TASK link, keyed by
    // target PR. Each entry records the edge, the review, and per-review checks.
    struct ApprovingLink<'a> {
        edge_id: &'a str,
        review_id: &'a str,
        review: &'a GraphRecord,
        review_valid_time: String,
    }
    let mut links_by_pr: BTreeMap<&str, Vec<ApprovingLink<'_>>> = BTreeMap::new();
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
        if label.as_str() != "REFERENCES_TASK" || !merged_set.contains(target.as_str()) {
            continue;
        }
        let Some(merged_at) = by_id
            .get(target.as_str())
            .and_then(|r| merged_pr_merge_time(r))
            .and_then(parse_rfc3339)
        else {
            continue;
        };
        let Some(review) = by_id.get(source.as_str()).copied() else {
            continue;
        };
        if !is_approving_review(review) {
            continue;
        }
        let Some(review_vt) = resolve_valid_time(review) else {
            continue;
        };
        // The reporting window bounds which PRs are IN SCOPE (via `merged_at`), NOT
        // which approvals count. An approving review submitted BEFORE the window
        // `from` but at or before the PR's merge legitimately gated that merge, so
        // the review's own position relative to `[from, to)` must not disqualify it
        // (Codex Finding 2). The only temporal gate is at-or-before `merged_at`.
        let Some(review_ts) = parse_rfc3339(&review_vt) else {
            continue;
        };
        if review_ts > merged_at {
            continue; // post-merge approval never gates the merge (round-9)
        }
        links_by_pr
            .entry(target.as_str())
            .or_default()
            .push(ApprovingLink {
                edge_id: record.id(),
                review_id: source.as_str(),
                review,
                review_valid_time: review_vt,
            });
    }

    let mut rows: Vec<ReviewCoverageRow> = Vec::new();
    let mut covered_pr_ids: BTreeSet<String> = BTreeSet::new();
    let mut any_approving_pr_ids: BTreeSet<String> = BTreeSet::new();
    let mut coverage_substantiations: Vec<CoverageSubstantiation> = Vec::new();

    for pr_id in &merged_pr_ids {
        let task = by_id.get(pr_id.as_str()).copied();
        let system_native_id = task.and_then(pr_system_native_id);
        let merge_commit_sha = task.and_then(pr_merge_commit_sha);
        let merged_at = task
            .and_then(|t| merged_pr_merge_time(t))
            .map(str::to_owned);
        let task_author = task.and_then(author_login);
        let head_sha = task.and_then(pr_head_sha);

        let mut links = links_by_pr.remove(pr_id.as_str()).unwrap_or_default();
        // Deterministic evaluation order.
        links.sort_by(|a, b| a.review_id.cmp(b.review_id));

        if !links.is_empty() {
            any_approving_pr_ids.insert(pr_id.clone());
        }

        // Per-review predicates under the enabled knobs.
        struct Eval<'a> {
            link: &'a ApprovingLink<'a>,
            qualifies: bool,
            is_self: bool,
            head_stale: bool,
            head_unavailable: bool,
            author_ok: bool,
            identity_unavailable: bool,
            unanchored: bool,
        }
        let evals: Vec<Eval<'_>> = links
            .iter()
            .map(|link| {
                let rcs = review_commit_sha_of(link.review);
                let rev_author = author_login(link.review);
                let identity_unavailable =
                    options.require_non_author && (rev_author.is_none() || task_author.is_none());
                let is_self = options.require_non_author
                    && matches!((rev_author, task_author), (Some(a), Some(b)) if a == b);
                // Non-author check passes when the knob is off, when identity is
                // unavailable (degrade — never fabricate self-approval), or when
                // the logins genuinely differ.
                let author_ok = !options.require_non_author || identity_unavailable || !is_self;
                let unanchored = options.require_final_head && rcs.is_none();
                // Head check: stale only when anchored AND the anchor differs from
                // the final head; an unanchored review degrades (never guessed
                // stale).
                let head_stale = options.require_final_head
                    && matches!((rcs, head_sha), (Some(r), Some(h)) if r != h);
                // Head unavailable: an ANCHORED approval (has a `review_commit_sha`)
                // whose PR carries no `head_sha` (e.g. a pre-#333 or partial import)
                // cannot be confirmed as reviewing the final head. Under
                // `--require-final-head` this must NOT silently pass — otherwise the
                // approval is classified `covered`, overstating coverage exactly
                // when the final head is unverifiable (Codex P2). It degrades to
                // `approval_stale_head` + a reported `head_sha_unavailable`
                // sub-label, never a guess. Guarded on `rcs.is_some()` so an
                // unanchored review keeps its `approval_unanchored` degradation
                // (no double-classification). When the knob is OFF (the lenient
                // #338 pack path) the head is not checked at all, preserving the
                // pack numbers and the AC7 zero-divergence fixture.
                let head_unavailable =
                    options.require_final_head && rcs.is_some() && head_sha.is_none();
                let head_ok = !head_stale && !head_unavailable;
                Eval {
                    link,
                    qualifies: author_ok && head_ok,
                    is_self,
                    head_stale,
                    head_unavailable,
                    author_ok,
                    identity_unavailable,
                    unanchored,
                }
            })
            .collect();

        // Every approving link substantiates the coverage of a covered PR; a PR is
        // covered iff at least one link qualifies under the enabled knobs.
        let verdict;
        let mut deciding: Option<&Eval<'_>> = None;
        if let Some(best) = evals
            .iter()
            .filter(|e| e.qualifies)
            // Prefer a fully non-degraded qualifying review; tiebreak by review id.
            .min_by(|a, b| {
                let da = usize::from(a.identity_unavailable) + usize::from(a.unanchored);
                let db = usize::from(b.identity_unavailable) + usize::from(b.unanchored);
                da.cmp(&db)
                    .then_with(|| a.link.review_id.cmp(b.link.review_id))
            })
        {
            verdict = ReviewVerdict::Covered;
            deciding = Some(best);
            covered_pr_ids.insert(pr_id.clone());
            for link in &links {
                coverage_substantiations.push(CoverageSubstantiation {
                    edge_id: link.edge_id.to_owned(),
                    review_id: link.review_id.to_owned(),
                    pr_id: pr_id.clone(),
                    review_valid_time: link.review_valid_time.clone(),
                    // Every covered PR is in `merged_pr_ids`, so its `merged_at`
                    // is present, parseable, and in-window.
                    merged_at: merged_at.clone().unwrap_or_default(),
                });
            }
        } else if links.is_empty() {
            verdict = ReviewVerdict::Uncovered;
        } else if let Some(stale) = evals
            .iter()
            .filter(|e| e.author_ok && (e.head_stale || e.head_unavailable))
            .min_by(|a, b| a.link.review_id.cmp(b.link.review_id))
        {
            // A genuine (non-author / degraded) reviewer approved a non-final head,
            // or an anchored approval whose PR head_sha is missing so the final
            // head cannot be confirmed (`head_unavailable`).
            verdict = ReviewVerdict::ApprovalStaleHead;
            deciding = Some(stale);
        } else if let Some(self_only) = evals
            .iter()
            .filter(|e| e.is_self)
            .min_by(|a, b| a.link.review_id.cmp(b.link.review_id))
        {
            verdict = ReviewVerdict::SelfApprovedOnly;
            deciding = Some(self_only);
        } else {
            // Residual (e.g. self + stale with no independent reviewer): the
            // self-approval is the dominant defect.
            verdict = ReviewVerdict::SelfApprovedOnly;
            deciding = evals
                .iter()
                .min_by(|a, b| a.link.review_id.cmp(b.link.review_id));
        }

        let mut sub_labels: BTreeSet<String> = BTreeSet::new();
        if let Some(e) = deciding {
            if e.identity_unavailable {
                sub_labels.insert("identity_unavailable".to_owned());
            }
            if e.unanchored {
                sub_labels.insert("approval_unanchored".to_owned());
            }
            if e.head_unavailable {
                sub_labels.insert("head_sha_unavailable".to_owned());
            }
        }

        let (approving_review_id, review_commit_sha, approver_login) = match (verdict, deciding) {
            (ReviewVerdict::Covered, Some(e)) => (
                Some(e.link.review_id.to_owned()),
                review_commit_sha_of(e.link.review).map(str::to_owned),
                author_login(e.link.review).map(str::to_owned),
            ),
            _ => (None, None, None),
        };

        rows.push(ReviewCoverageRow {
            pr_task_id: pr_id.clone(),
            system_native_id,
            merge_commit_sha,
            merged_at,
            verdict,
            sub_labels: sub_labels.into_iter().collect(),
            approving_review_id,
            review_commit_sha,
            approver_login,
            approver_identity_id: None,
        });
    }

    rows.sort_by(|a, b| a.pr_task_id.cmp(&b.pr_task_id));
    coverage_substantiations.sort_by(|a, b| a.edge_id.cmp(&b.edge_id));

    ReviewCoverageDerivation {
        rows,
        merged_pr_ids,
        covered_pr_ids,
        any_approving_pr_ids,
        coverage_substantiations,
        excluded_unresolvable_merge_time,
    }
}

/// Scrubs, hashes, and canonically orders a set of records for a section.
///
/// Applies the shared bundle scrub (prose/handles/PII) and, for log-graph nodes,
/// the pack-side log-aware text scrub (issue #340, Codex round-4 P2), so no raw
/// runtime log content — the normalized `template_excerpt`, exemplar
/// `event_excerpt`, or backtrace-frame path text — rides a section's hashed rows.
/// `bundle::scrub_record` never touches the `log` payload, so this is the pack's
/// only defense for those fields.
fn build_section_records(records: Vec<GraphRecord>) -> Vec<BundleRecord> {
    let mut rows: Vec<BundleRecord> = records
        .into_iter()
        .map(|r| {
            let scrubbed = scrub_log_node_text(scrub_record(r));
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

/// Pack-side, log-aware text scrub for one exported log node (issue #340, Codex
/// round-4 P2). Strips raw runtime log content from an `ErrorSignature` /
/// `LogEvent` node before it enters a hashed section row, WITHOUT weakening the
/// derived summary's Integrity binding:
///
/// * `ErrorSignature.template_excerpt` (the normalized log-line text) is REPLACED
///   by its own BLAKE3 fingerprint — the exact value the summary carries as
///   `template_hash`. `bind_error_signature_rows` then binds the summary's
///   `template_hash` to this stored fingerprint directly, so the per-node bind
///   still catches a forged fingerprint even with the whole-summary hash
///   recomputed, but no raw template text survives.
/// * Each backtrace `StackFrame`'s `module_path` / `file_path` (redaction-safe by
///   construction, but still frame text) is REPLACED by its BLAKE3 fingerprint,
///   preserving the `frame_chain_hash` fingerprint's discriminating power while
///   removing the readable path. `frame_index` / `line` (non-text) are retained.
/// * `LogEvent.event_excerpt` is REPLACED by its fingerprint (exemplars already
///   ride the summary as content-addressed handles, never as section text).
///
/// It also clears the free-text `summary` of a co-located `FRAME_RESOLVES_TO`
/// edge (issue #371, Codex P2 Safety/redaction): that required field is
/// synthesized safely by `eg resolve-frames`, but an older/hand-authored
/// importer could populate it with raw backtrace/log text, and neither
/// `bundle::scrub_record` (SECRET-redacts an edge summary only) nor `pack_safety`
/// (secret + Node-only scrubbed-field checks) strips non-secret text, so it would
/// otherwise ride the hashed section row while Safety still passes. The #371 bind
/// recomputes each row's `frame_resolution` / `frame_index` / endpoint IDs from
/// the edge's typed fields — never its summary — so clearing it is round-trip
/// safe. Centralized here because every co-located section row (including these
/// frame edges) flows through `build_section_records` -> `scrub_log_node_text`.
///
/// A no-op for every non-log node (`log: None`) and for `LogSource` /
/// `LogOccurrenceBucket` payloads, which carry no free-text field.
fn scrub_log_node_text(mut record: GraphRecord) -> GraphRecord {
    match &mut record {
        GraphRecord::Node {
            log: Some(payload), ..
        } => match payload.as_mut() {
            crate::ir::LogPayload::ErrorSignature(p) => {
                p.template_excerpt = blake3::hash(p.template_excerpt.as_bytes()).to_string();
                if let Some(frames) = p.frames.as_mut() {
                    redact_frame_text_in_place(frames);
                }
            }
            crate::ir::LogPayload::LogEvent(p) => {
                p.event_excerpt = blake3::hash(p.event_excerpt.as_bytes()).to_string();
            }
            crate::ir::LogPayload::LogSource(_) | crate::ir::LogPayload::LogOccurrenceBucket(_) => {
            }
        },
        GraphRecord::Edge {
            label: crate::ir::EdgeLabel::FrameResolvesTo,
            summary,
            ..
        } => {
            summary.clear();
        }
        _ => {}
    }
    record
}

/// Replaces each backtrace frame's `module_path` / `file_path` text with its
/// BLAKE3 fingerprint in place (issue #340, Codex round-4 P2). Shared by the
/// section-node scrub and the `frame_chain_hash` derivation so the exported node
/// and the summary hash bind against an identical, redaction-safe frame chain.
fn redact_frame_text_in_place(frames: &mut [crate::ir::StackFrame]) {
    for f in frames.iter_mut() {
        f.module_path = f
            .module_path
            .as_deref()
            .map(|m| blake3::hash(m.as_bytes()).to_string());
        f.file_path = f
            .file_path
            .as_deref()
            .map(|p| blake3::hash(p.as_bytes()).to_string());
    }
}

/// The redaction-safe frame chain a signature's `frame_chain_hash` binds: the
/// captured frames with their path text replaced by fingerprints (issue #340,
/// Codex round-4 P2). Derived from the ORIGINAL node, it is byte-identical to the
/// scrubbed frames the exported section node carries, so the summary's
/// `frame_chain_hash` and `bind_error_signature_rows`' node recompute agree.
fn redacted_frame_chain(frames: &[crate::ir::StackFrame]) -> Vec<crate::ir::StackFrame> {
    let mut cloned = frames.to_vec();
    redact_frame_text_in_place(&mut cloned);
    cloned
}

/// True when a section's evidence-class wire name is one of the log-graph classes
/// (issue #340): `error_signatures`, `occurrence_buckets`, `remediation_links`.
fn is_log_evidence_class(class: &str) -> bool {
    matches!(
        EvidenceClass::from_wire(class),
        Some(
            EvidenceClass::ErrorSignatures
                | EvidenceClass::OccurrenceBuckets
                | EvidenceClass::RemediationLinks
        )
    )
}

/// Canonical `(valid_time_or_empty, record_id)` sort key for a section row.
fn section_sort_key(record: &GraphRecord) -> (String, String) {
    (
        resolve_valid_time(record).unwrap_or_default(),
        record.id().to_owned(),
    )
}

// ── issue #340: log-graph incident evidence (CC7.x) ────────────────────────────

/// Hour width, in seconds, of a `LogOccurrenceBucket` (issue #320 emits `1h`
/// buckets; `bucket_width` is the constant `"1h"`).
const BUCKET_HOUR_SECONDS: i64 = 3600;

/// AC2 interval-intersection predicate for one occurrence bucket: the bucket's
/// hour `[bucket_start, bucket_start + 1h)` intersects the half-open window
/// `[from, to)`. A bucket straddling `from` is included WHOLE (documented; no
/// interpolation). This is DELIBERATELY NOT the point predicate every other
/// class uses (`from <= t < to`): a bucket whose hour began before `from` but
/// carries in-window occurrences must still count, so it is admitted whole and
/// its full count is summed.
fn bucket_hour_intersects_window(
    bucket_start: chrono::DateTime<chrono::FixedOffset>,
    from: chrono::DateTime<chrono::FixedOffset>,
    to: chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    let hour_end = bucket_start + chrono::Duration::seconds(BUCKET_HOUR_SECONDS);
    bucket_start < to && hour_end > from
}

/// The log payload of a node record, when it carries one.
fn node_log_payload(record: &GraphRecord) -> Option<&crate::ir::LogPayload> {
    match record {
        GraphRecord::Node { log: Some(p), .. } => Some(p.as_ref()),
        _ => None,
    }
}

/// True when a node has been superseded by a newer version (lifecycle filter):
/// such a record must not count as live evidence, mirroring the tombstone
/// exclusion `evidence_class_for_record` already applies to `Tombstone` records.
const fn is_superseded_node(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node { superseded_by: Some(s), .. } if !s.is_empty()
    )
}

/// Clips a valid-time instant to the window's lower bound (`max(ts, from)`),
/// returning the RFC3339 string to present. `from`/`to` are the canonical window
/// strings; `ts_parsed`/`from_parsed` the corresponding parsed instants.
fn clip_lower(
    ts: &str,
    ts_parsed: DateTimeFixed,
    from: &str,
    from_parsed: DateTimeFixed,
) -> String {
    if ts_parsed < from_parsed {
        from.to_owned()
    } else {
        ts.to_owned()
    }
}

/// Clips a valid-time instant to the window's upper bound (`min(ts, to)`).
fn clip_upper(ts: &str, ts_parsed: DateTimeFixed, to: &str, to_parsed: DateTimeFixed) -> String {
    if ts_parsed > to_parsed {
        to.to_owned()
    } else {
        ts.to_owned()
    }
}

/// Convenience alias for the parsed-instant type.
type DateTimeFixed = chrono::DateTime<chrono::FixedOffset>;

/// True when an edge label is one of the log-domain edges a concatenated
/// multi-scan graph duplicates (issue #340, Codex round-6 P2): a duplicate is an
/// exact copy (deterministic edge ID, identical endpoints/label), so deduping by
/// stable ID is loss-free.
fn is_log_domain_edge(label: &str) -> bool {
    matches!(
        label,
        "CAPTURED_FROM" | "FINGERPRINTED_AS" | "AGGREGATES" | "FRAME_RESOLVES_TO"
    )
}

/// Merges one RFC 3339 valid-time bound across a coalesced signature group to the
/// earliest (`earliest = true`) or latest value by parsed UTC INSTANT, returning
/// the winning raw RFC 3339 string. Mirrors `log_deltas::merge_bound` exactly:
/// comparison is by parsed instant (never raw string order — the same
/// cross-offset reason as classification), a parseable value always wins over an
/// unparseable one, an instant tie breaks lexically over the raw strings, and
/// only when NEITHER value parses does the lexically smallest/largest raw string
/// win. Both inputs originate from Egregore's own scanners and parse in practice.
fn merge_bound_pair(a: &str, b: &str, earliest: bool) -> String {
    match (parse_rfc3339(a), parse_rfc3339(b)) {
        (Some(pa), Some(pb)) => {
            let pick_b = if earliest {
                pb < pa || (pb == pa && b < a)
            } else {
                pb > pa || (pb == pa && b > a)
            };
            if pick_b { b.to_owned() } else { a.to_owned() }
        }
        // A parseable bound always beats an unparseable one.
        (Some(_), None) => a.to_owned(),
        (None, Some(_)) => b.to_owned(),
        // Nothing parseable: deterministic lexical fallback.
        (None, None) => {
            let pick_b = if earliest { b < a } else { b > a };
            if pick_b { b.to_owned() } else { a.to_owned() }
        }
    }
}

/// Merges `other`'s `ErrorSignature` valid-time span and occurrence count into the
/// accumulator node (issue #340, Codex round-6 P2). Earliest `first_seen`, latest
/// `last_seen` (by parsed UTC instant), summed `occurrence_count` — the exact
/// `log_deltas` cross-scan coalescing semantics. `template_excerpt` / `severity` /
/// `frames` are identity components (or identity-derived), so they are identical
/// across the group and left as the accumulator carries them. The node's
/// `valid_time` (which `resolve_valid_time` reports for window membership) is kept
/// consistent with the merged earliest `first_seen`.
fn merge_error_signature_into(acc: &mut GraphRecord, other: &GraphRecord) {
    let (o_first, o_last, o_count) = {
        let Some(crate::ir::LogPayload::ErrorSignature(o)) = node_log_payload(other) else {
            return;
        };
        (
            o.first_seen.clone(),
            o.last_seen.clone(),
            o.occurrence_count,
        )
    };
    let GraphRecord::Node {
        log: Some(payload),
        valid_time,
        temporal,
        ..
    } = acc
    else {
        return;
    };
    let crate::ir::LogPayload::ErrorSignature(a) = payload.as_mut() else {
        return;
    };
    a.first_seen = merge_bound_pair(&a.first_seen, &o_first, true);
    a.last_seen = merge_bound_pair(&a.last_seen, &o_last, false);
    a.occurrence_count = a.occurrence_count.saturating_add(o_count);
    let merged_first = a.first_seen.clone();
    // Keep the node's window-membership valid time aligned with the merged
    // earliest `first_seen` (scan-logs nodes stamp `valid_time` == `first_seen`).
    *valid_time = Some(merged_first.clone());
    if let Some(t) = temporal.as_mut()
        && !t.valid_time.is_empty()
    {
        t.valid_time = merged_first;
    }
}

/// Coalesces duplicate log-domain records by stable ID at ASSEMBLE time (issue
/// #340, Codex round-6 P2), mirroring the documented `query log-deltas` cross-scan
/// coalescing semantics so a concatenated multi-scan graph produces exactly ONE
/// row per stable log ID.
///
/// A `LogSource` is a NON-identity input for a signature — the `ErrorSignature`
/// stable ID is `(repository_id, fingerprint_algorithm, template, severity)` only
/// (a `LogOccurrenceBucket` ID, by contrast, IS source-aware since issue #361:
/// `(repository/signature/hour/width/SOURCE)`) — so a graph made
/// by concatenating several `scan-logs` outputs for one repo (a documented,
/// legitimate workflow) carries the SAME stable signature ID once per scan, while
/// distinct sources mint distinct bucket IDs. Without
/// coalescing, `assemble_pack` would emit one summary row per PHYSICAL record and
/// its own offline `verify_pack` (which requires exactly one row per signature)
/// would reject the freshly assembled pack. This restores the hard invariant that
/// every assembled pack passes its own verify.
///
/// Merge rules (match `log_deltas` exactly):
/// * `ErrorSignature` records sharing an ID → one node: earliest `first_seen`,
///   latest `last_seen` (by parsed UTC instant), summed `occurrence_count`.
/// * `LogOccurrenceBucket` records sharing an ID → deduped to the first
///   occurrence (issue #361, source-aware identity): bucket ID is now
///   `(repository/signature/hour/width/SOURCE)`, so a shared bucket ID means a
///   genuine rescan of identical bytes (identical content) — collapse it;
///   distinct sources mint distinct bucket IDs that survive and sum downstream.
/// * `LogSource` / `LogEvent` nodes and log-domain edges sharing an ID → deduped
///   to the first occurrence (an exact duplicate carries identical content).
///
/// Every NON-log record passes through unchanged, in order. Applied BEFORE window
/// filtering and summary building so first/last-seen clipping and window
/// membership use the merged extents. Deterministic: output preserves
/// first-occurrence order and every downstream section/summary re-sorts, so the
/// pack is byte-identical across runs regardless of input concatenation order. A
/// single-scan graph has no duplicate log IDs, so this is a no-op there and every
/// existing pack is unchanged.
fn coalesce_log_records(records: &[GraphRecord]) -> Vec<GraphRecord> {
    let mut out: Vec<GraphRecord> = Vec::with_capacity(records.len());
    let mut sig_slot: BTreeMap<&str, usize> = BTreeMap::new();
    let mut seen_dedup: BTreeSet<&str> = BTreeSet::new();
    for record in records {
        match record {
            GraphRecord::Node {
                id,
                log: Some(payload),
                ..
            } => match payload.as_ref() {
                crate::ir::LogPayload::ErrorSignature(_) => {
                    if let Some(&idx) = sig_slot.get(id.as_str()) {
                        merge_error_signature_into(&mut out[idx], record);
                    } else {
                        sig_slot.insert(id.as_str(), out.len());
                        out.push(record.clone());
                    }
                }
                crate::ir::LogPayload::LogOccurrenceBucket(_)
                | crate::ir::LogPayload::LogSource(_)
                | crate::ir::LogPayload::LogEvent(_) => {
                    // Dedup by record ID (issue #361): a shared bucket ID is a
                    // genuine rescan of identical bytes (source-aware identity),
                    // so collapse it rather than summing; distinct sources mint
                    // distinct bucket IDs that both survive and sum downstream.
                    if seen_dedup.insert(id.as_str()) {
                        out.push(record.clone());
                    }
                }
            },
            GraphRecord::Edge { id, label, .. } if is_log_domain_edge(label.as_str()) => {
                if seen_dedup.insert(id.as_str()) {
                    out.push(record.clone());
                }
            }
            other => out.push(other.clone()),
        }
    }
    out
}

/// Derives the sorted `FrameResolutionJoin` set for one signature from its
/// `FRAME_RESOLVES_TO` edges (issue #371) — the resolution label + `frame_index`
/// propagated verbatim (#152/#134), ordered by `(frame_index, target_id)`. This is
/// the SINGLE derivation shared by `build_error_signature_rows` (assemble, from the
/// whole-graph join index) and `bind_error_signature_rows` (verify, from the
/// co-located hashed edge rows), so a summary row's frame joins and their verify-time
/// recompute can never diverge.
fn frame_resolution_joins(edges: &[&GraphRecord]) -> Vec<FrameResolutionJoin> {
    let mut joins: Vec<FrameResolutionJoin> = edges
        .iter()
        .filter_map(|&edge| match edge {
            GraphRecord::Edge { target, .. } => Some(FrameResolutionJoin {
                frame_index: edge.frame_index(),
                resolution: edge.frame_resolution().map(|r| r.as_str().to_owned()),
                target_id: target.clone(),
            }),
            _ => None,
        })
        .collect();
    joins.sort_by(|a, b| {
        a.frame_index
            .cmp(&b.frame_index)
            .then_with(|| a.target_id.cmp(&b.target_id))
            // `resolution` is the final tiebreaker (issue #371, Codex round-7 P2):
            // `log_resolve` folds resolution into the FRAME_RESOLVES_TO edge id, so a
            // graph can carry two edges sharing (signature, frame_index, target) but
            // differing in resolution. Without this key those joins compare EQUAL and a
            // stable sort would preserve each caller's differing input order — assemble
            // (input edge order) and verify (canonically id-sorted section rows) would
            // then disagree and reject a freshly assembled pack. This makes the shared
            // derivation TOTAL and input-order-independent on BOTH paths.
            .then_with(|| a.resolution.cmp(&b.resolution))
    });
    joins
}

/// Builds the `error_signatures` summary rows (AC1) from the in-window signature
/// nodes plus the exemplar (`FINGERPRINTED_AS`) and frame-resolution
/// (`FRAME_RESOLVES_TO`) joins over the whole record set. Redaction-safe: every
/// field is an ID, a hash, a bounded label, a clipped instant, or a count.
fn build_error_signature_rows(
    in_window_signatures: &[GraphRecord],
    exemplars_by_signature: &BTreeMap<String, Vec<&GraphRecord>>,
    frame_edges_by_signature: &BTreeMap<String, Vec<&GraphRecord>>,
    window: &Window,
    from_ts: DateTimeFixed,
    to_ts: DateTimeFixed,
) -> Vec<ErrorSignatureRow> {
    let mut rows: Vec<ErrorSignatureRow> = Vec::new();
    for sig in in_window_signatures {
        let Some(crate::ir::LogPayload::ErrorSignature(payload)) = node_log_payload(sig) else {
            continue;
        };
        let signature_id = sig.id().to_owned();
        let template_hash = blake3::hash(payload.template_excerpt.as_bytes()).to_string();
        // Fingerprint the REDACTED frame chain (path text -> BLAKE3), the exact
        // frames the exported section node carries after `scrub_log_node_text`, so
        // the summary's `frame_chain_hash` and `bind_error_signature_rows`' node
        // recompute bind against identical, redaction-safe bytes (issue #340,
        // Codex round-4 P2).
        let frame_chain_hash = payload.frames.as_ref().map(|frames| {
            let serialized =
                serde_json::to_string(&redacted_frame_chain(frames)).unwrap_or_default();
            blake3::hash(serialized.as_bytes()).to_string()
        });
        // Clip the activity span to the window. first_seen is the signature's
        // valid time, which the point predicate already placed in-window, so the
        // lower clip is a no-op in practice; last_seen may extend past `to`.
        let first_seen_in_window = parse_rfc3339(&payload.first_seen).map_or_else(
            || payload.first_seen.clone(),
            |p| clip_lower(&payload.first_seen, p, &window.from, from_ts),
        );
        let last_seen_in_window = parse_rfc3339(&payload.last_seen).map_or_else(
            || payload.last_seen.clone(),
            |p| clip_upper(&payload.last_seen, p, &window.to, to_ts),
        );

        // Exemplars: content-addressed handle + hash ONLY, never text.
        let mut exemplars: Vec<ExemplarHandle> = exemplars_by_signature
            .get(&signature_id)
            .into_iter()
            .flatten()
            .filter_map(|ev| match node_log_payload(ev) {
                Some(crate::ir::LogPayload::LogEvent(evp)) => Some(ExemplarHandle {
                    protected_handle: format!(
                        "{}{}",
                        crate::protected::PROTECTED_HANDLE_PREFIX,
                        evp.event_content_hash
                    ),
                    content_hash: evp.event_content_hash.clone(),
                    source_line: evp.source_line,
                }),
                _ => None,
            })
            .collect();
        exemplars.sort_by(|a, b| {
            a.source_line
                .cmp(&b.source_line)
                .then_with(|| a.content_hash.cmp(&b.content_hash))
        });
        exemplars.dedup();

        // Frame joins: propagate the resolution label verbatim (#152/#134),
        // through the shared derivation `verify` recomputes against (issue #371).
        let empty: Vec<&GraphRecord> = Vec::new();
        let frame_resolutions = frame_resolution_joins(
            frame_edges_by_signature
                .get(&signature_id)
                .unwrap_or(&empty),
        );

        rows.push(ErrorSignatureRow {
            signature_id,
            severity: payload.severity.clone(),
            template_hash,
            frame_chain_hash,
            first_seen_in_window,
            last_seen_in_window,
            exemplars,
            frame_resolutions,
        });
    }
    rows.sort_by(|a, b| a.signature_id.cmp(&b.signature_id));
    rows
}

/// Builds the `occurrence_buckets` per-signature totals (AC2): sums ONLY the
/// in-window buckets, grouped by the signature the bucket `AGGREGATES`, ordered
/// by `(signature record_id, hour)`. Input is the coalesced record set, which
/// dedups buckets by record ID (issue #361, source-aware identity), so each
/// distinct bucket ID is counted exactly once — distinct sources (distinct IDs)
/// sum, a genuine rescan (shared ID) is collapsed upstream.
fn build_occurrence_totals(
    in_window_buckets: &[GraphRecord],
    bucket_to_signature: &BTreeMap<String, String>,
) -> Vec<SignatureOccurrenceTotal> {
    let mut by_signature: BTreeMap<String, Vec<BucketCount>> = BTreeMap::new();
    for bucket in in_window_buckets {
        let Some(crate::ir::LogPayload::LogOccurrenceBucket(payload)) = node_log_payload(bucket)
        else {
            continue;
        };
        let bucket_id = bucket.id().to_owned();
        let Some(signature_id) = bucket_to_signature.get(&bucket_id) else {
            // A bucket with no AGGREGATES edge cannot be attributed to a
            // signature; excluded from totals rather than mis-summed.
            continue;
        };
        by_signature
            .entry(signature_id.clone())
            .or_default()
            .push(BucketCount {
                bucket_id,
                hour: payload.bucket_start.clone(),
                occurrence_count: payload.occurrence_count,
            });
    }
    by_signature
        .into_iter()
        .map(|(signature_id, mut buckets)| {
            buckets.sort_by(|a, b| {
                a.hour
                    .cmp(&b.hour)
                    .then_with(|| a.bucket_id.cmp(&b.bucket_id))
            });
            let in_window_occurrences = buckets.iter().map(|b| b.occurrence_count).sum();
            SignatureOccurrenceTotal {
                signature_id,
                in_window_occurrences,
                buckets,
            }
        })
        .collect()
}

/// Builds the derived `remediation_links` leads (AC3):
/// `ErrorSignature --FRAME_RESOLVES_TO--> Symbol --CHANGED_IN--> Commit`, with
/// the commit's valid time at or after the signature's window activity, carrying
/// the verbatim frame-resolution label and any verification records linked to
/// the commit. Never a causal claim. Lifecycle filter: superseded/tombstoned
/// endpoints are skipped.
fn build_remediation_links(
    frame_edges_by_signature: &BTreeMap<String, Vec<&GraphRecord>>,
    changed_in_by_source: &BTreeMap<String, Vec<&GraphRecord>>,
    node_by_id: &BTreeMap<&str, &GraphRecord>,
    verification_ids_by_commit: &BTreeMap<String, Vec<String>>,
    signature_activity: &BTreeMap<String, DateTimeFixed>,
) -> Vec<RemediationLinkRow> {
    let mut rows: Vec<RemediationLinkRow> = Vec::new();
    for (signature_id, edges) in frame_edges_by_signature {
        // Only signatures active in-window anchor a remediation lead.
        let Some(activity) = signature_activity.get(signature_id) else {
            continue;
        };
        for frame_edge in edges {
            let GraphRecord::Edge {
                target: symbol_id, ..
            } = frame_edge
            else {
                continue;
            };
            // The frame must resolve to a live Symbol node (not File/Diagnostic,
            // not a superseded/absent record).
            let Some(symbol_node) = node_by_id.get(symbol_id.as_str()) else {
                continue;
            };
            if is_superseded_node(symbol_node) || symbol_node.node_kind_name() != Some("Symbol") {
                continue;
            }
            let Some(changed_in_edges) = changed_in_by_source.get(symbol_id.as_str()) else {
                continue;
            };
            for changed_in in changed_in_edges {
                let GraphRecord::Edge {
                    target: commit_id, ..
                } = changed_in
                else {
                    continue;
                };
                // Target must be a live Commit node.
                let Some(commit_node) = node_by_id.get(commit_id.as_str()) else {
                    continue;
                };
                if is_superseded_node(commit_node) || commit_node.node_kind_name() != Some("Commit")
                {
                    continue;
                }
                // Commit valid time must be at or after the signature's window
                // activity: a remediation cannot pre-date the error it addresses.
                let Some(commit_vt) =
                    resolve_valid_time(changed_in).or_else(|| resolve_valid_time(commit_node))
                else {
                    continue;
                };
                let Some(commit_parsed) = parse_rfc3339(&commit_vt) else {
                    continue;
                };
                if commit_parsed < *activity {
                    continue;
                }
                let verification_ids = verification_ids_by_commit
                    .get(commit_id.as_str())
                    .cloned()
                    .unwrap_or_default();
                rows.push(RemediationLinkRow {
                    signature_id: signature_id.clone(),
                    symbol_id: symbol_id.clone(),
                    commit_id: commit_id.clone(),
                    commit_valid_time: commit_vt,
                    frame_index: frame_edge.frame_index(),
                    frame_resolution: frame_edge.frame_resolution().map(|r| r.as_str().to_owned()),
                    verification_ids,
                    disclaimer: REMEDIATION_SECTION_DISCLAIMER.to_owned(),
                });
            }
        }
    }
    rows.sort_by(|a, b| {
        a.signature_id
            .cmp(&b.signature_id)
            .then_with(|| a.symbol_id.cmp(&b.symbol_id))
            .then_with(|| a.commit_id.cmp(&b.commit_id))
            .then_with(|| a.commit_valid_time.cmp(&b.commit_valid_time))
            // `frame_index`/`frame_resolution` complete the key (issue #371, Codex
            // round-7 P2): two FRAME_RESOLVES_TO edges sharing (signature, target)
            // but differing in resolution mint two remediation rows identical on
            // every key above; without these tiebreakers a stable sort preserves the
            // caller's input edge order and the derived leads (and the pack bytes)
            // would depend on it.
            .then_with(|| a.frame_index.cmp(&b.frame_index))
            .then_with(|| a.frame_resolution.cmp(&b.frame_resolution))
    });
    rows.dedup();
    rows
}

/// All log-graph derived summaries for one window, plus the capability flag for
/// the derived `remediation_links` class (issue #340).
struct LogSummaries {
    error_signatures: Vec<ErrorSignatureRow>,
    occurrence_totals: Vec<SignatureOccurrenceTotal>,
    remediation_links: Vec<RemediationLinkRow>,
    /// True when `ErrorSignature` + `FRAME_RESOLVES_TO` + `CHANGED_IN` facts all
    /// exist (the remediation capability probe).
    remediation_capable: bool,
}

/// Computes every log-graph summary (AC1/AC2/AC3) from the whole record set and
/// the in-window per-class collections.
#[allow(clippy::too_many_lines)]
fn build_log_summaries(
    records: &[GraphRecord],
    in_window_by_class: &BTreeMap<&'static str, Vec<GraphRecord>>,
    window: &Window,
    from_ts: DateTimeFixed,
    to_ts: DateTimeFixed,
) -> LogSummaries {
    let node_by_id: BTreeMap<&str, &GraphRecord> = records
        .iter()
        .filter(|r| matches!(r, GraphRecord::Node { .. }))
        .map(|r| (r.id(), r))
        .collect();

    // Join indices over the whole record set.
    let mut exemplars_by_signature: BTreeMap<String, Vec<&GraphRecord>> = BTreeMap::new();
    let mut frame_edges_by_signature: BTreeMap<String, Vec<&GraphRecord>> = BTreeMap::new();
    let mut changed_in_by_source: BTreeMap<String, Vec<&GraphRecord>> = BTreeMap::new();
    let mut bucket_to_signature: BTreeMap<String, String> = BTreeMap::new();
    let mut has_frame_resolves_to = false;
    let mut has_changed_in = false;
    let mut has_error_signature = false;

    // Verification-class records, and the commit they are edge-linked to.
    let verification_ids: BTreeSet<&str> = records
        .iter()
        .filter(|r| {
            !is_superseded_node(r)
                && evidence_class_for_record(r) == Some(EvidenceClass::VerificationEvidence)
        })
        .map(GraphRecord::id)
        .collect();
    let commit_ids: BTreeSet<&str> = records
        .iter()
        .filter(|r| r.node_kind_name() == Some("Commit"))
        .map(GraphRecord::id)
        .collect();
    let mut verification_ids_by_commit: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for record in records {
        if record.node_kind_name() == Some("ErrorSignature") {
            has_error_signature = true;
        }
        let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
        else {
            continue;
        };
        match label.as_str() {
            "FINGERPRINTED_AS" => {
                // LogEvent --FINGERPRINTED_AS--> ErrorSignature
                if let Some(ev) = node_by_id.get(source.as_str()) {
                    exemplars_by_signature
                        .entry(target.clone())
                        .or_default()
                        .push(ev);
                }
            }
            "FRAME_RESOLVES_TO" => {
                has_frame_resolves_to = true;
                frame_edges_by_signature
                    .entry(source.clone())
                    .or_default()
                    .push(record);
            }
            "CHANGED_IN" => {
                has_changed_in = true;
                changed_in_by_source
                    .entry(source.clone())
                    .or_default()
                    .push(record);
            }
            "AGGREGATES" => {
                // LogOccurrenceBucket --AGGREGATES--> ErrorSignature
                bucket_to_signature.insert(source.clone(), target.clone());
            }
            _ => {
                // Any edge linking a commit to a verification-class record makes
                // that verification linked to the commit (direction-agnostic).
                let (commit, other) = if commit_ids.contains(source.as_str()) {
                    (source.as_str(), target.as_str())
                } else if commit_ids.contains(target.as_str()) {
                    (target.as_str(), source.as_str())
                } else {
                    continue;
                };
                if verification_ids.contains(other) {
                    verification_ids_by_commit
                        .entry(commit.to_owned())
                        .or_default()
                        .push(other.to_owned());
                }
            }
        }
    }
    for ids in verification_ids_by_commit.values_mut() {
        ids.sort();
        ids.dedup();
    }

    let empty: Vec<GraphRecord> = Vec::new();
    let in_window_signatures = in_window_by_class
        .get(EvidenceClass::ErrorSignatures.as_wire())
        .unwrap_or(&empty);
    let in_window_buckets = in_window_by_class
        .get(EvidenceClass::OccurrenceBuckets.as_wire())
        .unwrap_or(&empty);

    // Per-signature window activity anchor (clipped first_seen) for remediation.
    let mut signature_activity: BTreeMap<String, DateTimeFixed> = BTreeMap::new();
    for sig in in_window_signatures {
        if let Some(crate::ir::LogPayload::ErrorSignature(payload)) = node_log_payload(sig) {
            let anchor = parse_rfc3339(&payload.first_seen)
                .map_or(from_ts, |p| if p < from_ts { from_ts } else { p });
            signature_activity.insert(sig.id().to_owned(), anchor);
        }
    }

    let error_signatures = build_error_signature_rows(
        in_window_signatures,
        &exemplars_by_signature,
        &frame_edges_by_signature,
        window,
        from_ts,
        to_ts,
    );
    let occurrence_totals = build_occurrence_totals(in_window_buckets, &bucket_to_signature);
    let remediation_links = build_remediation_links(
        &frame_edges_by_signature,
        &changed_in_by_source,
        &node_by_id,
        &verification_ids_by_commit,
        &signature_activity,
    );
    LogSummaries {
        error_signatures,
        occurrence_totals,
        remediation_links,
        remediation_capable: has_error_signature && has_frame_resolves_to && has_changed_in,
    }
}

/// Canonical BLAKE3 hash of a derived log summary (issue #340). Assembled onto the
/// section as `log_summary_hash` and recomputed by `verify_pack`'s Integrity so a
/// tampered summary value fails verification — the whole-summary analogue of the
/// per-record `BundleRecord.hash` and the only binding surface for the derived
/// `remediation_links` join (which carries no backing hashed row).
fn hash_log_summary(summary: &LogEvidenceSummary) -> String {
    let serialized = serde_json::to_string(summary).unwrap_or_default();
    blake3::hash(serialized.as_bytes()).to_string()
}

/// Binds a section's derived `log_summary` (issue #340) into `verify_pack`'s
/// Integrity so a tampered summary value fails verification, mirroring the
/// `review_coverage` `measurement` bind. Returns `Err(detail)` on any mismatch.
///
/// Two layers:
///   1. A whole-summary BLAKE3 bind (`log_summary_hash`): the summary must be
///      present iff the hash is, and the recomputed hash must match. This is the
///      ONLY bind available for `remediation_links` — a derived join whose
///      `commit_id`/`symbol_id`/`verification_ids` are the highest-risk tamper
///      target yet have NO backing hashed row — and for the exemplar/frame-join
///      fields that ride edges absent from the pack.
///   2. An independent recompute bind for the fields with backing hashed rows in
///      the section: the `error_signatures` template/frame-chain hashes, severity,
///      and window-clipped span (recomputed from the section's `ErrorSignature`
///      nodes), and the `occurrence_buckets` per-signature totals (recomputed from
///      the section's `LogOccurrenceBucket` nodes). These cannot be made to diverge
///      from the hashed evidence they summarize even by recomputing the summary
///      hash.
fn bind_log_summary_integrity(section: &EvidenceSection, window: &Window) -> Result<(), String> {
    // P1 (Codex round-4): a PRESENT log-class section MUST carry BOTH a
    // `log_summary` and its binding `log_summary_hash`. `assemble_pack` ALWAYS
    // emits the derived summary for a present `error_signatures` /
    // `occurrence_buckets` / `remediation_links` section, and `remediation_links`
    // evidence exists ONLY in the summary (the section carries zero hashed rows) —
    // so stripping BOTH the summary and its hash would silently drop all
    // remediation evidence yet still verify clean. Absence of either on a present
    // log section is an Integrity defect. (An `unavailable` section legitimately
    // carries no summary, so this only guards present log sections.)
    if section.status == "present" && is_log_evidence_class(&section.class) {
        if section.log_summary.is_none() {
            return Err(format!(
                "present log section {} carries no log_summary (derived log evidence \
                 stripped)",
                section.class
            ));
        }
        if section.log_summary_hash.is_none() {
            return Err(format!(
                "present log section {} carries no binding log_summary_hash",
                section.class
            ));
        }
    }
    let Some(summary) = &section.log_summary else {
        // No summary: the bind hash must also be absent. A hash without a summary
        // is a stripped-summary tamper (the derived evidence removed while its
        // binding lingered).
        if section.log_summary_hash.is_some() {
            return Err(format!(
                "section {} carries a log_summary_hash but no log_summary",
                section.class
            ));
        }
        return Ok(());
    };
    // (1) whole-summary hash bind.
    let Some(stored) = &section.log_summary_hash else {
        return Err(format!(
            "section {} carries a log_summary but no binding log_summary_hash",
            section.class
        ));
    };
    if *stored != hash_log_summary(summary) {
        return Err(format!(
            "section {} log_summary_hash does not bind its log_summary \
             (recomputed hash differs)",
            section.class
        ));
    }
    // (2) independent recompute binds for fields with backing hashed rows.
    let node_by_id: BTreeMap<&str, &GraphRecord> = section
        .records
        .iter()
        .filter(|br| matches!(br.record, GraphRecord::Node { .. }))
        .map(|br| (br.record.id(), &br.record))
        .collect();
    // (3) variant <-> section.class bind. Each log_summary variant must sit on its
    // matching log section; a log_summary on any other section, or a mismatched log
    // section (e.g. a RemediationLinks summary smuggled onto an error_signatures
    // section, or any log_summary on a non-log section), is an Integrity defect.
    // assemble never emits that, but verify must reject it.
    match summary {
        LogEvidenceSummary::ErrorSignatures { signatures } => {
            require_summary_on_class(&section.class, "error_signatures")?;
            // The frame-resolution attribution rides the co-located
            // `ErrorSignature --FRAME_RESOLVES_TO--> target` edges (issue #371),
            // mirroring the occurrence_buckets AGGREGATES bind (issue #340, round-3
            // P1): the signature NODE carries no frame-resolution field, so each
            // row's `frame_resolutions` is bound against these hash-bound edges,
            // grouped by their source signature — not the node.
            let mut frame_edges_by_signature: BTreeMap<&str, Vec<&GraphRecord>> = BTreeMap::new();
            for br in &section.records {
                if let GraphRecord::Edge { label, source, .. } = &br.record
                    && label.as_str() == "FRAME_RESOLVES_TO"
                {
                    frame_edges_by_signature
                        .entry(source.as_str())
                        .or_default()
                        .push(&br.record);
                }
            }
            bind_error_signature_rows(
                &section.class,
                signatures,
                &node_by_id,
                &frame_edges_by_signature,
                window,
            )
        }
        LogEvidenceSummary::OccurrenceBuckets { signature_totals } => {
            require_summary_on_class(&section.class, "occurrence_buckets")?;
            // The bucket->signature attribution rides the co-located
            // `LogOccurrenceBucket --AGGREGATES--> ErrorSignature` edges (issue #340,
            // Codex round-3 P1): the bucket NODE payload carries no signature field,
            // so `total.signature_id` is bound against these hash-bound edge
            // endpoints, not the node.
            let mut aggregates_by_bucket: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
            for br in &section.records {
                if let GraphRecord::Edge {
                    label,
                    source,
                    target,
                    ..
                } = &br.record
                    && label.as_str() == "AGGREGATES"
                {
                    aggregates_by_bucket
                        .entry(source.as_str())
                        .or_default()
                        .insert(target.as_str());
                }
            }
            bind_occurrence_totals(
                &section.class,
                signature_totals,
                &node_by_id,
                &aggregates_by_bucket,
            )
        }
        // The derived join has no backing hashed row; the whole-summary hash above
        // is its binding surface.
        LogEvidenceSummary::RemediationLinks { .. } => {
            require_summary_on_class(&section.class, "remediation_links")?;
            Ok(())
        }
    }
}

/// Requires a `log_summary` variant to sit on its own section class (issue #340):
/// `ErrorSignatures` <-> `error_signatures`, `OccurrenceBuckets` <->
/// `occurrence_buckets`, `RemediationLinks` <-> `remediation_links`. A summary on a
/// non-log section or on a mismatched log section is an Integrity defect.
fn require_summary_on_class(class: &str, expected: &str) -> Result<(), String> {
    if class != expected {
        return Err(format!(
            "section {class} carries a {expected} log_summary whose variant does not \
             match its class"
        ));
    }
    Ok(())
}

/// Binds every `error_signatures` summary row to its backing hashed `ErrorSignature`
/// node in the section (issue #340): an exact one-to-one correspondence, and each
/// row's `template_hash`, `frame_chain_hash`, `severity`, and window-clipped span
/// recomputed from the node payload. The `frame_resolutions` field is additionally
/// re-derived from the co-located `FRAME_RESOLVES_TO` edges (issue #371). Only the
/// exemplar handles ride a node absent from the pack (the redaction-scrubbed
/// `LogEvent`) and are bound by the whole-summary hash alone.
fn bind_error_signature_rows(
    class: &str,
    signatures: &[ErrorSignatureRow],
    node_by_id: &BTreeMap<&str, &GraphRecord>,
    frame_edges_by_signature: &BTreeMap<&str, Vec<&GraphRecord>>,
    window: &Window,
) -> Result<(), String> {
    // Exact bijection: one summary row per section ErrorSignature node, so a
    // signature cannot be dropped from — or a phantom one smuggled into — the
    // summary while every hashed row stays valid.
    let section_sig_ids: BTreeSet<&str> = node_by_id
        .iter()
        .filter(|(_, rec)| {
            matches!(
                node_log_payload(rec),
                Some(crate::ir::LogPayload::ErrorSignature(_))
            )
        })
        .map(|(id, _)| *id)
        .collect();
    // Reject a repeated `signature_id` BEFORE the dedup set collapses it (issue
    // #373, Codex round-5 P2). A `BTreeSet` would silently merge two rows sharing a
    // `signature_id` — both then bind the same hashed `ErrorSignature` node and the
    // bijection still balances — but the summary would no longer have exactly one
    // row per signature, and a duplicate row can carry divergent exemplar handles or
    // frame-resolution targets bound only by the whole-summary hash. Detect the
    // collision explicitly instead of silently absorbing it.
    let mut summary_ids: BTreeSet<&str> = BTreeSet::new();
    for row in signatures {
        if !summary_ids.insert(row.signature_id.as_str()) {
            return Err(format!(
                "error_signatures summary lists signature {} more than once in \
                 section {class} (one row per signature required)",
                row.signature_id
            ));
        }
    }
    if let Some(extra) = summary_ids.difference(&section_sig_ids).next() {
        return Err(format!(
            "error_signatures summary row {extra} has no backing hashed ErrorSignature \
             node in section {class}"
        ));
    }
    if let Some(missing) = section_sig_ids.difference(&summary_ids).next() {
        return Err(format!(
            "error_signatures section {class} carries ErrorSignature node {missing} \
             absent from the summary"
        ));
    }
    let from_ts = parse_rfc3339(&window.from);
    let to_ts = parse_rfc3339(&window.to);
    for row in signatures {
        let Some(crate::ir::LogPayload::ErrorSignature(payload)) = node_by_id
            .get(row.signature_id.as_str())
            .copied()
            .and_then(node_log_payload)
        else {
            return Err(format!(
                "error_signatures summary row {} has no ErrorSignature payload to bind against",
                row.signature_id
            ));
        };
        // The exported section node's `template_excerpt` has been scrubbed to hold
        // the template fingerprint itself (issue #340, Codex round-4 P2), which is
        // exactly the value the summary carries as `template_hash`. Bind them
        // directly: a forged summary `template_hash` still fails even with the
        // whole-summary hash recomputed, and no raw template text is required.
        if row.template_hash != payload.template_excerpt {
            return Err(format!(
                "error_signatures row {} template_hash does not bind its \
                 ErrorSignature template fingerprint",
                row.signature_id
            ));
        }
        // The section node's frames are already redaction-scrubbed (path text ->
        // fingerprint), identical to what `build_error_signature_rows` hashed, so
        // recomputing over them reproduces the summary's `frame_chain_hash`.
        let expect_frame_chain = payload.frames.as_ref().map(|frames| {
            let serialized = serde_json::to_string(frames).unwrap_or_default();
            blake3::hash(serialized.as_bytes()).to_string()
        });
        if row.frame_chain_hash != expect_frame_chain {
            return Err(format!(
                "error_signatures row {} frame_chain_hash does not bind its \
                 ErrorSignature frames",
                row.signature_id
            ));
        }
        if row.severity != payload.severity {
            return Err(format!(
                "error_signatures row {} severity does not bind its ErrorSignature node",
                row.signature_id
            ));
        }
        // Window-clipped span recompute (only when both bounds parse; a malformed
        // window is caught independently by Window-consistency).
        if let (Some(from_ts), Some(to_ts)) = (from_ts, to_ts) {
            let expect_first = parse_rfc3339(&payload.first_seen).map_or_else(
                || payload.first_seen.clone(),
                |p| clip_lower(&payload.first_seen, p, &window.from, from_ts),
            );
            let expect_last = parse_rfc3339(&payload.last_seen).map_or_else(
                || payload.last_seen.clone(),
                |p| clip_upper(&payload.last_seen, p, &window.to, to_ts),
            );
            if row.first_seen_in_window != expect_first || row.last_seen_in_window != expect_last {
                return Err(format!(
                    "error_signatures row {} window-clipped span does not bind its \
                     ErrorSignature valid times",
                    row.signature_id
                ));
            }
        }
        // Frame-resolution attribution bind (issue #371).
        bind_frame_resolutions(class, row, frame_edges_by_signature)?;
    }
    Ok(())
}

/// Binds one `error_signatures` row's `frame_resolutions` to the co-located
/// `FRAME_RESOLVES_TO` edges sourced at its signature (issue #371): the row must
/// EQUAL the shared `frame_resolution_joins` derivation of those edges — the EXACT
/// mapping + ordering `build_error_signature_rows` used — so a relabeled resolution,
/// a moved `frame_index`, a retargeted edge, a dropped edge, or an added/smuggled
/// edge all break the equality, and the whole-summary hash is no longer the sole
/// binding surface for these two fields. Section membership already restricts the
/// co-located edges to `FRAME_RESOLVES_TO` edges sourced at a present signature node,
/// and the caller's row bijection guarantees every such source has exactly one
/// summary row, so no frame edge escapes this per-row check.
fn bind_frame_resolutions(
    class: &str,
    row: &ErrorSignatureRow,
    frame_edges_by_signature: &BTreeMap<&str, Vec<&GraphRecord>>,
) -> Result<(), String> {
    let empty: Vec<&GraphRecord> = Vec::new();
    let expected = frame_resolution_joins(
        frame_edges_by_signature
            .get(row.signature_id.as_str())
            .unwrap_or(&empty),
    );
    if row.frame_resolutions != expected {
        return Err(format!(
            "error_signatures row {} frame_resolutions do not bind their \
             co-located FRAME_RESOLVES_TO edges in section {class}",
            row.signature_id
        ));
    }
    Ok(())
}

/// Binds every `occurrence_buckets` summary total to its backing hashed
/// `LogOccurrenceBucket` nodes in the section (issue #340): each listed bucket must
/// resolve to a present hashed node whose `occurrence_count`/`bucket_start` match,
/// no bucket may be double-counted, and each `in_window_occurrences` must equal the
/// recomputed sum — so the occurrence total (the tamper target flagged by the P1)
/// cannot be inflated without adding real, count-matching hashed bucket rows.
fn bind_occurrence_totals(
    class: &str,
    totals: &[SignatureOccurrenceTotal],
    node_by_id: &BTreeMap<&str, &GraphRecord>,
    aggregates_by_bucket: &BTreeMap<&str, BTreeSet<&str>>,
) -> Result<(), String> {
    // Exact bijection (mirrors `bind_error_signature_rows`): the set of hashed
    // `LogOccurrenceBucket` nodes actually present in the section must equal the set
    // of buckets the summary lists. The per-bucket backing-node check below rejects
    // a phantom summary bucket (summary \ section); the reverse guard after the loop
    // rejects a hashed bucket dropped from the summary (section \ summary), which
    // would under-report `in_window_occurrences` while its hashed row lingers.
    let section_bucket_ids: BTreeSet<&str> = node_by_id
        .iter()
        .filter(|(_, rec)| {
            matches!(
                node_log_payload(rec),
                Some(crate::ir::LogPayload::LogOccurrenceBucket(_))
            )
        })
        .map(|(id, _)| *id)
        .collect();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut seen_signatures: BTreeSet<&str> = BTreeSet::new();
    for total in totals {
        // Reject a duplicate `signature_id` across totals (issue #373, Codex
        // round-5 P2): one occurrence total per signature. Two totals for one
        // signature — splitting its buckets, or an appended empty zero total — each
        // balance their own bucket sum and leave the reverse bucket-coverage guard
        // satisfied, so without this a consumer reading a per-signature total sees
        // the count under-reported. This is the round-3 under-reporting failure
        // re-expressed via row duplication rather than bucket omission.
        if !seen_signatures.insert(total.signature_id.as_str()) {
            return Err(format!(
                "occurrence_buckets summary lists signature {} more than once in \
                 section {class} (one occurrence total per signature required)",
                total.signature_id
            ));
        }
        let mut sum: u64 = 0;
        for bucket in &total.buckets {
            if !seen.insert(bucket.bucket_id.as_str()) {
                return Err(format!(
                    "occurrence_buckets bucket {} appears more than once in section {class}",
                    bucket.bucket_id
                ));
            }
            let Some(crate::ir::LogPayload::LogOccurrenceBucket(payload)) = node_by_id
                .get(bucket.bucket_id.as_str())
                .copied()
                .and_then(node_log_payload)
            else {
                return Err(format!(
                    "occurrence_buckets bucket {} has no backing hashed \
                     LogOccurrenceBucket node in section {class}",
                    bucket.bucket_id
                ));
            };
            if bucket.occurrence_count != payload.occurrence_count {
                return Err(format!(
                    "occurrence_buckets bucket {} occurrence_count does not bind its \
                     LogOccurrenceBucket node",
                    bucket.bucket_id
                ));
            }
            if bucket.hour != payload.bucket_start {
                return Err(format!(
                    "occurrence_buckets bucket {} hour does not bind its \
                     LogOccurrenceBucket node",
                    bucket.bucket_id
                ));
            }
            // Signature attribution bind (issue #340, Codex round-3 P1): the bucket
            // must be filed under the SAME signature its co-located
            // `LogOccurrenceBucket --AGGREGATES--> ErrorSignature` edge names. Without
            // this a tampered pack could move a bucket under another signature — the
            // node count/hour still bind and both sums still balance — and consumers
            // would read per-signature occurrence counts for the WRONG incident.
            match aggregates_by_bucket.get(bucket.bucket_id.as_str()) {
                Some(sigs) if sigs.contains(total.signature_id.as_str()) => {}
                Some(_) => {
                    return Err(format!(
                        "occurrence_buckets bucket {} is filed under signature {} but its \
                         AGGREGATES attribution edge(s) name a different signature in \
                         section {class}",
                        bucket.bucket_id, total.signature_id
                    ));
                }
                None => {
                    return Err(format!(
                        "occurrence_buckets bucket {} has no co-located AGGREGATES \
                         attribution edge binding it to signature {} in section {class}",
                        bucket.bucket_id, total.signature_id
                    ));
                }
            }
            sum = sum.saturating_add(payload.occurrence_count);
        }
        if total.in_window_occurrences != sum {
            return Err(format!(
                "occurrence_buckets signature {} in_window_occurrences {} does not equal \
                 the sum {} of its bound buckets",
                total.signature_id, total.in_window_occurrences, sum
            ));
        }
    }
    // Reverse guard: every hashed `LogOccurrenceBucket` node the section carries must
    // be covered by the summary. `seen` holds exactly the summary buckets that
    // resolved to a backing node (a phantom bucket already returned above), so any
    // hashed bucket in `section_bucket_ids` missing from `seen` is a bucket silently
    // dropped from the summary — Integrity must reject it.
    if let Some(missing) = section_bucket_ids.difference(&seen).next() {
        return Err(format!(
            "occurrence_buckets section {class} carries LogOccurrenceBucket node {missing} \
             absent from the summary"
        ));
    }
    Ok(())
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

    // --- coalesce duplicate log records by stable ID (issue #340, round-6 P2) ---
    // A concatenated multi-scan graph carries the SAME stable log ID once per scan
    // (a `LogSource` is a non-identity input). Merge those BEFORE window filtering
    // and summary building so exactly one row per signature/bucket ID flows into
    // both the hashed section nodes AND the derived summary — otherwise assemble
    // would emit duplicate rows its own `verify_pack` rejects. No-op for a
    // single-scan graph (no duplicate log IDs), so every existing pack is
    // byte-identical.
    let coalesced = coalesce_log_records(records);
    let records = coalesced.as_slice();

    // --- capability availability (whole record set) ---
    let mut any_pr = false;
    let mut present_classes: BTreeSet<&'static str> = BTreeSet::new();
    let mut has_error_signature = false;
    let mut has_frame_resolves_to = false;
    let mut has_changed_in = false;
    for record in records {
        if let Some(class) = evidence_class_for_record(record) {
            present_classes.insert(class.as_wire());
            if class == EvidenceClass::PullRequests {
                any_pr = true;
            }
        }
        match record {
            GraphRecord::Node { kind, .. } if kind.as_str() == "ErrorSignature" => {
                has_error_signature = true;
            }
            GraphRecord::Edge { label, .. } if label.as_str() == "FRAME_RESOLVES_TO" => {
                has_frame_resolves_to = true;
            }
            GraphRecord::Edge { label, .. } if label.as_str() == "CHANGED_IN" => {
                has_changed_in = true;
            }
            _ => {}
        }
    }
    // `remediation_links` (issue #340) has no backing node kind, so its
    // availability rides a capability probe: the derived
    // `ErrorSignature --FRAME_RESOLVES_TO--> Symbol --CHANGED_IN--> Commit` join
    // is available iff all three fact kinds exist in the record set.
    let remediation_capable = has_error_signature && has_frame_resolves_to && has_changed_in;
    if remediation_capable {
        present_classes.insert(EvidenceClass::RemediationLinks.as_wire());
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
            // `occurrence_buckets` use the AC2 interval-intersection rule, NOT the
            // point predicate: a bucket whose hour `[bucket_start, +1h)`
            // intersects the window is included WHOLE (issue #340). Every other
            // class uses the half-open point predicate `from <= t < to`.
            Some(parsed)
                if if class == EvidenceClass::OccurrenceBuckets {
                    bucket_hour_intersects_window(parsed, from_ts, to_ts)
                } else {
                    from_ts <= parsed && parsed < to_ts
                } =>
            {
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

    // --- occurrence-bucket signature attribution (issue #340, Codex round-3) ---
    // Each `occurrence_buckets` summary total attributes its buckets to a signature
    // (`total.signature_id`), but the `LogOccurrenceBucket` NODE payload carries no
    // signature field — the signature is only an identity input hashed into the
    // bucket's stable ID. So the bucket->signature binding must be carried by a
    // HASH-BOUND row for `verify_pack` to re-derive it offline: the
    // `LogOccurrenceBucket --AGGREGATES--> ErrorSignature` edge. Those edges are
    // co-located into the occurrence_buckets section here (P1). An in-window bucket
    // with NO attribution edge cannot be filed under any signature, so it is
    // EXCLUDED from BOTH the section records and the summary under a counted
    // `unattributed_bucket` diagnostic — never silently mis-summed and never left in
    // the section where the reverse-coverage guard would reject the freshly
    // assembled pack (P2, the assemble<->verify consistency invariant). Mirrors the
    // `missing_valid_time` exclusion-diagnostic idiom.
    {
        let bucket_ids_with_edge: BTreeSet<&str> = records
            .iter()
            .filter_map(|r| match r {
                GraphRecord::Edge { label, source, .. } if label.as_str() == "AGGREGATES" => {
                    Some(source.as_str())
                }
                _ => None,
            })
            .collect();
        if let Some(buckets) =
            in_window_by_class.get_mut(EvidenceClass::OccurrenceBuckets.as_wire())
        {
            let existing = std::mem::take(buckets);
            let mut included_ids: BTreeSet<String> = BTreeSet::new();
            for bucket in existing {
                if bucket_ids_with_edge.contains(bucket.id()) {
                    included_ids.insert(bucket.id().to_owned());
                    buckets.push(bucket);
                } else {
                    diagnostics.push(PackDiagnostic {
                        code: "unattributed_bucket".to_owned(),
                        evidence_class: Some(EvidenceClass::OccurrenceBuckets.as_wire().to_owned()),
                        unavailable_reason: None,
                        record_ids: vec![bucket.id().to_owned()],
                        detail: "in-window LogOccurrenceBucket excluded: no AGGREGATES \
                                 attribution edge names its signature"
                            .to_owned(),
                    });
                }
            }
            // Co-locate every AGGREGATES attribution edge whose source is an included
            // bucket as a hash-bound row, so verify re-derives the bucket->signature
            // binding from tamper-evident evidence.
            for record in records {
                if let GraphRecord::Edge { label, source, .. } = record
                    && label.as_str() == "AGGREGATES"
                    && included_ids.contains(source.as_str())
                {
                    buckets.push(record.clone());
                }
            }
        }
    }

    // --- error-signature frame-resolution attribution (issue #371) ---
    // Each `error_signatures` summary row propagates its `FRAME_RESOLVES_TO` edges
    // verbatim as `frame_resolutions` (the resolution label + `frame_index`, issues
    // #152/#134), but the summary's `template_hash`/`frame_chain_hash`/severity/span
    // fields are recomputed by `verify_pack` from the co-located `ErrorSignature`
    // NODE while the `frame_resolution`/`frame_index` fields were bound ONLY by the
    // whole-summary hash — weaker than every node-backed field. Mirror the #340/#365
    // AGGREGATES treatment: co-locate every
    // `ErrorSignature --FRAME_RESOLVES_TO--> {Symbol|File|Diagnostic}` edge whose
    // SOURCE is an included in-window signature as a hash-bound row, so `verify_pack`
    // re-derives each row's frame joins from tamper-evident evidence. Unlike the
    // bucket case there is NO exclusion: a signature with zero frame edges is
    // legitimate (never mis-attributed), so every in-window signature stays and only
    // its frame edges (if any) are appended. The bind reads only edge identity +
    // label + `frame_index` + endpoint IDs; the edge's REQUIRED free-text `summary`
    // is not read here, but an older/hand-authored importer could populate it with
    // raw backtrace/log text, so `scrub_log_node_text` clears a FRAME_RESOLVES_TO
    // edge's summary in `build_section_records` before it enters a hashed row —
    // preserving the zero-raw-log Safety invariant (issue #371, Codex P2).
    {
        if let Some(signatures) =
            in_window_by_class.get_mut(EvidenceClass::ErrorSignatures.as_wire())
        {
            let signature_ids: BTreeSet<String> = signatures
                .iter()
                .filter(|r| {
                    matches!(
                        node_log_payload(r),
                        Some(crate::ir::LogPayload::ErrorSignature(_))
                    )
                })
                .map(|r| r.id().to_owned())
                .collect();
            for record in records {
                if let GraphRecord::Edge { label, source, .. } = record
                    && label.as_str() == "FRAME_RESOLVES_TO"
                    && signature_ids.contains(source.as_str())
                {
                    signatures.push(record.clone());
                }
            }
        }
    }

    // --- review coverage measurement over in-window merged PRs ---
    // Delegated to the SHARED review-coverage derivation (issue #339, AC7) so the
    // pack's `review_coverage` section / `merged_pr_without_approving_review` gap
    // and `eg audit review-coverage` can never fork this logic. The pack uses the
    // LENIENT options — any genuine approving review resolving in-window AT OR
    // BEFORE the PR's `merged_at` counts as coverage, matching the established
    // #338 semantics (round-5/round-9) — so `covered_pr_ids` equals both the
    // approving-target set and the audit lane's covered set when that lane is run
    // with the same lenient options. `eg audit review-coverage` calls the SAME
    // function with its strict `--require-non-author` / `--require-final-head`
    // defaults. A PR is "merged in window" iff its MERGE time (`merged_at`,
    // first-class since #333) falls in the half-open window — never its Task
    // `valid_time` (github_updated_at). Post-merge approvals never gate the merge.
    let derivation = derive_review_coverage(
        records,
        window,
        ReviewCoverageOptions {
            require_non_author: false,
            require_final_head: false,
        },
    );
    let merged_pr_ids: Vec<String> = derivation.merged_pr_ids.clone();
    // The `merged_pr_without_approving_review` gap keys on the OPTION-INDEPENDENT
    // lenient any-approving set (never the strict covered set), so a self-approved
    // or stale-head PR is never mislabeled as having no approving review at all.
    let approving_targets: BTreeSet<String> = derivation.any_approving_pr_ids.clone();

    // Rebuild the coverage-substantiating link edges (each stamped with its
    // covered PR's in-window `merged_at` as the window-relevant valid time, with
    // the approving review's valid time preserved as the cited `author_time` —
    // Codex Finding A) and their deduped source review nodes from the shared
    // derivation. Co-located in the `review_coverage` section so `verify_pack`
    // resolves every coverage edge's source offline even when the catalog maps no
    // `reviews` section (Codex round-18 Finding 2).
    let by_id_pack: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let mut coverage_link_edges: Vec<GraphRecord> = Vec::new();
    let mut coverage_review_nodes: BTreeMap<String, GraphRecord> = BTreeMap::new();
    for sub in &derivation.coverage_substantiations {
        if let Some(edge) = by_id_pack.get(sub.edge_id.as_str()) {
            // Stamp the edge's window-relevant valid time from the PR's in-window
            // `merged_at` (Codex Finding A); the pre-window-eligible review valid
            // time is preserved as the edge's cited `author_time`.
            coverage_link_edges.push(stamp_edge_valid_time(
                (*edge).clone(),
                &sub.merged_at,
                &sub.review_valid_time,
            ));
        }
        if let Some(review) = by_id_pack.get(sub.review_id.as_str()) {
            coverage_review_nodes
                .entry(sub.review_id.clone())
                .or_insert_with(|| (*review).clone());
        }
    }
    let unapproved_pr_ids: Vec<String> = merged_pr_ids
        .iter()
        .filter(|id| !derivation.covered_pr_ids.contains(*id))
        .cloned()
        .collect();
    let merged_pr_count = merged_pr_ids.len();
    let approved_pr_count = derivation.covered_pr_ids.len();
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

    // --- log-graph derived summaries (issue #340) ---
    // Computed from the whole record set + the in-window per-class collections
    // BEFORE the section loop consumes `in_window_by_class` via `remove`.
    let log_summaries = build_log_summaries(records, &in_window_by_class, window, from_ts, to_ts);
    debug_assert_eq!(log_summaries.remediation_capable, remediation_capable);

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
        let disclaimer = match class {
            EvidenceClass::StructuralDeltas | EvidenceClass::PublicApiDeltas => {
                Some(DELTA_SECTION_DISCLAIMER.to_owned())
            }
            EvidenceClass::ErrorSignatures | EvidenceClass::OccurrenceBuckets => {
                Some(LOG_SECTION_DISCLAIMER.to_owned())
            }
            EvidenceClass::RemediationLinks => Some(REMEDIATION_SECTION_DISCLAIMER.to_owned()),
            _ => None,
        };
        // Attach the derived log summary (issue #340) only on a Present log
        // section; an unavailable section stays clean (`log_summary: None`) so the
        // degradation markers read exactly as before.
        let log_summary = if availability == Availability::Present {
            match class {
                EvidenceClass::ErrorSignatures => Some(LogEvidenceSummary::ErrorSignatures {
                    signatures: log_summaries.error_signatures.clone(),
                }),
                EvidenceClass::OccurrenceBuckets => Some(LogEvidenceSummary::OccurrenceBuckets {
                    signature_totals: log_summaries.occurrence_totals.clone(),
                }),
                EvidenceClass::RemediationLinks => Some(LogEvidenceSummary::RemediationLinks {
                    links: log_summaries.remediation_links.clone(),
                }),
                _ => None,
            }
        } else {
            None
        };
        // Bind the derived summary into Integrity (issue #340): the whole-summary
        // BLAKE3 hash `verify_pack` recomputes so a tampered summary value fails.
        let log_summary_hash = log_summary.as_ref().map(hash_log_summary);
        sections.push(EvidenceSection {
            class: class.as_wire().to_owned(),
            requirement: cr.requirement.as_wire().to_owned(),
            status: status.to_owned(),
            outcome,
            unavailable_reason: reason,
            record_count: records_for_class.len(),
            records: records_for_class,
            measurement: is_review_coverage.then(|| review_measurement.clone()),
            log_summary,
            log_summary_hash,
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
        &derivation.coverage_substantiations,
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

#[allow(clippy::too_many_lines)]
fn derive_gaps(
    records: &[GraphRecord],
    control: &Control,
    window: &Window,
    merged_pr_ids: &[String],
    approving_targets: &BTreeSet<String>,
    coverage_substantiations: &[CoverageSubstantiation],
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
    // edge) is now MERGED, so the two dependent gap classes
    // (`review_unanchored_no_commit_sha`, `approval_precedes_final_head`) are
    // derived from the recorded reviewed-commit facts using the SAME
    // approving-review -> merged-PR anchoring join the coverage derivation uses.
    // The capability diagnostic now fires ONLY when those facts are genuinely
    // unavailable in this store (a pre-#334 store carrying no `review_commit_sha`
    // on any review AND no `REVIEWS_COMMIT` edge), so a pack over old data never
    // presents as if the two checks ran cleanly (Codex round-7 P2: resemblance is
    // never mistaken for a real derivation), while a pack over #334-era data
    // reports the real defects.
    if want_review_anchored {
        // Presence probe on the ACTUAL typed facts, never string resemblance: a
        // Review carrying a real `review_commit_sha`, a real `REVIEWS_COMMIT`
        // anchor edge, OR the importer's own `github_review_unanchored`
        // `Diagnostic` (Codex P2). The diagnostic is a #334-era importer artifact:
        // when an approving review genuinely carries no `commit_id`, the #334
        // importer emits neither a `review_commit_sha` nor a `REVIEWS_COMMIT` edge,
        // only that diagnostic. Without this branch a store whose reviews are ALL
        // unanchored would look pre-#334 and wrongly degrade to
        // `capability_unavailable`, hiding the real `review_unanchored_no_commit_sha`
        // gap behind apparently-satisfied coverage. It is matched as a
        // `NodeKind::Diagnostic` node whose summary carries the importer's
        // `[github_review_unanchored]` bracket-code prefix — the same structured
        // signal `diagnostics_with_code` matches (see `src/github/records.rs`), a
        // deterministic extractor artifact, not user-authored string resemblance.
        // A genuinely PRE-#334 store carries none of the three signals and still
        // degrades.
        let facts_available = records.iter().any(|r| {
            review_commit_sha_of(r).is_some()
                || matches!(r, GraphRecord::Edge { label, .. } if label.as_str() == "REVIEWS_COMMIT")
                || matches!(
                    r,
                    GraphRecord::Node {
                        kind: crate::ir::NodeKind::Diagnostic,
                        summary,
                        ..
                    } if summary.contains("[github_review_unanchored]")
                )
        });
        if facts_available {
            // Drive the #334 gap derivation from the SAME approving-review set the
            // coverage derivation produced (`coverage_substantiations`), never a
            // re-derivation with a different filter (Codex Finding B). Coverage now
            // counts a pre-window approval that gated an in-window merge (its
            // position relative to `[from, to)` is irrelevant; only the
            // at-or-before-`merged_at` gate applies), so requiring the review to be
            // in-window HERE would skip the #334 defect of exactly those approvals,
            // leaving a pack with passing coverage but no #334 gap at period
            // boundaries. Consuming the substantiations makes coverage and gaps
            // structurally unable to diverge: every counted approving review is
            // checked for its #334 anchor, and only those.
            for sub in coverage_substantiations {
                let Some(review) = by_id.get(sub.review_id.as_str()) else {
                    continue;
                };
                let Some(task) = by_id.get(sub.pr_id.as_str()) else {
                    continue;
                };
                let mut record_ids = vec![sub.review_id.clone(), sub.pr_id.clone()];
                record_ids.sort();
                match (review_commit_sha_of(review), pr_head_sha(task)) {
                    (None, _) => gaps.push(GapRow {
                        gap_class: GapClass::ReviewUnanchoredNoCommitSha.as_wire().to_owned(),
                        record_ids,
                        // The gap's window-relevant timestamp is the covered PR's
                        // in-window `merged_at` (Codex Finding A/B): a pre-window
                        // review valid time would place the gap outside `[from,
                        // to)`, so a consumer filtering gaps by the manifest window
                        // would drop it — inconsistent with the in-window coverage
                        // it mirrors.
                        valid_time: Some(sub.merged_at.clone()),
                        detail: "approving review references a merged pull request but \
                                 carries no review_commit_sha anchor"
                            .to_owned(),
                    }),
                    (Some(rcs), Some(head)) if rcs != head => gaps.push(GapRow {
                        gap_class: GapClass::ApprovalPrecedesFinalHead.as_wire().to_owned(),
                        record_ids,
                        valid_time: Some(sub.merged_at.clone()),
                        detail: "approving review anchored to a commit other than the pull \
                                 request's final head (approval precedes final head)"
                            .to_owned(),
                    }),
                    // An ANCHORED approval whose PR carries no `head_sha` (a
                    // pre-#333 or partial import): the final head cannot be
                    // verified, so the anchor check must NOT appear to have run
                    // cleanly through the catch-all. This mirrors the coverage
                    // side (9c639f7), which degrades exactly this PR to
                    // `approval_stale_head` + a `head_sha_unavailable` sub-label
                    // rather than `covered`; the gap side surfaces the
                    // corresponding `approval_precedes_final_head` defect, flagged
                    // `head_sha_unavailable` so it reports "final head
                    // unverifiable" — never a fabricated specific mismatch, since
                    // the PR head is absent (there is no head to differ from).
                    (Some(_), None) => gaps.push(GapRow {
                        gap_class: GapClass::ApprovalPrecedesFinalHead.as_wire().to_owned(),
                        record_ids,
                        valid_time: Some(sub.merged_at.clone()),
                        detail: "approving review is anchored but the pull request has no \
                                 recorded head_sha, so the final head cannot be verified \
                                 (head_sha_unavailable)"
                            .to_owned(),
                    }),
                    _ => {}
                }
            }
        } else {
            let needs_334: Vec<&'static str> = GapClass::ALL
                .iter()
                .filter(|g| g.needs_issue_334())
                .map(GapClass::as_wire)
                .collect();
            diagnostics.push(PackDiagnostic {
                code: "capability_unavailable".to_owned(),
                evidence_class: None,
                unavailable_reason: Some("issue_334_reviewed_commit_facts_absent".to_owned()),
                record_ids: Vec::new(),
                detail: format!(
                    "gap classes {} require issue #334 reviewed-commit facts \
                     (review_commit_sha / REVIEWS_COMMIT), which are absent from this store",
                    needs_334.join(" and "),
                ),
            });
        }
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
        // Co-located `FRAME_RESOLVES_TO` attribution edges (issue #371) carry a
        // required free-text `summary` that `scrub_log_node_text` clears at
        // assemble time. But `verify_pack` reads `section.records` AS-IS and never
        // re-runs that scrub, the frame binding derives only
        // `frame_index`/`resolution`/`target_id` (never `summary`), and the checks
        // above only catch secrets (`detect_secret`) and Node scrubbed fields
        // (`first_unscrubbed_field` is a no-op for edges). So a NON-secret raw
        // backtrace placed in this summary — with the row hash recomputed so
        // Integrity passes — would otherwise verify clean. Treat a nonempty
        // co-located frame-edge summary as an unscrubbed field so verify FAILS
        // Safety, mirroring the assemble-side scrub discipline.
        if let GraphRecord::Edge {
            label: crate::ir::EdgeLabel::FrameResolvesTo,
            summary,
            ..
        } = &br.record
            && !summary.is_empty()
        {
            return (
                false,
                format!(
                    "record {record_id} retains scrubbed field 'summary' \
                     (co-located FRAME_RESOLVES_TO edge)"
                ),
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
#[allow(clippy::too_many_lines)]
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
        // Log-summary text fields (issue #340). Every field is an ID, hash,
        // bounded label, clipped instant, or count — never raw log/exemplar text —
        // but enumerate them anyway so a `WHERE` detail can name them precisely
        // (LOCKSTEP contract). The whole-artifact backstop still catches any field
        // missed here.
        match &s.log_summary {
            Some(LogEvidenceSummary::ErrorSignatures { signatures }) => {
                for (j, r) in signatures.iter().enumerate() {
                    let base = format!("sections[{i}].log_summary.signatures[{j}]");
                    out.push((format!("{base}.signature_id"), r.signature_id.as_str()));
                    out.push((format!("{base}.severity"), r.severity.as_str()));
                    out.push((format!("{base}.template_hash"), r.template_hash.as_str()));
                    if let Some(h) = r.frame_chain_hash.as_deref() {
                        out.push((format!("{base}.frame_chain_hash"), h));
                    }
                    out.push((
                        format!("{base}.first_seen_in_window"),
                        r.first_seen_in_window.as_str(),
                    ));
                    out.push((
                        format!("{base}.last_seen_in_window"),
                        r.last_seen_in_window.as_str(),
                    ));
                    for (k, ex) in r.exemplars.iter().enumerate() {
                        out.push((
                            format!("{base}.exemplars[{k}].protected_handle"),
                            ex.protected_handle.as_str(),
                        ));
                        out.push((
                            format!("{base}.exemplars[{k}].content_hash"),
                            ex.content_hash.as_str(),
                        ));
                    }
                    for (k, fr) in r.frame_resolutions.iter().enumerate() {
                        if let Some(res) = fr.resolution.as_deref() {
                            out.push((format!("{base}.frame_resolutions[{k}].resolution"), res));
                        }
                        out.push((
                            format!("{base}.frame_resolutions[{k}].target_id"),
                            fr.target_id.as_str(),
                        ));
                    }
                }
            }
            Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) => {
                for (j, t) in signature_totals.iter().enumerate() {
                    let base = format!("sections[{i}].log_summary.signature_totals[{j}]");
                    out.push((format!("{base}.signature_id"), t.signature_id.as_str()));
                    for (k, b) in t.buckets.iter().enumerate() {
                        out.push((
                            format!("{base}.buckets[{k}].bucket_id"),
                            b.bucket_id.as_str(),
                        ));
                        out.push((format!("{base}.buckets[{k}].hour"), b.hour.as_str()));
                    }
                }
            }
            Some(LogEvidenceSummary::RemediationLinks { links }) => {
                for (j, l) in links.iter().enumerate() {
                    let base = format!("sections[{i}].log_summary.links[{j}]");
                    out.push((format!("{base}.signature_id"), l.signature_id.as_str()));
                    out.push((format!("{base}.symbol_id"), l.symbol_id.as_str()));
                    out.push((format!("{base}.commit_id"), l.commit_id.as_str()));
                    out.push((
                        format!("{base}.commit_valid_time"),
                        l.commit_valid_time.as_str(),
                    ));
                    if let Some(res) = l.frame_resolution.as_deref() {
                        out.push((format!("{base}.frame_resolution"), res));
                    }
                    for (k, v) in l.verification_ids.iter().enumerate() {
                        out.push((format!("{base}.verification_ids[{k}]"), v.as_str()));
                    }
                    out.push((format!("{base}.disclaimer"), l.disclaimer.as_str()));
                }
            }
            None => {}
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
                    let GraphRecord::Edge {
                        source,
                        target,
                        temporal,
                        ..
                    } = &br.record
                    else {
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
                    // TARGET-INDEPENDENT stamped-edge merge proof (Codex round-21,
                    // the comprehensive fix for the whole windowed-out-target class).
                    // The coverage edge's OWN stamped fields must prove the gate
                    // whether or not the target PR Task is present in the pack. A
                    // merged PR whose Task `valid_time` is out of window is
                    // legitimately absent from every section, so the target-present
                    // proof below cannot run; WITHOUT a target-independent proof a
                    // tampered pack whose coverage edge cites an absent target could
                    // forge the stamped anchors — or leave a source review row whose
                    // real time post-dates the merge — and still substantiate
                    // `approved_pr_count` offline (the four successive round-1..4
                    // holes were all this same class). Prove the gate ENTIRELY from
                    // the edge's stamp plus the (always-present, checked above) source
                    // review node, independent of the target:
                    //   (1) the stamped `valid_time` (the PR `merged_at` anchor)
                    //       parses AND is in the half-open manifest window;
                    //   (2) the stamped `author_time` (the review-time anchor)
                    //       parses AND is at or before that `merged_at` (a post-merge
                    //       approval never gated the merge); and
                    //   (3) the SOURCE review node's OWN resolved valid time EQUALS
                    //       the edge's stamped `author_time` (binding). This ties the
                    //       cited review time to the review row the edge names, so an
                    //       attacker cannot move the review row's time without also
                    //       moving `author_time` — which then fails (2) — and cannot
                    //       forge `author_time` without the review row disagreeing.
                    let Some(t) = temporal.as_ref() else {
                        integrity_passed = false;
                        integrity_detail = format!(
                            "review_coverage edge {} carries no stamped temporal metadata",
                            br.record.id(),
                        );
                        break 'integrity;
                    };
                    let Some(stamped_merged_at) = parse_rfc3339(&t.valid_time)
                        .filter(|_| in_window(&t.valid_time, &pack.manifest.window))
                    else {
                        integrity_passed = false;
                        integrity_detail = format!(
                            "review_coverage edge {} stamped merged_at is not a valid RFC3339 \
                             instant inside the manifest window",
                            br.record.id(),
                        );
                        break 'integrity;
                    };
                    let Some(stamped_review_at) = t.author_time.as_deref().and_then(parse_rfc3339)
                    else {
                        integrity_passed = false;
                        integrity_detail = format!(
                            "review_coverage edge {} carries no valid stamped author_time \
                             (review-time anchor)",
                            br.record.id(),
                        );
                        break 'integrity;
                    };
                    if stamped_review_at > stamped_merged_at {
                        integrity_passed = false;
                        integrity_detail = format!(
                            "review_coverage edge {} stamped review time is after its stamped \
                             merged_at (post-merge approval does not gate the merge)",
                            br.record.id(),
                        );
                        break 'integrity;
                    }
                    // (3) Bind the edge's cited review time to its source review row.
                    // The source node is present and approving (checked above).
                    let source_valid_time = node_by_id
                        .get(source.as_str())
                        .and_then(|rec| resolve_valid_time(rec))
                        .as_deref()
                        .and_then(parse_rfc3339);
                    if source_valid_time != Some(stamped_review_at) {
                        integrity_passed = false;
                        integrity_detail = format!(
                            "review_coverage edge {} source {source} (target {target}) review \
                             valid time does not equal the edge's stamped author_time \
                             (review-time binding violation)",
                            br.record.id(),
                        );
                        break 'integrity;
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
        } else if section.class == EvidenceClass::RemediationLinks.as_wire() {
            // `remediation_links` (issue #340) is a DERIVED join with no backing
            // node kind: its leads ride the section's `log_summary`, never hashed
            // rows (a remediation commit may legitimately fall OUTSIDE the evidence
            // window, so it cannot be a windowed section row). The bounded
            // membership exemption is therefore the tightest possible — the
            // section carries ZERO hashed rows — mirroring `review_coverage`'s
            // bounded exemption so nothing can be smuggled in as a hashed
            // remediation "row" (a `Commit`, a `Symbol`, any node mapping to a real
            // evidence class, or any edge) with counts recomputed.
            if !section.records.is_empty() {
                integrity_passed = false;
                integrity_detail = format!(
                    "remediation_links section carries {} hashed row(s); the derived \
                     join rides `log_summary` and permits no hashed rows",
                    section.records.len(),
                );
                break 'integrity;
            }
        } else if section.class == EvidenceClass::OccurrenceBuckets.as_wire() {
            // `occurrence_buckets` legitimately co-locates the bucket->signature
            // attribution edges (issue #340, Codex round-3 P1): `LogOccurrenceBucket
            // --AGGREGATES--> ErrorSignature`, the hash-bound binding verify
            // re-derives. Those edges map to no evidence class, so the class check
            // cannot apply to them — but the exemption is BOUNDED (mirroring
            // review_coverage): an edge is admitted IFF it is an AGGREGATES edge whose
            // SOURCE is a LogOccurrenceBucket node present in the section. Anything
            // else (a node mapping to a non-occurrence_buckets class, an unrelated
            // edge label, an AGGREGATES edge sourced at a non-present bucket) still
            // fails Integrity, so nothing can be smuggled in as a fake attribution.
            let bucket_node_ids: BTreeSet<&str> = section
                .records
                .iter()
                .filter(|br| {
                    matches!(
                        node_log_payload(&br.record),
                        Some(crate::ir::LogPayload::LogOccurrenceBucket(_))
                    )
                })
                .map(|br| br.record.id())
                .collect();
            for br in &section.records {
                let admitted = match &br.record {
                    GraphRecord::Edge { label, source, .. } => {
                        label.as_str() == "AGGREGATES" && bucket_node_ids.contains(source.as_str())
                    }
                    _ => {
                        evidence_class_for_record(&br.record).map(|c| c.as_wire())
                            == Some(section.class.as_str())
                    }
                };
                if !admitted {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "record {} in section {} is neither an occurrence_buckets record nor \
                         an AGGREGATES attribution edge for a present bucket (section \
                         membership mismatch)",
                        br.record.id(),
                        section.class,
                    );
                    break 'integrity;
                }
            }
        } else if section.class == EvidenceClass::ErrorSignatures.as_wire() {
            // `error_signatures` legitimately co-locates the frame-resolution
            // attribution edges (issue #371): `ErrorSignature --FRAME_RESOLVES_TO-->
            // {Symbol|File|Diagnostic}`, the hash-bound binding verify re-derives each
            // row's `frame_resolution`/`frame_index` from. Those edges map to no
            // evidence class, so the class check cannot apply to them — but the
            // exemption is BOUNDED (mirroring occurrence_buckets/review_coverage): an
            // edge is admitted IFF it is a FRAME_RESOLVES_TO edge whose SOURCE is an
            // ErrorSignature node present in the section. Anything else (a node mapping
            // to a non-error_signatures class, an unrelated edge label, a
            // FRAME_RESOLVES_TO edge sourced at a non-present signature) still fails
            // Integrity, so nothing can be smuggled in as a fake attribution.
            let signature_node_ids: BTreeSet<&str> = section
                .records
                .iter()
                .filter(|br| {
                    matches!(
                        node_log_payload(&br.record),
                        Some(crate::ir::LogPayload::ErrorSignature(_))
                    )
                })
                .map(|br| br.record.id())
                .collect();
            for br in &section.records {
                let admitted = match &br.record {
                    GraphRecord::Edge { label, source, .. } => {
                        label.as_str() == "FRAME_RESOLVES_TO"
                            && signature_node_ids.contains(source.as_str())
                    }
                    _ => {
                        evidence_class_for_record(&br.record).map(|c| c.as_wire())
                            == Some(section.class.as_str())
                    }
                };
                if !admitted {
                    integrity_passed = false;
                    integrity_detail = format!(
                        "record {} in section {} is neither an error_signatures record nor \
                         a FRAME_RESOLVES_TO attribution edge for a present signature (section \
                         membership mismatch)",
                        br.record.id(),
                        section.class,
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
        // Bind the derived log summary (issue #340) so a tampered summary value —
        // e.g. an inflated `in_window_occurrences`, a swapped `template_hash`, or a
        // forged remediation `commit_id`, none of which the per-record/manifest/
        // window/safety checks reach — fails Integrity, mirroring the
        // `review_coverage` `measurement` bind above.
        if let Err(detail) = bind_log_summary_integrity(section, &pack.manifest.window) {
            integrity_passed = false;
            integrity_detail = detail;
            break 'integrity;
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
            // In the `review_coverage` section, a co-located approving-review NODE
            // is a SOURCE-RESOLUTION aid for its coverage edge (round-18 Finding
            // 2), not independently-windowed evidence. Its own valid time is
            // legitimately BEFORE `from` when the approval preceded a merge inside
            // the window (Codex Finding A): a pre-window approval that gated an
            // in-window merge. Such a node cannot be re-stamped without falsifying
            // its genuine review time, so its window relevance is carried by its
            // coverage edge.
            //
            // Codex round-21 (SOUNDNESS HOLE, comprehensive close): a co-located
            // coverage source-review NODE must be admitted ONLY through a coverage
            // `REFERENCES_TASK` edge it sources whose stamped fields themselves
            // prove the gate — NEVER merely for falling in-window, and NOT
            // conditional on the target PR Task being present. Two things went
            // wrong across the four successive round-1..4 holes: (i) the proof was
            // only run when the target Task was present, and (ii) an in-window
            // coverage-source review was silently admitted by the ordinary
            // `from <= t < to` arm below, so a post-merge approval whose row time
            // was moved into the window still passed. The fix derives the proof
            // ENTIRELY from the edge's stamp and BINDS it to the review row:
            //   (1) the edge's `valid_time` (the PR `merged_at`) parses AND is
            //       in-window (`from <= merged_at < to`);
            //   (2) the edge's `author_time` (the review time) parses AND is at or
            //       before that `merged_at`; and
            //   (3) that proven `author_time` is what the review row is matched
            //       against — a coverage-source review is admitted IFF its OWN
            //       resolved valid time EQUALS a proven edge's `author_time`, so
            //       moving the review row's time (in-window post-merge, or anywhere)
            //       without moving the edge's `author_time` (which then fails (2))
            //       is rejected. Because a proven `author_time` is `<= merged_at <
            //       to`, the admitted review time is always below the upper bound;
            //       it may legitimately be BELOW `from` (a pre-window approval that
            //       gated an in-window merge), which is the whole point of the lane.
            let coverage_proven_review_times: BTreeMap<
                &str,
                Vec<chrono::DateTime<chrono::FixedOffset>>,
            > = if section.class == EvidenceClass::ReviewCoverage.as_wire() {
                let mut proven: BTreeMap<&str, Vec<chrono::DateTime<chrono::FixedOffset>>> =
                    BTreeMap::new();
                for br in &section.records {
                    let GraphRecord::Edge {
                        label,
                        source,
                        temporal,
                        ..
                    } = &br.record
                    else {
                        continue;
                    };
                    if label.as_str() != "REFERENCES_TASK" {
                        continue;
                    }
                    // Derive the proof ENTIRELY from the edge's stamped fields.
                    let Some(t) = temporal else { continue };
                    let Some(merged_at) = parse_rfc3339(&t.valid_time) else {
                        continue;
                    };
                    // (1) the merged_at anchor is itself in-window.
                    if !(from <= merged_at && merged_at < to) {
                        continue;
                    }
                    // (2) the cited review time (edge author_time) is at or
                    //     before the merge — a post-merge approval, or a missing
                    //     review time, proves no gate and is skipped.
                    let Some(review_at) = t.author_time.as_deref().and_then(parse_rfc3339) else {
                        continue;
                    };
                    if review_at > merged_at {
                        continue;
                    }
                    // Record the PROVEN review-time anchor (never the merged_at):
                    // (3) binds the review row's own valid time to this value.
                    proven.entry(source.as_str()).or_default().push(review_at);
                }
                proven
            } else {
                BTreeMap::new()
            };
            for br in &section.records {
                let resolved = resolve_valid_time(&br.record).and_then(|vt| parse_rfc3339(&vt));
                // A co-located coverage source-review node is admitted EXCLUSIVELY
                // through a proven edge it sources (never the ordinary in-window
                // arm) — its resolved valid time must EQUAL a proven `author_time`.
                // Every other row (the coverage edges themselves, and rows in
                // class-scoped sections) is held to the ordinary half-open window.
                let admitted = if section.class == EvidenceClass::ReviewCoverage.as_wire()
                    && is_approving_review(&br.record)
                {
                    matches!(resolved, Some(t) if coverage_proven_review_times
                        .get(br.record.id())
                        .is_some_and(|times| times.contains(&t)))
                } else if section.class == EvidenceClass::OccurrenceBuckets.as_wire() {
                    if matches!(&br.record, GraphRecord::Edge { label, .. } if label.as_str() == "AGGREGATES")
                    {
                        // A co-located `AGGREGATES` attribution edge (issue #340,
                        // Codex round-3 P1) carries no valid time of its own; its
                        // window relevance rides the bucket it binds (the bucket row
                        // IS held to the interval rule below). Section membership
                        // already restricts it to edges sourced at a present bucket.
                        true
                    } else {
                        // `occurrence_buckets` bucket rows are admitted by the AC2
                        // interval-intersection rule, NOT the point predicate (issue
                        // #340): a bucket whose hour `[bucket_start, +1h)` intersects
                        // the window is legitimately included even when its
                        // `bucket_start` precedes `from` (a partial-overlap hour is
                        // counted whole). The SAME predicate `assemble_pack` selected
                        // it with, so assemble and verify agree.
                        matches!(resolved, Some(t) if bucket_hour_intersects_window(t, from, to))
                    }
                } else if section.class == EvidenceClass::ErrorSignatures.as_wire()
                    && matches!(&br.record, GraphRecord::Edge { label, .. } if label.as_str() == "FRAME_RESOLVES_TO")
                {
                    // A co-located `FRAME_RESOLVES_TO` attribution edge (issue #371)
                    // carries no valid time of its own; its window relevance rides the
                    // signature it binds (the signature NODE is held to the point
                    // predicate below). Section membership already restricts it to
                    // edges sourced at a present signature.
                    true
                } else {
                    matches!(resolved, Some(t) if from <= t && t < to)
                };
                if !admitted {
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

    // ── issue #340: log-graph incident-evidence fixture builders ───────────────

    use crate::ir::{
        ErrorSignaturePayload, FrameResolution, LOG_SCHEMA_VERSION, LogEventPayload,
        LogOccurrenceBucketPayload, LogPayload, StackFrame,
    };

    /// A benign, redaction-safe normalized template for a signature (NOT raw log
    /// text — these are the bounded excerpts the shipping log domain stores).
    fn log_node(
        id: &str,
        kind: NodeKind,
        summary: String,
        payload: LogPayload,
        valid_time: &str,
    ) -> GraphRecord {
        GraphRecord::node(id.to_owned(), kind, None, None, None, summary)
            .with_domain("log", LOG_SCHEMA_VERSION)
            .with_log(payload)
            .with_valid_time(valid_time.to_owned(), "log_event_timestamp")
    }

    /// An `ErrorSignature` node (issue #320). `frames` optional.
    pub fn error_signature(
        id: &str,
        severity: &str,
        template_excerpt: &str,
        first_seen: &str,
        last_seen: &str,
        occurrence_count: u64,
        frames: Option<Vec<StackFrame>>,
    ) -> GraphRecord {
        log_node(
            id,
            NodeKind::ErrorSignature,
            format!("{severity} signature x{occurrence_count}"),
            LogPayload::ErrorSignature(ErrorSignaturePayload {
                fingerprint_algorithm: "template-v1".to_owned(),
                template_excerpt: template_excerpt.to_owned(),
                severity: severity.to_owned(),
                occurrence_count,
                first_seen: first_seen.to_owned(),
                last_seen: last_seen.to_owned(),
                frames,
            }),
            first_seen,
        )
    }

    /// A `LogEvent` exemplar node whose `event_excerpt` carries a caller-supplied
    /// (possibly sentinel) string — used to prove exemplar text NEVER enters the
    /// assembled pack (only content-addressed handles do).
    pub fn log_event(
        id: &str,
        severity: &str,
        event_excerpt: &str,
        content_hash: &str,
        source_line: u64,
        valid_time: &str,
    ) -> GraphRecord {
        log_node(
            id,
            NodeKind::LogEvent,
            format!("{severity} event at line {source_line}"),
            LogPayload::LogEvent(LogEventPayload {
                event_excerpt: event_excerpt.to_owned(),
                event_content_hash: content_hash.to_owned(),
                source_line,
                severity: severity.to_owned(),
            }),
            valid_time,
        )
    }

    /// An hourly `LogOccurrenceBucket` node (issue #320).
    pub fn occurrence_bucket(id: &str, bucket_start: &str, occurrence_count: u64) -> GraphRecord {
        log_node(
            id,
            NodeKind::LogOccurrenceBucket,
            format!("bucket {bucket_start} x{occurrence_count}"),
            LogPayload::LogOccurrenceBucket(LogOccurrenceBucketPayload {
                bucket_start: bucket_start.to_owned(),
                bucket_width: "1h".to_owned(),
                occurrence_count,
                // Fixtures assign bucket record IDs explicitly, so the payload
                // source_id (identity input since #361) is a fixed placeholder;
                // dedup/coalesce keys on the record ID, not this field.
                source_id: "log:v2:fixture-source".to_owned(),
            }),
            bucket_start,
        )
    }

    /// A `LogEvent --FINGERPRINTED_AS--> ErrorSignature` edge.
    pub fn fingerprinted_as(event_id: &str, signature_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::FingerprintedAs,
            event_id.to_owned(),
            signature_id.to_owned(),
            None,
            "LogEvent fingerprinted as ErrorSignature".to_owned(),
        )
    }

    /// A `LogOccurrenceBucket --AGGREGATES--> ErrorSignature` edge.
    pub fn aggregates(bucket_id: &str, signature_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::Aggregates,
            bucket_id.to_owned(),
            signature_id.to_owned(),
            None,
            "LogOccurrenceBucket aggregates ErrorSignature".to_owned(),
        )
    }

    /// An `ErrorSignature --FRAME_RESOLVES_TO--> Symbol` edge (issue #322).
    pub fn frame_resolves_to(
        signature_id: &str,
        target_id: &str,
        resolution: FrameResolution,
        frame_index: u32,
    ) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::FrameResolvesTo,
            signature_id.to_owned(),
            target_id.to_owned(),
            None,
            "ErrorSignature frame resolves to symbol".to_owned(),
        )
        .with_frame_resolution(resolution)
        .with_frame_index(frame_index)
    }

    /// A `Symbol` node.
    pub fn symbol(id: &str, path: &str, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    /// A `Symbol --CHANGED_IN--> Commit` edge stamped with the commit's temporal.
    pub fn changed_in(symbol_id: &str, commit_id: &str, vt: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::ChangedIn,
            symbol_id.to_owned(),
            commit_id.to_owned(),
            Some("1.0".to_owned()),
            "symbol changed in commit".to_owned(),
        )
        .with_temporal(TemporalMetadata {
            git_commit: format!("sha-{commit_id}"),
            git_parent_commits: Vec::new(),
            valid_time: vt.to_owned(),
            author_time: Some(vt.to_owned()),
            observed_at: vt.to_owned(),
            valid_time_source: Some("git_committer".to_owned()),
        })
    }

    /// A `Commit --VALIDATED_BY--> Verification` edge linking passing verification
    /// evidence to a commit (direction-agnostic in the remediation derivation).
    pub fn validated_by(commit_id: &str, verification_id: &str) -> GraphRecord {
        GraphRecord::edge(
            EdgeLabel::ValidatedBy,
            commit_id.to_owned(),
            verification_id.to_owned(),
            None,
            "commit validated by verification run".to_owned(),
        )
    }

    /// A passing verification (`CommandRun`) node.
    pub fn verification_run(id: &str, executed: &str) -> GraphRecord {
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

    /// A `Commit` node (public alias of the private helper).
    pub fn commit_node(id: &str, vt: &str) -> GraphRecord {
        commit(id, vt)
    }

    /// Sentinel string planted ONLY in exemplar `event_excerpt` fields; the
    /// assembled pack must NEVER contain it (exemplars surface as handles+hashes).
    pub const EXEMPLAR_SENTINEL: &str = "SENTINEL_RAW_EXEMPLAR_TEXT_9f3a1c";

    /// Builds a deterministic log-graph incident-evidence fixture (issue #340).
    ///
    /// Three distinct signatures, >=48 hourly buckets (incl. out-of-window
    /// buckets), exemplar `LogEvent`s carrying the raw-text sentinel, and a
    /// planted `sig1 -> symbol -> in-window commit -> passing verification`
    /// remediation chain. Window is March 2026 (`WINDOW_FROM`/`WINDOW_TO`).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn build_log_incident_records() -> Vec<GraphRecord> {
        let mut records: Vec<GraphRecord> = Vec::new();

        // ── Three distinct signatures ─────────────────────────────────────────
        // sig1: last_seen extends PAST the window `to` (upper-clip test) and
        //       carries backtrace frames (frame_chain_hash + remediation anchor).
        let frames = vec![
            StackFrame {
                frame_index: 0,
                module_path: Some("app::db".to_owned()),
                file_path: Some("src/db.rs".to_owned()),
                line: Some(42),
            },
            StackFrame {
                frame_index: 1,
                module_path: Some("app::main".to_owned()),
                file_path: Some("src/main.rs".to_owned()),
                line: Some(10),
            },
        ];
        records.push(error_signature(
            "log:v1:sig1",
            "error",
            "connection refused to HOST",
            &march(2, 9),           // first_seen in-window
            "2026-04-05T09:00:00Z", // last_seen AFTER `to` -> clipped
            120,
            Some(frames),
        ));
        // sig2: entirely in-window, no frames.
        records.push(error_signature(
            "log:v1:sig2",
            "warn",
            "deprecated config key KEY",
            &march(3, 10),
            &march(20, 10),
            30,
            None,
        ));
        // sig3: entirely in-window.
        records.push(error_signature(
            "log:v1:sig3",
            "fatal",
            "panic at NUMBER",
            &march(4, 11),
            &march(4, 15),
            3,
            None,
        ));

        // ── Exemplars (LogEvent), carrying the raw-text sentinel ──────────────
        for (sig, line) in [
            ("log:v1:sig1", 42u64),
            ("log:v1:sig2", 7),
            ("log:v1:sig3", 99),
        ] {
            let content_hash = "ab".repeat(32); // 64-hex benign content hash
            let ev_id = format!("log:v1:ev-{}", sig.trim_start_matches("log:v1:"));
            records.push(log_event(
                &ev_id,
                "error",
                &format!("{EXEMPLAR_SENTINEL} raw line for {sig} value=hunter2"),
                &content_hash,
                line,
                &march(5, 12),
            ));
            records.push(fingerprinted_as(&ev_id, sig));
        }

        // ── >=48 hourly buckets: 16 per signature ─────────────────────────────
        // sig1: a partial-overlap bucket that STARTS one hour BEFORE `from`
        // (2026-02-28T23:00) whose hour [23:00, 00:00) touches `from` boundary —
        // actually intersects [from, to) only if hour_end > from; hour_end =
        // 2026-03-01T00:00 == from, so it does NOT intersect (half-open). Use a
        // bucket at 2026-02-28T23:30? Buckets are hour-floored, so instead plant a
        // bucket at exactly `from`'s hour minus 30m is impossible. Model the
        // partial-overlap case with a window that is NOT hour-aligned in the
        // dedicated test; here every bucket is hour-aligned.
        // 15 in-window hourly buckets for sig1 (Mar 2, hours 0..15), counts 1..15.
        for h in 0..15u32 {
            let start = march(2, h);
            let bid = format!("log:v1:b1-{h:02}");
            records.push(occurrence_bucket(&bid, &start, u64::from(h) + 1));
            records.push(aggregates(&bid, "log:v1:sig1"));
        }
        // 1 OUT-OF-window bucket for sig1 (Feb 20) — must NOT be summed.
        records.push(occurrence_bucket(
            "log:v1:b1-oobefore",
            "2026-02-20T09:00:00Z",
            999,
        ));
        records.push(aggregates("log:v1:b1-oobefore", "log:v1:sig1"));
        // 1 OUT-OF-window bucket for sig1 (Apr 10) — must NOT be summed.
        records.push(occurrence_bucket(
            "log:v1:b1-ooafter",
            "2026-04-10T09:00:00Z",
            888,
        ));
        records.push(aggregates("log:v1:b1-ooafter", "log:v1:sig1"));

        // 16 in-window hourly buckets for sig2 and sig3 (Mar 3 / Mar 4).
        for h in 0..16u32 {
            let b2 = format!("log:v1:b2-{h:02}");
            records.push(occurrence_bucket(&b2, &march(3, h), 2));
            records.push(aggregates(&b2, "log:v1:sig2"));
            let b3 = format!("log:v1:b3-{h:02}");
            records.push(occurrence_bucket(&b3, &march(4, h), 5));
            records.push(aggregates(&b3, "log:v1:sig3"));
        }

        // ── Remediation chain: sig1 -> symbol -> in-window commit -> verify ────
        records.push(symbol("codegraph:v5:sym-db", "src/db.rs", "connect"));
        records.push(frame_resolves_to(
            "log:v1:sig1",
            "codegraph:v5:sym-db",
            FrameResolution::Resolved,
            0,
        ));
        // Fix commit valid time is AFTER sig1 first_seen and IN-window.
        records.push(commit_node("codegraph:v5:fixc", &march(10, 9)));
        records.push(changed_in(
            "codegraph:v5:sym-db",
            "codegraph:v5:fixc",
            &march(10, 9),
        ));
        records.push(verification_run("verification:v1:fixver", &march(10, 10)));
        records.push(validated_by("codegraph:v5:fixc", "verification:v1:fixver"));

        records
    }

    /// The three planted signature record IDs.
    pub const LOG_SIGNATURE_IDS: [&str; 3] = ["log:v1:sig1", "log:v1:sig2", "log:v1:sig3"];
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

    /// Codex Finding 2: an approving review submitted BEFORE the window `from`
    /// (but at or before the PR's `merged_at`) legitimately gated the merge and
    /// MUST count as coverage. The reporting window bounds which PRs are in scope
    /// (via `merged_at`), not which approvals count. Before the fix the review's
    /// own out-of-window position dropped it, wrongly classifying a PR approved
    /// near a period boundary as `uncovered`.
    #[test]
    fn approval_before_window_but_before_merge_counts_as_covered() {
        use super::fixture::{pr, references_task, review};
        // PR merges inside the March window; its sole approving review was
        // submitted in February — before `from` (2026-03-01) yet before the merge.
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            review("project:v1:rvX", "2026-02-20T08:00:00Z", "approved"),
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
            "pre-window approval before merge must suppress the gap: gaps={:?}",
            pack.gaps
        );
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.merged_pr_count, 1);
        assert_eq!(
            rc.approved_pr_count, 1,
            "a review before the window but before merge legitimately gated it"
        );
        assert!(rc.unapproved_pr_ids.is_empty());
    }

    /// Codex Finding A: a pack assembled over a PR merged IN-WINDOW whose sole
    /// approving review was submitted BEFORE the window `from` (but before the
    /// merge) must PASS offline `verify_pack`. The coverage edge / section row
    /// stamps its window-relevant valid time from the PR's in-window `merged_at`,
    /// NOT the pre-window review valid time, so the section row falls inside the
    /// manifest window and Window-consistency holds. Before the fix the edge was
    /// stamped with the pre-window review valid time, so the freshly-assembled
    /// pack for the newly-supported pre-window-approval case failed verification.
    #[test]
    fn pre_window_approval_pack_passes_verify() {
        use super::fixture::{pr, references_task, review};
        // PR merges inside the March window; its sole approving review was
        // submitted in February — before `from` (2026-03-01) yet before the merge.
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            review("project:v1:rvX", "2026-02-20T08:00:00Z", "approved"),
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

        // The PR is covered (round-1 behavior) ...
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .and_then(|s| s.measurement.as_ref())
            .expect("review_coverage measurement");
        assert_eq!(rc.approved_pr_count, 1);

        // ... and the freshly-assembled pack must verify clean, including
        // Window-consistency over the coverage section row.
        let report = verify_pack(&pack);
        assert!(
            report.window_consistency.passed,
            "pre-window-approval coverage row must be stamped in-window: {:?}",
            report.window_consistency
        );
        assert!(
            report.ok,
            "pre-window-approval pack must verify clean: {report:?}"
        );

        // The coverage edge's stamped valid time is the in-window merged_at, while
        // the pre-window review valid time is preserved as a cited field.
        let section = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        let edge = section
            .records
            .iter()
            .find(|br| {
                matches!(&br.record,
                GraphRecord::Edge { label, .. } if label.as_str() == "REFERENCES_TASK")
            })
            .expect("coverage edge row");
        let vt = resolve_valid_time(&edge.record).expect("edge valid time");
        assert_eq!(
            vt, "2026-03-15T12:00:00Z",
            "edge valid time must be the PR merged_at (in-window)"
        );
        if let GraphRecord::Edge { temporal, .. } = &edge.record {
            assert_eq!(
                temporal.as_ref().and_then(|t| t.author_time.clone()),
                Some("2026-02-20T08:00:00Z".to_owned()),
                "the pre-window review valid time is preserved as a cited field"
            );
        }
    }

    /// Round-20 legit case: a PR merged IN-WINDOW whose approving review predates
    /// the window is covered even when the PR Task itself is WINDOWED OUT of the
    /// pack (its `valid_time` falls outside the window, so it rides no section).
    /// The coverage edge carries the in-window `merged_at` anchor and the
    /// pre-window review time, so Window-consistency must PASS purely from the
    /// edge's stamped fields — the target Task record is legitimately absent and
    /// the exemption proof must not depend on it.
    #[test]
    fn pre_window_approval_windowed_out_target_passes_verify() {
        use super::fixture::{pr_with_merge_time, references_task, review};
        // PR's Task valid_time (Feb 10) is OUT of the March window, so it is
        // windowed out of every section; its merge (March 15) is IN-window and its
        // sole approving review (Feb 20) precedes the merge.
        let records = vec![
            pr_with_merge_time(
                "project:v1:prX",
                "2026-02-10T00:00:00Z",
                "2026-03-15T12:00:00Z",
                "cX",
            ),
            review("project:v1:rvX", "2026-02-20T08:00:00Z", "approved"),
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

        // The PR is covered even though its Task is windowed out of every section.
        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        assert_eq!(
            rc.measurement
                .as_ref()
                .expect("measurement")
                .approved_pr_count,
            1
        );
        assert!(
            !pack.sections.iter().any(|s| s
                .records
                .iter()
                .any(|br| br.record.id() == "project:v1:prX")),
            "the merged PR Task is windowed out (absent from every section)"
        );

        let report = verify_pack(&pack);
        assert!(
            report.window_consistency.passed,
            "windowed-out-target pre-window-approval pack must pass Window-consistency \
             from the edge's own stamped fields: {:?}",
            report.window_consistency
        );
        assert!(
            report.ok,
            "windowed-out-target pack must verify clean: {report:?}"
        );
    }

    /// Codex round-20 P2 (SOUNDNESS HOLE): the lower-bound Window-consistency
    /// exemption for a pre-window approving-review NODE must require a
    /// SELF-CONTAINED merge proof from its coverage edge's OWN stamped timestamps
    /// — the edge's `valid_time` (the PR `merged_at`) in-window AND the edge's
    /// `author_time` (the review time) at or before that `merged_at`. Here the
    /// coverage edge's `author_time` is forged to POST-DATE the merge while the
    /// target PR Task is windowed out (absent), so the integrity target-present
    /// merge proof never runs. Before the fix the review NODE is exempted merely
    /// because it sources an included edge and the pack verifies clean (the hole);
    /// after the fix the unproven edge grants no exemption and the pre-window
    /// review fails Window-consistency.
    #[test]
    fn tampered_post_merge_review_time_on_windowed_out_target_fails_verify() {
        use super::fixture::{pr_with_merge_time, references_task, review};
        let records = vec![
            pr_with_merge_time(
                "project:v1:prX",
                "2026-02-10T00:00:00Z",
                "2026-03-15T12:00:00Z",
                "cX",
            ),
            review("project:v1:rvX", "2026-02-20T08:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
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

        // Forge the coverage edge's `author_time` (the cited review time) to
        // 2026-03-25 — AFTER the PR's 2026-03-15 `merged_at`. The edge's
        // `valid_time` (merged_at) stays in-window so the edge row still clears its
        // own window check; only the self-contained "review gated the merge" proof
        // is now violated. Recompute the row hash so Integrity is untouched.
        let mut touched = false;
        for section in &mut pack.sections {
            if section.class != "review_coverage" {
                continue;
            }
            for br in &mut section.records {
                if let GraphRecord::Edge {
                    label, temporal, ..
                } = &mut br.record
                    && label.as_str() == "REFERENCES_TASK"
                {
                    let t = temporal.as_mut().expect("stamped coverage edge");
                    t.author_time = Some("2026-03-25T08:00:00Z".to_owned());
                    br.hash = blake3::hash(serde_json::to_string(&br.record).unwrap().as_bytes())
                        .to_string();
                    touched = true;
                }
            }
            section
                .records
                .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
        }
        assert!(touched, "coverage edge present to tamper");

        let report = verify_pack(&pack);
        assert!(
            !report.window_consistency.passed,
            "a pre-window review whose coverage edge does not prove it gated the \
             in-window merge (author_time post-dates merged_at) must fail \
             Window-consistency: {:?}",
            report.window_consistency
        );
        assert!(
            report.window_consistency.detail.contains("project:v1:rvX"),
            "detail names the unproven review node: {}",
            report.window_consistency.detail
        );
        assert!(!report.ok, "overall verdict fails");
    }

    // ---------------------------------------------------------------------
    // Codex round-21 COMPREHENSIVE CLASS COVERAGE: `verify_pack` must reject
    // EVERY variant of a coverage edge whose merge/review proof is falsified,
    // whether or not the target PR Task is windowed out. These tests exercise
    // the whole class (target-independent stamped-edge proof + review-row
    // binding + coverage-source Window-consistency binding), so no fifth
    // adjacent variant can exist. Vectors a-e reject; f/g (existing) and h pass.
    // ---------------------------------------------------------------------

    /// A pack whose sole covered PR has its Task WINDOWED OUT of every section
    /// (Task `valid_time` in February, merge in-window in March, approval in
    /// February before merge). The coverage edge and its source review node are
    /// the only `review_coverage` rows; the target PR Task is absent.
    fn windowed_out_target_pack() -> EvidencePack {
        use super::fixture::{pr_with_merge_time, references_task, review};
        let records = vec![
            pr_with_merge_time(
                "project:v1:prX",
                "2026-02-10T00:00:00Z",
                "2026-03-15T12:00:00Z",
                "cX",
            ),
            review("project:v1:rvX", "2026-02-20T08:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
        assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles")
    }

    /// A pack whose sole covered PR Task is PRESENT and merged in-window, with an
    /// in-window approval before the merge — the ordinary happy path (vector h).
    fn present_target_pack() -> EvidencePack {
        use super::fixture::{pr, references_task, review};
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            review("project:v1:rvX", "2026-03-15T08:00:00Z", "approved"),
            references_task("project:v1:rvX", "project:v1:prX"),
        ];
        assemble_pack(
            &records,
            &load_default_catalog(),
            "CC8.1",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles")
    }

    /// Rehashes + canonically re-sorts the `review_coverage` section after an
    /// in-place tamper, so the ONLY thing that can fail `verify_pack` is the
    /// merge/review proof under test (never a stale hash, count, or ordering).
    /// Record ids / domains / kinds are unchanged by these tampers, so the
    /// manifest aggregates stay valid without recompute.
    fn rehash_review_coverage_section(pack: &mut EvidencePack) {
        for section in &mut pack.sections {
            if section.class != "review_coverage" {
                continue;
            }
            for br in &mut section.records {
                br.hash =
                    blake3::hash(serde_json::to_string(&br.record).unwrap().as_bytes()).to_string();
            }
            section
                .records
                .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
        }
    }

    /// Sets a co-located review NODE's `valid_time` in the `review_coverage` section.
    fn set_review_node_valid_time(pack: &mut EvidencePack, review_id: &str, vt: &str) {
        let mut touched = false;
        for section in &mut pack.sections {
            if section.class != "review_coverage" {
                continue;
            }
            for br in &mut section.records {
                if br.record.id() == review_id
                    && let GraphRecord::Node { valid_time, .. } = &mut br.record
                {
                    *valid_time = Some(vt.to_owned());
                    touched = true;
                }
            }
        }
        assert!(touched, "review node {review_id} present to retime");
        rehash_review_coverage_section(pack);
    }

    /// Sets the coverage `REFERENCES_TASK` edge's stamped `valid_time` (`merged_at`
    /// anchor) and/or `author_time` (review-time anchor) in the `review_coverage`
    /// section. `None` leaves that field as stamped.
    fn set_coverage_edge_stamps(
        pack: &mut EvidencePack,
        new_valid_time: Option<&str>,
        new_author_time: Option<&str>,
    ) {
        let mut touched = false;
        for section in &mut pack.sections {
            if section.class != "review_coverage" {
                continue;
            }
            for br in &mut section.records {
                if let GraphRecord::Edge {
                    label, temporal, ..
                } = &mut br.record
                    && label.as_str() == "REFERENCES_TASK"
                {
                    let t = temporal.as_mut().expect("stamped coverage edge");
                    if let Some(v) = new_valid_time {
                        v.clone_into(&mut t.valid_time);
                        v.clone_into(&mut t.observed_at);
                    }
                    if let Some(a) = new_author_time {
                        t.author_time = Some(a.to_owned());
                    }
                    touched = true;
                }
            }
        }
        assert!(touched, "coverage edge present to retime");
        rehash_review_coverage_section(pack);
    }

    /// VECTOR a (the confirmed round-4 finding). Target ABSENT + source review row
    /// moved to an IN-WINDOW time AFTER the edge's stamped merge time (a post-merge
    /// approval whose row time is smuggled into the window). The edge's stamped
    /// `author_time` is left at the honest pre-window value, so the review row and
    /// the edge disagree. Before the fix the review row was admitted by the ordinary
    /// in-window Window-consistency arm and Integrity skipped the proof (absent
    /// target); after the fix both the binding and the coverage-source admission
    /// reject it.
    #[test]
    fn absent_target_review_row_moved_in_window_post_merge_fails_verify() {
        let mut pack = windowed_out_target_pack();
        assert!(verify_pack(&pack).ok, "baseline windowed-out pack verifies");
        // rvX approved Feb 20; PR merged March 15. Move rvX in-window to March 20
        // (post-merge). Edge author_time stays Feb 20.
        set_review_node_valid_time(&mut pack, "project:v1:rvX", "2026-03-20T08:00:00Z");
        let report = verify_pack(&pack);
        assert!(
            !report.ok,
            "an in-window post-merge review row on an absent-target coverage edge \
             must fail verify: {report:?}"
        );
        assert!(
            !report.integrity.passed || !report.window_consistency.passed,
            "the merge/review proof must reject it in Integrity or Window-consistency: {report:?}"
        );
    }

    /// VECTOR b (binding violation). Target ABSENT + edge `author_time` forged to a
    /// value at-or-before `merged_at`, but the SOURCE review row's real valid time
    /// DIFFERS from that forged `author_time`. The edge cannot claim a review time
    /// the review row it cites does not corroborate.
    #[test]
    fn absent_target_edge_author_time_unbound_from_review_row_fails_verify() {
        let mut pack = windowed_out_target_pack();
        assert!(verify_pack(&pack).ok, "baseline windowed-out pack verifies");
        // Forge author_time to March 1 (<= March 15 merge) while the review row
        // stays at Feb 20 — the edge and the row now disagree.
        set_coverage_edge_stamps(&mut pack, None, Some("2026-03-01T00:00:00Z"));
        let report = verify_pack(&pack);
        assert!(
            !report.ok,
            "a coverage edge whose author_time disagrees with its source review row \
             must fail verify: {report:?}"
        );
        assert!(
            !report.integrity.passed || !report.window_consistency.passed,
            "the binding must reject it in Integrity or Window-consistency: {report:?}"
        );
    }

    /// VECTOR c (merge anchor forged out of window). Target ABSENT + edge
    /// `valid_time` (`merged_at` anchor) forged to `>= to`. The covered PR is supposed
    /// to have merged in-window; an out-of-window merge anchor proves nothing.
    #[test]
    fn absent_target_edge_merged_at_forged_out_of_window_fails_verify() {
        let mut pack = windowed_out_target_pack();
        assert!(verify_pack(&pack).ok, "baseline windowed-out pack verifies");
        // Forge merged_at to April 15 (>= window `to` 2026-04-01). Keep author_time
        // <= that so only the in-window anchor gate is violated.
        set_coverage_edge_stamps(&mut pack, Some("2026-04-15T12:00:00Z"), None);
        let report = verify_pack(&pack);
        assert!(
            !report.ok,
            "a coverage edge whose merged_at anchor is outside the window must fail \
             verify: {report:?}"
        );
        assert!(
            !report.integrity.passed || !report.window_consistency.passed,
            "the in-window merge-anchor gate must reject it: {report:?}"
        );
    }

    /// VECTOR d (honest edge, post-merge approval). Target ABSENT + edge
    /// `author_time` > `merged_at` while the review row equals `author_time` (a
    /// self-consistent but post-merge approval). Mirrors the existing round-20
    /// windowed-out test; kept here so the whole class is guarded in one place.
    #[test]
    fn absent_target_post_merge_author_time_fails_verify() {
        let mut pack = windowed_out_target_pack();
        assert!(verify_pack(&pack).ok, "baseline windowed-out pack verifies");
        // Move BOTH the edge author_time and the review row to March 25 (post the
        // March 15 merge) so they agree but the approval did not gate the merge.
        set_coverage_edge_stamps(&mut pack, None, Some("2026-03-25T08:00:00Z"));
        set_review_node_valid_time(&mut pack, "project:v1:rvX", "2026-03-25T08:00:00Z");
        let report = verify_pack(&pack);
        assert!(
            !report.ok,
            "a self-consistent post-merge approval on an absent-target edge must fail \
             verify: {report:?}"
        );
        assert!(
            !report.integrity.passed || !report.window_consistency.passed,
            "the at-or-before-merge gate must reject it: {report:?}"
        );
    }

    /// VECTOR e(a) — the vector-a attack with the target PR Task PRESENT. Guards the
    /// present-target path against the same in-window post-merge review-row smuggle.
    #[test]
    fn present_target_review_row_moved_post_merge_fails_verify() {
        let mut pack = present_target_pack();
        assert!(
            verify_pack(&pack).ok,
            "baseline present-target pack verifies"
        );
        // PR merged March 15; move the review row to March 20 (post-merge, still
        // in-window). Edge author_time stays at the honest March 15 08:00.
        set_review_node_valid_time(&mut pack, "project:v1:rvX", "2026-03-20T08:00:00Z");
        let report = verify_pack(&pack);
        assert!(
            !report.ok,
            "an in-window post-merge review row must fail verify even when the target \
             PR Task is present: {report:?}"
        );
        assert!(
            !report.integrity.passed || !report.window_consistency.passed,
            "the proof must reject it in Integrity or Window-consistency: {report:?}"
        );
    }

    /// VECTOR e(d) — the vector-d attack with the target PR Task PRESENT.
    #[test]
    fn present_target_post_merge_author_time_fails_verify() {
        let mut pack = present_target_pack();
        assert!(
            verify_pack(&pack).ok,
            "baseline present-target pack verifies"
        );
        // Move both edge author_time and review row to March 20 (post the March 15
        // merge). They agree, but the approval post-dates the merge.
        set_coverage_edge_stamps(&mut pack, None, Some("2026-03-20T08:00:00Z"));
        set_review_node_valid_time(&mut pack, "project:v1:rvX", "2026-03-20T08:00:00Z");
        let report = verify_pack(&pack);
        assert!(
            !report.ok,
            "a self-consistent post-merge approval must fail verify even when the \
             target PR Task is present: {report:?}"
        );
        assert!(
            !report.integrity.passed || !report.window_consistency.passed,
            "the at-or-before-merge gate must reject it: {report:?}"
        );
    }

    /// VECTOR h (legit happy path). A normal in-window approval with the target PR
    /// Task present must verify clean.
    #[test]
    fn present_target_in_window_approval_passes_verify() {
        let pack = present_target_pack();
        let report = verify_pack(&pack);
        assert!(
            report.ok,
            "an ordinary in-window approval with a present target must verify clean: {report:?}"
        );
        assert!(report.integrity.passed && report.window_consistency.passed);
    }

    /// Codex Finding B(i): a PR merged in-window whose sole approving review was
    /// submitted BEFORE the window `from` (but before merge) and carries no
    /// `review_commit_sha` must STILL yield a `review_unanchored_no_commit_sha`
    /// gap. Coverage counts the pre-window approval, so the #334 gap join must use
    /// the SAME at-or-before-merge filter (not require the review in-window) or
    /// coverage and gaps diverge: the PR would read as covered with no #334 defect.
    #[test]
    fn pre_window_approval_unanchored_still_yields_gap() {
        use super::fixture::{pr, references_task, review};
        // Anchor another review so the #334 reviewed-commit facts are PRESENT in
        // the store (the capability probe requires at least one real fact).
        let anchored = |mut r: GraphRecord, sha: &str| -> GraphRecord {
            if let GraphRecord::Node {
                review_commit_sha, ..
            } = &mut r
            {
                *review_commit_sha = Some(sha.to_owned());
            }
            r
        };
        let head_of = |p: &GraphRecord| -> String {
            match p {
                GraphRecord::Node {
                    head_sha: Some(h), ..
                } => h.clone(),
                _ => panic!("no head_sha"),
            }
        };
        // prA: approved by an ANCHORED-at-head review (facts present, no gap).
        let pr_a = pr("project:v1:prA", "2026-03-15T12:00:00Z", "cA");
        let rv_a = anchored(
            review("project:v1:rvA", "2026-03-15T08:00:00Z", "approved"),
            &head_of(&pr_a),
        );
        // prB: merged in-window, approved by a PRE-WINDOW UNANCHORED review → gap.
        let pr_b = pr("project:v1:prB", "2026-03-16T12:00:00Z", "cB");
        let rv_b = review("project:v1:rvB", "2026-02-16T08:00:00Z", "approved");
        let records = vec![
            pr_a,
            rv_a,
            references_task("project:v1:rvA", "project:v1:prA"),
            pr_b,
            rv_b,
            references_task("project:v1:rvB", "project:v1:prB"),
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
        let unanchored: Vec<&GapRow> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "review_unanchored_no_commit_sha")
            .collect();
        assert_eq!(
            unanchored.len(),
            1,
            "pre-window unanchored approval must still yield a gap: {:?}",
            pack.gaps
        );
        assert!(
            unanchored[0]
                .record_ids
                .contains(&"project:v1:prB".to_owned())
        );
        assert!(
            unanchored[0]
                .record_ids
                .contains(&"project:v1:rvB".to_owned())
        );
    }

    /// Codex Finding B(ii): a PR merged in-window whose sole approving review was
    /// submitted BEFORE the window `from` (but before merge) and is anchored to a
    /// commit OTHER than the PR's final head must STILL yield an
    /// `approval_precedes_final_head` gap — the #334 gap join uses the same
    /// at-or-before-merge filter coverage uses.
    #[test]
    fn pre_window_approval_stale_head_still_yields_gap() {
        use super::fixture::{pr, references_task, review};
        let anchored = |mut r: GraphRecord, sha: &str| -> GraphRecord {
            if let GraphRecord::Node {
                review_commit_sha, ..
            } = &mut r
            {
                *review_commit_sha = Some(sha.to_owned());
            }
            r
        };
        // prC: merged in-window, approved by a PRE-WINDOW review anchored to a
        // NON-head commit → stale.
        let pr_c = pr("project:v1:prC", "2026-03-17T12:00:00Z", "cC");
        let rv_c = anchored(
            review("project:v1:rvC", "2026-02-17T08:00:00Z", "approved"),
            "not-the-final-head",
        );
        let records = vec![
            pr_c,
            rv_c,
            references_task("project:v1:rvC", "project:v1:prC"),
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
        let precedes: Vec<&GapRow> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "approval_precedes_final_head")
            .collect();
        assert_eq!(
            precedes.len(),
            1,
            "pre-window stale-head approval must still yield a gap: {:?}",
            pack.gaps
        );
        assert!(
            precedes[0]
                .record_ids
                .contains(&"project:v1:prC".to_owned())
        );
        assert!(
            precedes[0]
                .record_ids
                .contains(&"project:v1:rvC".to_owned())
        );
    }

    /// Codex Finding 1: a merged `github_pr` whose `merged_at` is present but
    /// UNPARSEABLE has no window-resolvable merge time. It must be routed to the
    /// counted `excluded_unresolvable_merge_time` diagnostic, never silently
    /// dropped as if it were merely merged out of window. A PR whose `merged_at`
    /// parses but falls outside the window stays correctly excluded WITHOUT a
    /// diagnostic.
    #[test]
    fn malformed_merged_at_is_counted_unresolvable_not_dropped() {
        use super::fixture::pr_with_merge_time;
        let records = vec![
            // Present-but-malformed merged_at on a merged github_pr.
            pr_with_merge_time(
                "project:v1:prBad",
                "2026-03-15T08:00:00Z", // updated_at -> Task valid_time
                "not-a-timestamp",      // merged_at -> unparseable merge time
                "cBad",
            ),
            // Parseable merge time cleanly outside the window: excluded, NOT a
            // diagnostic (guards against over-counting).
            pr_with_merge_time(
                "project:v1:prOut",
                "2026-03-15T08:00:00Z",
                "2026-02-15T12:00:00Z", // before window
                "cOut",
            ),
        ];
        let derivation = derive_review_coverage(
            &records,
            &win(),
            ReviewCoverageOptions {
                require_non_author: false,
                require_final_head: false,
            },
        );

        assert!(
            derivation
                .excluded_unresolvable_merge_time
                .contains(&"project:v1:prBad".to_owned()),
            "PR with malformed merged_at must be counted unresolvable: {:?}",
            derivation.excluded_unresolvable_merge_time
        );
        assert!(
            !derivation
                .merged_pr_ids
                .contains(&"project:v1:prBad".to_owned()),
            "PR with malformed merged_at must not be in the merged set"
        );
        // The out-of-window (but parseable) PR is excluded WITHOUT a diagnostic.
        assert!(
            !derivation
                .excluded_unresolvable_merge_time
                .contains(&"project:v1:prOut".to_owned()),
            "a resolvable out-of-window merge time must not be diagnosed unresolvable"
        );
        assert!(
            !derivation
                .merged_pr_ids
                .contains(&"project:v1:prOut".to_owned())
        );
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

    /// Issue #339 AC7 (SHARED-IMPLEMENTATION INVARIANT): #338's evidence-pack
    /// `review_coverage` section MUST embed the identical derivation the
    /// `eg audit review-coverage` lane uses — a single shared implementation.
    /// This asserts, on the SAME store, that the pack's covered PR set (merged
    /// minus the measurement's `unapproved_pr_ids`) and the pack section's
    /// distinct coverage-edge targets both equal the shared derivation's covered
    /// set under the pack's own (lenient) options, with ZERO divergence.
    #[test]
    fn review_coverage_shared_impl_matches_audit_lane() {
        let records = build_seed_records();
        let pack = assemble_cc81();

        // The pack uses lenient options; run the SHARED derivation the audit lane
        // calls with those same options.
        let derivation = derive_review_coverage(
            &records,
            &win(),
            ReviewCoverageOptions {
                require_non_author: false,
                require_final_head: false,
            },
        );

        let rc = pack
            .sections
            .iter()
            .find(|s| s.class == "review_coverage")
            .expect("review_coverage section");
        let m = rc.measurement.as_ref().expect("measurement");

        // Pack covered set = merged minus unapproved.
        let unapproved: BTreeSet<&str> = m.unapproved_pr_ids.iter().map(String::as_str).collect();
        let pack_covered: BTreeSet<String> = derivation
            .merged_pr_ids
            .iter()
            .filter(|id| !unapproved.contains(id.as_str()))
            .cloned()
            .collect();
        assert_eq!(
            pack_covered, derivation.covered_pr_ids,
            "pack covered set must equal the shared derivation covered set (0 divergence)"
        );

        // The pack section's distinct coverage-edge targets equal the covered set.
        let section_targets: BTreeSet<String> = rc
            .records
            .iter()
            .filter_map(|br| match &br.record {
                GraphRecord::Edge { target, .. } => Some(target.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            section_targets, derivation.covered_pr_ids,
            "pack coverage-edge targets must equal the shared derivation covered set"
        );

        // And the measurement's counts agree with the shared derivation.
        assert_eq!(m.merged_pr_count, derivation.merged_pr_ids.len());
        assert_eq!(m.approved_pr_count, derivation.covered_pr_ids.len());
    }

    /// Issue #334 is MERGED. When the reviewed-commit facts are present in the
    /// store, `derive_gaps` derives `review_unanchored_no_commit_sha` for an
    /// approving review of a merged PR that carries no `review_commit_sha`, and no
    /// longer emits the capability-unavailable diagnostic.
    #[test]
    fn issue_334_review_unanchored_gap_is_derived_when_facts_present() {
        use super::fixture::{pr, references_task, review};
        let anchored = |mut r: GraphRecord, sha: &str| -> GraphRecord {
            if let GraphRecord::Node {
                review_commit_sha, ..
            } = &mut r
            {
                *review_commit_sha = Some(sha.to_owned());
            }
            r
        };
        let head_of = |p: &GraphRecord| -> String {
            match p {
                GraphRecord::Node {
                    head_sha: Some(h), ..
                } => h.clone(),
                _ => panic!("no head_sha"),
            }
        };
        // prA: approved by an ANCHORED-at-head review (facts present, no gap).
        let pr_a = pr("project:v1:prA", "2026-03-15T12:00:00Z", "cA");
        let rv_a = anchored(
            review("project:v1:rvA", "2026-03-15T08:00:00Z", "approved"),
            &head_of(&pr_a),
        );
        // prB: approved by an UNANCHORED review (no review_commit_sha) → gap.
        let pr_b = pr("project:v1:prB", "2026-03-16T12:00:00Z", "cB");
        let rv_b = review("project:v1:rvB", "2026-03-16T08:00:00Z", "approved");
        let records = vec![
            pr_a,
            rv_a,
            references_task("project:v1:rvA", "project:v1:prA"),
            pr_b,
            rv_b,
            references_task("project:v1:rvB", "project:v1:prB"),
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
        // Facts present → capability diagnostic must NOT fire.
        assert!(
            !pack
                .diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent")),
            "with #334 facts present, the capability diagnostic must not fire: {:?}",
            pack.diagnostics
        );
        let unanchored: Vec<&GapRow> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "review_unanchored_no_commit_sha")
            .collect();
        assert_eq!(unanchored.len(), 1, "one unanchored gap: {:?}", pack.gaps);
        assert!(
            unanchored[0]
                .record_ids
                .contains(&"project:v1:prB".to_owned())
        );
        assert!(
            unanchored[0]
                .record_ids
                .contains(&"project:v1:rvB".to_owned())
        );
    }

    /// Issue #334 real derivation: an approving review anchored to a commit other
    /// than the PR's final head yields an `approval_precedes_final_head` gap.
    #[test]
    fn issue_334_approval_precedes_final_head_gap_is_derived() {
        use super::fixture::{pr, references_task, review};
        let anchored = |mut r: GraphRecord, sha: &str| -> GraphRecord {
            if let GraphRecord::Node {
                review_commit_sha, ..
            } = &mut r
            {
                *review_commit_sha = Some(sha.to_owned());
            }
            r
        };
        // prC: approved by a review anchored to a NON-head commit → stale.
        let pr_c = pr("project:v1:prC", "2026-03-17T12:00:00Z", "cC");
        let rv_c = anchored(
            review("project:v1:rvC", "2026-03-17T08:00:00Z", "approved"),
            "not-the-final-head",
        );
        let records = vec![
            pr_c,
            rv_c,
            references_task("project:v1:rvC", "project:v1:prC"),
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
                .diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"),
            "facts present → no capability diagnostic: {:?}",
            pack.diagnostics
        );
        let precedes: Vec<&GapRow> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "approval_precedes_final_head")
            .collect();
        assert_eq!(
            precedes.len(),
            1,
            "one precedes-final-head gap: {:?}",
            pack.gaps
        );
        assert!(
            precedes[0]
                .record_ids
                .contains(&"project:v1:prC".to_owned())
        );
        assert!(
            precedes[0]
                .record_ids
                .contains(&"project:v1:rvC".to_owned())
        );
    }

    /// Issue #334 / Codex P2 (gap-side sibling of the coverage fix 9c639f7): an
    /// approving review that IS anchored (`review_commit_sha` present) but whose
    /// covered PR carries NO `head_sha` (a pre-#333 or partial import) cannot be
    /// confirmed as reviewing the final head. The `(Some, None)` case must
    /// surface an `approval_precedes_final_head` gap flagged `head_sha_unavailable`
    /// — never fall silently through the catch-all — otherwise the #334 final-head
    /// check appears to have run cleanly while strict coverage degrades the same
    /// PR to `approval_stale_head` + `head_sha_unavailable`. The gap must NOT
    /// claim a specific head mismatch: the point is "final head unverifiable".
    #[test]
    fn issue_334_anchored_approval_missing_head_sha_yields_gap_not_silent_pass() {
        use super::fixture::{pr, references_task, review};
        let anchored = |mut r: GraphRecord, sha: &str| -> GraphRecord {
            if let GraphRecord::Node {
                review_commit_sha, ..
            } = &mut r
            {
                *review_commit_sha = Some(sha.to_owned());
            }
            r
        };
        let without_head = |mut r: GraphRecord| -> GraphRecord {
            if let GraphRecord::Node { head_sha, .. } = &mut r {
                *head_sha = None;
            }
            r
        };
        // prD: merged in-window, approved by an ANCHORED review, but the PR record
        // carries no head_sha (final head unverifiable).
        let pr_d = without_head(pr("project:v1:prD", "2026-03-17T12:00:00Z", "cD"));
        let rv_d = anchored(
            review("project:v1:rvD", "2026-03-17T08:00:00Z", "approved"),
            "any-anchor-sha",
        );
        let records = vec![
            pr_d,
            rv_d,
            references_task("project:v1:rvD", "project:v1:prD"),
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
        // #334 facts present → no capability diagnostic.
        assert!(
            !pack
                .diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"),
            "facts present → no capability diagnostic: {:?}",
            pack.diagnostics
        );
        let precedes: Vec<&GapRow> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "approval_precedes_final_head")
            .collect();
        assert_eq!(
            precedes.len(),
            1,
            "a missing head_sha must surface an approval_precedes_final_head gap, \
             not a silent catch-all pass: {:?}",
            pack.gaps
        );
        assert!(
            precedes[0]
                .record_ids
                .contains(&"project:v1:prD".to_owned())
        );
        assert!(
            precedes[0]
                .record_ids
                .contains(&"project:v1:rvD".to_owned())
        );
        assert!(
            precedes[0].detail.contains("head_sha_unavailable"),
            "the gap must flag the head as unavailable, never claim a specific \
             mismatch: {}",
            precedes[0].detail
        );

        // Consistency with the coverage side (9c639f7): strict coverage degrades
        // the SAME PR to approval_stale_head + head_sha_unavailable, and the gap
        // side now surfaces the corresponding defect — the two agree.
        let strict =
            derive_review_coverage(records.as_slice(), &win(), ReviewCoverageOptions::default());
        let row = strict
            .rows
            .iter()
            .find(|r| r.pr_task_id == "project:v1:prD")
            .expect("prD classified");
        assert_eq!(
            row.verdict,
            ReviewVerdict::ApprovalStaleHead,
            "strict coverage must degrade the unverifiable final head"
        );
        assert!(
            row.sub_labels.iter().any(|s| s == "head_sha_unavailable"),
            "coverage must report head_sha_unavailable, got {:?}",
            row.sub_labels
        );
    }

    /// Builds a #334-era `github_review_unanchored` project `Diagnostic` node the
    /// GitHub importer emits when an approving review carries no `commit_id` (see
    /// `src/github/records.rs::review_diagnostic`). Its `NodeKind::Diagnostic`
    /// summary carries the `[github_review_unanchored]` bracket-code prefix the
    /// importer stamps, matched exactly like `diagnostics_with_code` does.
    fn review_unanchored_diagnostic(review_id: &str) -> GraphRecord {
        GraphRecord::node(
            format!("project:v1:diag-{review_id}"),
            crate::ir::NodeKind::Diagnostic,
            None,
            None,
            None,
            format!(
                "[github_review_unanchored] review carries no commit_id; cannot anchor it to a \
                 commit; review='{review_id}'"
            ),
        )
    }

    /// Codex P2 (#334 capability probe): a genuine #334-era import whose approving
    /// reviews are ALL unanchored emits a `github_review_unanchored` diagnostic but
    /// no `review_commit_sha` and no `REVIEWS_COMMIT` edge. The capability probe
    /// must recognize that diagnostic as #334-era evidence, so `derive_gaps`
    /// emits the real `review_unanchored_no_commit_sha` gap instead of degrading
    /// to `capability_unavailable`. Before the fix the probe saw neither a
    /// `review_commit_sha` nor a `REVIEWS_COMMIT` edge and wrongly concluded
    /// "pre-#334", suppressing the gap — a pack that looked like satisfied
    /// coverage with the anchor check merely unavailable.
    #[test]
    fn issue_334_all_unanchored_reviews_with_diagnostic_yield_gap_not_capability_unavailable() {
        use super::fixture::{pr, references_task, review};
        // prB: merged in window, approved by an UNANCHORED review (no
        // review_commit_sha, no REVIEWS_COMMIT edge) — the sole #334 signal is the
        // importer's github_review_unanchored diagnostic.
        let pr_b = pr("project:v1:prB", "2026-03-16T12:00:00Z", "cB");
        let rv_b = review("project:v1:rvB", "2026-03-16T08:00:00Z", "approved");
        let records = vec![
            pr_b,
            rv_b,
            references_task("project:v1:rvB", "project:v1:prB"),
            review_unanchored_diagnostic("project:v1:rvB"),
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
        // The github_review_unanchored diagnostic is #334-era evidence: the
        // capability-unavailable degradation must NOT fire.
        assert!(
            !pack
                .diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent")),
            "a github_review_unanchored diagnostic proves #334-era extraction; the \
             capability diagnostic must not fire: {:?}",
            pack.diagnostics
        );
        let unanchored: Vec<&GapRow> = pack
            .gaps
            .iter()
            .filter(|g| g.gap_class == "review_unanchored_no_commit_sha")
            .collect();
        assert_eq!(
            unanchored.len(),
            1,
            "the all-unanchored #334 store must yield the real gap: {:?}",
            pack.gaps
        );
        assert!(
            unanchored[0]
                .record_ids
                .contains(&"project:v1:prB".to_owned())
        );
        assert!(
            unanchored[0]
                .record_ids
                .contains(&"project:v1:rvB".to_owned())
        );
    }

    /// The mirror of the case above: a genuinely PRE-#334 store carrying the SAME
    /// all-unanchored approving review but NO `github_review_unanchored`
    /// diagnostic (and no anchors, no `REVIEWS_COMMIT` edge) must STILL degrade to
    /// `capability_unavailable` with no fabricated #334 gap. This is the boundary
    /// the fix must not erase: absence of every #334 signal stays pre-#334.
    #[test]
    fn issue_334_pre_334_store_without_diagnostic_still_degrades() {
        use super::fixture::{pr, references_task, review};
        let pr_b = pr("project:v1:prB", "2026-03-16T12:00:00Z", "cB");
        let rv_b = review("project:v1:rvB", "2026-03-16T08:00:00Z", "approved");
        let records = vec![
            pr_b,
            rv_b,
            references_task("project:v1:rvB", "project:v1:prB"),
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
            pack.diagnostics
                .iter()
                .any(|d| d.code == "capability_unavailable"
                    && d.unavailable_reason.as_deref()
                        == Some("issue_334_reviewed_commit_facts_absent")),
            "a pre-#334 store with no #334 signal must degrade: {:?}",
            pack.diagnostics
        );
        assert!(
            !pack
                .gaps
                .iter()
                .any(|g| g.gap_class == "review_unanchored_no_commit_sha"),
            "no #334 gap can be fabricated for a pre-#334 store: {:?}",
            pack.gaps
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

#[cfg(test)]
mod pack340_tests {
    use super::fixture::{
        EXEMPLAR_SENTINEL, LOG_SIGNATURE_IDS, WINDOW_FROM, WINDOW_TO, build_log_incident_records,
        build_seed_records, error_signature,
    };
    use super::*;

    fn win() -> Window {
        Window {
            from: WINDOW_FROM.to_owned(),
            to: WINDOW_TO.to_owned(),
        }
    }

    fn assemble_cc73(records: &[GraphRecord]) -> EvidencePack {
        assemble_pack(
            records,
            &load_default_catalog(),
            "CC7.3",
            &win(),
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles")
    }

    fn section(pack: &EvidencePack, class: EvidenceClass) -> &EvidenceSection {
        pack.sections
            .iter()
            .find(|s| s.class == class.as_wire())
            .unwrap_or_else(|| panic!("section {} present", class.as_wire()))
    }

    // ── AC1: error_signatures ─────────────────────────────────────────────────
    #[test]
    fn error_signatures_populate_with_clipped_span_and_frame_joins() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        let sec = section(&pack, EvidenceClass::ErrorSignatures);
        assert_eq!(sec.status, "present");
        assert_eq!(sec.outcome, ClassOutcome::Pass);
        assert!(sec.unavailable_reason.is_none());
        // All three in-window signatures surface as hashed rows, plus sig1's one
        // co-located `FRAME_RESOLVES_TO` attribution edge (issue #371): 3 nodes + 1
        // edge = 4 hashed rows.
        assert_eq!(sec.record_count, 4);
        let sig_node_rows = sec
            .records
            .iter()
            .filter(|br| matches!(br.record, GraphRecord::Node { .. }))
            .count();
        let frame_edge_rows = sec
            .records
            .iter()
            .filter(|br| {
                matches!(&br.record,
                GraphRecord::Edge { label, .. } if label.as_str() == "FRAME_RESOLVES_TO")
            })
            .count();
        assert_eq!(sig_node_rows, 3, "3 in-window ErrorSignature nodes");
        assert_eq!(
            frame_edge_rows, 1,
            "sig1's single FRAME_RESOLVES_TO attribution edge co-located"
        );
        // Zero `unavailable` markers when records present (AC1).
        assert!(
            !pack
                .diagnostics
                .iter()
                .any(|d| d.evidence_class.as_deref() == Some("error_signatures")
                    && d.code == "evidence_class_unavailable"),
            "no unavailable marker when signatures present: {:?}",
            pack.diagnostics
        );
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = &sec.log_summary else {
            panic!("error_signatures summary present");
        };
        assert_eq!(signatures.len(), 3);
        let ids: Vec<&str> = signatures.iter().map(|s| s.signature_id.as_str()).collect();
        assert_eq!(ids, LOG_SIGNATURE_IDS, "rows ordered by record_id");

        let sig1 = &signatures[0];
        assert_eq!(sig1.signature_id, "log:v1:sig1");
        assert_eq!(sig1.severity, "error");
        // first_seen is in-window -> unchanged; last_seen (Apr 5) clipped to `to`.
        assert_eq!(sig1.first_seen_in_window, "2026-03-02T09:00:00Z");
        assert_eq!(
            sig1.last_seen_in_window, WINDOW_TO,
            "last_seen clipped to window"
        );
        // template_hash is a redaction-safe fingerprint of the excerpt.
        assert_eq!(
            sig1.template_hash,
            blake3::hash(b"connection refused to HOST").to_string()
        );
        assert!(
            sig1.frame_chain_hash.is_some(),
            "frames -> frame_chain_hash"
        );
        // Exemplar: handle + hash only, never text.
        assert_eq!(sig1.exemplars.len(), 1);
        let ex = &sig1.exemplars[0];
        assert!(ex.protected_handle.starts_with("protected:v1:"));
        assert_eq!(ex.content_hash, "ab".repeat(32));
        // FRAME_RESOLVES_TO join propagated verbatim.
        assert_eq!(sig1.frame_resolutions.len(), 1);
        assert_eq!(
            sig1.frame_resolutions[0].resolution.as_deref(),
            Some("resolved")
        );
        assert_eq!(sig1.frame_resolutions[0].frame_index, Some(0));
        assert_eq!(sig1.frame_resolutions[0].target_id, "codegraph:v5:sym-db");
        // sig2/sig3 carry no frames.
        assert!(signatures[1].frame_chain_hash.is_none());
        assert!(signatures[1].frame_resolutions.is_empty());
    }

    // Builds a `sig1 --FRAME_RESOLVES_TO--> codegraph:v5:sym-db` edge at
    // frame_index 0 with a caller-chosen resolution and an explicit stable id.
    // The real `log_resolve::resolve_frames` folds `resolution` into the edge id,
    // so two edges sharing (signature, frame_index, target) but differing in
    // resolution coexist as distinct records — this helper reproduces that (the
    // `fixture::frame_resolves_to` id omits resolution, so it would collapse the
    // pair under `coalesce_log_records`).
    fn frame_edge_with_id(id: &str, resolution: crate::ir::FrameResolution) -> GraphRecord {
        let mut e =
            super::fixture::frame_resolves_to("log:v1:sig1", "codegraph:v5:sym-db", resolution, 0);
        if let GraphRecord::Edge { id: eid, .. } = &mut e {
            *eid = id.to_owned();
        }
        e
    }

    // Issue #371, Codex round-7 P2: when a graph carries two FRAME_RESOLVES_TO
    // edges for the SAME signature+frame_index+target but DIFFERENT resolution,
    // the shared `frame_resolution_joins` derivation must be a TOTAL order so
    // assemble (which sees the input edge order) and verify (which sees the
    // canonically id-sorted section rows) agree, and the assembled pack is
    // byte-identical regardless of input edge order. Without `resolution` in the
    // sort key the two joins compare EQUAL, a stable sort preserves each caller's
    // differing input order, and (a) verify rejects the freshly assembled pack
    // and (b) reversing the input edges changes the pack bytes.
    #[test]
    fn frame_join_ordering_is_total_over_resolution_across_assemble_and_verify() {
        // Craft ids so the section's id-ascending canonical order is the REVERSE
        // of the input order below: "...path_only" < "...resolved" lexically.
        let id_resolved = "log:v1:edge-frame-sig1-symdb-resolved";
        let id_path_only = "log:v1:edge-frame-sig1-symdb-path_only";

        // Base incident, minus its built-in single FRAME_RESOLVES_TO edge so this
        // test wholly controls sig1's frame edges.
        let base: Vec<GraphRecord> = build_log_incident_records()
            .into_iter()
            .filter(|r| {
                !matches!(r,
                    GraphRecord::Edge { label, .. } if label.as_str() == "FRAME_RESOLVES_TO")
            })
            .collect();

        let e_resolved = frame_edge_with_id(id_resolved, crate::ir::FrameResolution::Resolved);
        let e_path_only = frame_edge_with_id(id_path_only, crate::ir::FrameResolution::PathOnly);

        // Forward input order: resolved (larger id) FIRST, path_only (smaller id)
        // SECOND — the reverse of the section's id-ascending order.
        let mut fwd = base.clone();
        fwd.push(e_resolved.clone());
        fwd.push(e_path_only.clone());

        // Reversed input order: same two edges, swapped.
        let mut rev = base;
        rev.push(e_path_only);
        rev.push(e_resolved);

        let pack_fwd = assemble_cc73(&fwd);
        let pack_rev = assemble_cc73(&rev);

        // (a) Round-trip: a freshly assembled pack must pass its own verify. Before
        // the fix, assemble records [resolved, path_only] (input order) while verify
        // recomputes [path_only, resolved] (section id order), so integrity fails.
        let report = verify_pack(&pack_fwd);
        assert!(
            report.ok,
            "freshly assembled pack must pass verify (round-trip): {report:?}"
        );

        // (b) Determinism: the assembled pack must be byte-identical regardless of
        // the caller's input edge order. Before the fix the tied joins keep input
        // order, so the two packs diverge.
        let bytes_fwd = serde_json::to_vec(&pack_fwd).expect("serialize fwd");
        let bytes_rev = serde_json::to_vec(&pack_rev).expect("serialize rev");
        assert_eq!(
            bytes_fwd, bytes_rev,
            "pack bytes must be independent of input FRAME_RESOLVES_TO edge order"
        );

        // Both packs must land on the total order (path_only < resolved).
        let sec = section(&pack_fwd, EvidenceClass::ErrorSignatures);
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = &sec.log_summary else {
            panic!("error_signatures summary present");
        };
        let sig1 = signatures
            .iter()
            .find(|s| s.signature_id == "log:v1:sig1")
            .expect("sig1 row present");
        let labels: Vec<Option<&str>> = sig1
            .frame_resolutions
            .iter()
            .map(|j| j.resolution.as_deref())
            .collect();
        assert_eq!(
            labels,
            vec![Some("path_only"), Some("resolved")],
            "frame joins sorted by (frame_index, target_id, resolution)"
        );
    }

    // ── AC2: occurrence_buckets ───────────────────────────────────────────────
    #[test]
    fn occurrence_buckets_sum_only_in_window_ordered() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        let sec = section(&pack, EvidenceClass::OccurrenceBuckets);
        assert_eq!(sec.status, "present");
        // 15 (sig1) + 16 (sig2) + 16 (sig3) = 47 in-window bucket NODES; the 2
        // out-of-window sig1 buckets never appear (0 leakage). Each bucket also
        // co-locates its `AGGREGATES` attribution edge (issue #340, Codex round-3),
        // so the section carries 47 nodes + 47 edges = 94 hashed rows.
        assert_eq!(sec.record_count, 94);
        let node_rows = sec
            .records
            .iter()
            .filter(|br| matches!(br.record, GraphRecord::Node { .. }))
            .count();
        let edge_rows = sec
            .records
            .iter()
            .filter(|br| {
                matches!(&br.record,
                GraphRecord::Edge { label, .. } if label.as_str() == "AGGREGATES")
            })
            .count();
        assert_eq!(node_rows, 47, "47 in-window bucket nodes");
        assert_eq!(edge_rows, 47, "one AGGREGATES attribution edge per bucket");
        let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) = &sec.log_summary
        else {
            panic!("occurrence_buckets summary present");
        };
        let by_sig: BTreeMap<&str, &SignatureOccurrenceTotal> = signature_totals
            .iter()
            .map(|t| (t.signature_id.as_str(), t))
            .collect();
        // sig1: sum(1..=15) = 120; the 999 + 888 out-of-window buckets excluded.
        let s1 = by_sig["log:v1:sig1"];
        assert_eq!(s1.in_window_occurrences, 120);
        assert_eq!(s1.buckets.len(), 15);
        // Buckets ordered by hour.
        let hours: Vec<&str> = s1.buckets.iter().map(|b| b.hour.as_str()).collect();
        let mut sorted = hours.clone();
        sorted.sort_unstable();
        assert_eq!(hours, sorted, "buckets ordered by (signature, hour)");
        // sig2: 16 * 2 = 32; sig3: 16 * 5 = 80.
        assert_eq!(by_sig["log:v1:sig2"].in_window_occurrences, 32);
        assert_eq!(by_sig["log:v1:sig3"].in_window_occurrences, 80);
    }

    #[test]
    fn occurrence_bucket_partial_overlap_included_whole() {
        // A NON-hour-aligned window so a bucket's hour straddles `from`.
        // Window from 2026-03-02T05:30 to 2026-03-02T07:30.
        let window = Window {
            from: "2026-03-02T05:30:00Z".to_owned(),
            to: "2026-03-02T07:30:00Z".to_owned(),
        };
        let mut records = vec![error_signature(
            "log:v1:sigp",
            "error",
            "partial overlap probe",
            "2026-03-02T05:00:00Z",
            "2026-03-02T08:00:00Z",
            10,
            Some(Vec::new()),
        )];
        // bucket at 04:00 -> hour [04:00,05:00): hour_end 05:00 <= from 05:30 -> EXCLUDED.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:bp-04",
            "2026-03-02T04:00:00Z",
            100,
        ));
        records.push(super::fixture::aggregates("log:v1:bp-04", "log:v1:sigp"));
        // bucket at 05:00 -> hour [05:00,06:00): straddles from (05:00 < 05:30 < 06:00) -> INCLUDED WHOLE.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:bp-05",
            "2026-03-02T05:00:00Z",
            7,
        ));
        records.push(super::fixture::aggregates("log:v1:bp-05", "log:v1:sigp"));
        // bucket at 06:00 -> fully inside -> INCLUDED.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:bp-06",
            "2026-03-02T06:00:00Z",
            3,
        ));
        records.push(super::fixture::aggregates("log:v1:bp-06", "log:v1:sigp"));
        // bucket at 07:00 -> hour [07:00,08:00): straddles to (07:00 < 07:30 < 08:00) -> INCLUDED WHOLE.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:bp-07",
            "2026-03-02T07:00:00Z",
            5,
        ));
        records.push(super::fixture::aggregates("log:v1:bp-07", "log:v1:sigp"));
        // bucket at 08:00 -> starts at/after to -> EXCLUDED.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:bp-08",
            "2026-03-02T08:00:00Z",
            200,
        ));
        records.push(super::fixture::aggregates("log:v1:bp-08", "log:v1:sigp"));

        let pack = assemble_pack(
            &records,
            &load_default_catalog(),
            "CC7.2",
            &window,
            1.0,
            "test-0.0.0",
            None,
        )
        .expect("assembles");
        let sec = section(&pack, EvidenceClass::OccurrenceBuckets);
        // 05:00 (whole), 06:00, 07:00 (whole) included; 04:00 and 08:00 excluded.
        // Each of the 3 included buckets co-locates its AGGREGATES attribution edge
        // (issue #340, Codex round-3): 3 nodes + 3 edges = 6 hashed rows.
        assert_eq!(sec.record_count, 6);
        let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) = &sec.log_summary
        else {
            panic!("summary present");
        };
        assert_eq!(signature_totals[0].in_window_occurrences, 7 + 3 + 5);
        // The partial-overlap bucket survives verify Window-consistency.
        let report = verify_pack(&pack);
        assert!(
            report.window_consistency.passed,
            "{:?}",
            report.window_consistency
        );
        assert!(report.ok, "verify passes for partial-overlap buckets");
    }

    // ── AC3: remediation_links ────────────────────────────────────────────────
    #[test]
    fn remediation_links_derive_planted_chain() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        let sec = section(&pack, EvidenceClass::RemediationLinks);
        assert_eq!(sec.status, "present", "capability probe -> present");
        assert_eq!(
            sec.record_count, 0,
            "no hashed rows; leads ride log_summary"
        );
        let Some(LogEvidenceSummary::RemediationLinks { links }) = &sec.log_summary else {
            panic!("remediation summary present");
        };
        assert_eq!(links.len(), 1, "exactly the planted chain");
        let link = &links[0];
        assert_eq!(link.signature_id, "log:v1:sig1");
        assert_eq!(link.symbol_id, "codegraph:v5:sym-db");
        assert_eq!(link.commit_id, "codegraph:v5:fixc");
        assert_eq!(link.commit_valid_time, "2026-03-10T09:00:00Z");
        assert_eq!(link.frame_resolution.as_deref(), Some("resolved"));
        assert_eq!(
            link.verification_ids,
            vec!["verification:v1:fixver".to_owned()]
        );
        assert_eq!(link.disclaimer, REMEDIATION_SECTION_DISCLAIMER);
    }

    #[test]
    fn remediation_excludes_commit_before_signature_activity() {
        // A commit that pre-dates the signature's first_seen is not a remediation.
        let mut records = build_log_incident_records();
        // Repoint the fix commit's valid time to BEFORE sig1 first_seen (Mar 2 09).
        records.push(super::fixture::commit_node(
            "codegraph:v5:oldc",
            "2026-03-01T00:30:00Z",
        ));
        records.push(super::fixture::changed_in(
            "codegraph:v5:sym-db",
            "codegraph:v5:oldc",
            "2026-03-01T00:30:00Z",
        ));
        let pack = assemble_cc73(&records);
        let Some(LogEvidenceSummary::RemediationLinks { links }) =
            &section(&pack, EvidenceClass::RemediationLinks).log_summary
        else {
            panic!("summary present");
        };
        assert!(
            links.iter().all(|l| l.commit_id != "codegraph:v5:oldc"),
            "pre-activity commit is not a remediation lead"
        );
        assert_eq!(links.len(), 1);
    }

    // ── AC4: trust separation ─────────────────────────────────────────────────
    #[test]
    fn log_rows_reported_runtime_observation_never_source_or_verification() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        // All error_signatures + occurrence_buckets rows tallied runtime_observation.
        let counts = &pack.manifest.included_record_counts;
        assert_eq!(counts.get("runtime_observation").copied(), Some(3 + 47));
        assert!(
            !counts.contains_key("source_fact"),
            "log rows never tallied source_fact"
        );
        assert!(
            !counts.contains_key("verification_evidence"),
            "log rows never tallied verification_evidence"
        );
        // Non-code 100%-handle rule: every runtime_observation row is cited.
        let tally = pack
            .verdicts
            .citation_tallies
            .iter()
            .find(|t| t.trust_class == "runtime_observation")
            .expect("runtime_observation tally");
        assert_eq!(tally.total, 50);
        assert_eq!(tally.cited, 50);
        assert_eq!(tally.missing, 0);
    }

    // ── AC5: degradation, both directions ─────────────────────────────────────
    #[test]
    fn degrades_to_log_domain_absent_when_no_log_records() {
        // A no-log store (the #338 seed): optional classes -> unavailable, exit 0.
        let records = build_seed_records();
        let pack = assemble_cc73(&records);
        for class in [
            EvidenceClass::ErrorSignatures,
            EvidenceClass::OccurrenceBuckets,
            EvidenceClass::RemediationLinks,
        ] {
            let sec = section(&pack, class);
            assert_eq!(sec.status, "unavailable", "{}", class.as_wire());
            assert_eq!(sec.unavailable_reason.as_deref(), Some("log_domain_absent"));
            assert_eq!(sec.outcome, ClassOutcome::ReportedOptionalUnavailable);
            assert!(sec.log_summary.is_none());
        }
        assert!(pack.verdicts.required_classes.passed, "optional -> ok");
        assert!(pack.verdicts.ok, "no-log optional degradation is a pass");
    }

    #[test]
    fn required_flip_gate_fails_when_log_domain_absent() {
        // A catalog variant marking the log classes REQUIRED (NOT the shipped
        // soc2-v1.json) gate-fails over a no-log store.
        let mut catalog = load_default_catalog();
        let cc73 = catalog
            .controls
            .iter_mut()
            .find(|c| c.control_id == "CC7.3")
            .expect("CC7.3");
        for cr in &mut cc73.evidence_classes {
            if matches!(
                cr.class,
                EvidenceClass::ErrorSignatures
                    | EvidenceClass::OccurrenceBuckets
                    | EvidenceClass::RemediationLinks
            ) {
                cr.requirement = Requirement::Required;
            }
        }
        let records = build_seed_records();
        let pack = assemble_pack(&records, &catalog, "CC7.3", &win(), 1.0, "test-0.0.0", None)
            .expect("assembles");
        assert!(!pack.verdicts.ok, "required + absent -> gate fail");
        assert!(!pack.verdicts.required_classes.passed);
        for class in [
            "error_signatures",
            "occurrence_buckets",
            "remediation_links",
        ] {
            assert!(
                pack.diagnostics
                    .iter()
                    .any(|d| d.code == "required_class_unavailable"
                        && d.evidence_class.as_deref() == Some(class)),
                "required_class_unavailable for {class}"
            );
        }
    }

    // ── AC6: determinism ──────────────────────────────────────────────────────
    #[test]
    fn packs_byte_identical_across_five_runs() {
        let records = build_log_incident_records();
        let baseline = serde_json::to_string(&assemble_cc73(&records)).expect("serializes");
        for _ in 0..5 {
            let again = serde_json::to_string(&assemble_cc73(&records)).expect("serializes");
            assert_eq!(again, baseline, "byte-identical across runs");
        }
    }

    // ── AC7: verify covers new sections + safety (zero raw log bytes) ──────────
    #[test]
    fn verify_passes_and_no_raw_log_or_exemplar_text_in_pack() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        let report = verify_pack(&pack);
        assert!(report.integrity.passed, "{:?}", report.integrity);
        assert!(report.safety.passed, "{:?}", report.safety);
        assert!(
            report.window_consistency.passed,
            "{:?}",
            report.window_consistency
        );
        assert!(report.coverage.passed, "{:?}", report.coverage);
        assert!(report.ok, "verify clean over log sections");
        // Scanner assertion: exemplar raw text never enters the pack.
        let serialized = serde_json::to_string(&pack).expect("serializes");
        assert!(
            !serialized.contains(EXEMPLAR_SENTINEL),
            "exemplar raw text must never appear in the assembled pack"
        );
        assert!(
            !serialized.contains("hunter2"),
            "raw exemplar payload value must never appear"
        );
        // Codex round-4 P2: no normalized template text or backtrace-frame path
        // text may ride the hashed section records either. The fixture's template
        // excerpts and frame-only path/module text must be absent from the whole
        // serialized pack. ("src/db.rs" is deliberately excluded here: a legitimate
        // Symbol node in the remediation chain carries that code path, unrelated to
        // the frame leak; the frame-only strings below are exclusive to frames.)
        for raw in [
            "connection refused to HOST",
            "deprecated config key KEY",
            "panic at NUMBER",
            "app::db",
            "app::main",
            "src/main.rs",
        ] {
            assert!(
                !serialized.contains(raw),
                "raw log template/frame text `{raw}` must never appear in the pack"
            );
        }
    }

    /// Issue #371 (Codex P2 Safety/redaction): a co-located `FRAME_RESOLVES_TO`
    /// edge's REQUIRED free-text `summary` must never carry raw log/backtrace text
    /// into a hashed `error_signatures` row. `resolve-frames` synthesizes a safe
    /// summary, but an older/hand-authored importer can populate that field with
    /// anything; `bundle::scrub_record` only SECRET-redacts an edge summary and
    /// `scrub_log_node_text` is a Node-only no-op for edges, so non-secret raw
    /// text would otherwise ride the hashed row while Safety (secret + Node
    /// scrubbed-field checks) still passes. The #371 binding recomputes from
    /// `frame_resolution` + `frame_index` + endpoint IDs only, so clearing the
    /// summary preserves the assemble->verify round-trip.
    #[test]
    fn co_located_frame_edge_summary_raw_text_never_rides_the_pack() {
        const RAW_FRAME_SENTINEL: &str = "RAW_FRAME_SENTINEL_backtrace_line";
        let mut records = build_log_incident_records();
        // Plant raw backtrace text in the sig1 FRAME_RESOLVES_TO edge summary the
        // way an older/hand-authored importer could (the field is a required
        // String that deserializes verbatim from graph JSONL).
        let mut planted = false;
        for r in &mut records {
            if let GraphRecord::Edge { label, summary, .. } = r
                && label.as_str() == "FRAME_RESOLVES_TO"
            {
                *summary = format!("{RAW_FRAME_SENTINEL} at src/db.rs:42 in app::db::connect");
                planted = true;
            }
        }
        assert!(
            planted,
            "fixture carries a FRAME_RESOLVES_TO edge to plant on"
        );

        let pack = assemble_cc73(&records);

        // (a) The sentinel must be absent from the whole assembled artifact.
        let serialized = serde_json::to_string(&pack).expect("serializes");
        assert!(
            !serialized.contains(RAW_FRAME_SENTINEL),
            "raw frame-edge summary text must never appear in the assembled pack"
        );

        // (b) Safety passes AND the round-trip verifies clean.
        let report = verify_pack(&pack);
        assert!(report.safety.passed, "{:?}", report.safety);
        assert!(report.integrity.passed, "{:?}", report.integrity);
        assert!(
            report.ok,
            "round-trip verify clean after frame-summary scrub"
        );

        // The #371 binding is preserved: sig1 still carries its resolved frame join.
        let sec = section(&pack, EvidenceClass::ErrorSignatures);
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = &sec.log_summary else {
            panic!("error_signatures summary present");
        };
        let sig1 = signatures
            .iter()
            .find(|s| s.signature_id == "log:v1:sig1")
            .expect("sig1 present");
        assert_eq!(sig1.frame_resolutions.len(), 1);
        assert_eq!(sig1.frame_resolutions[0].target_id, "codegraph:v5:sym-db");
    }

    /// Issue #371 (Codex P2 verify-side follow-on): the assemble-side scrub clears
    /// a co-located `FRAME_RESOLVES_TO` edge's free-text `summary`, but
    /// `verify_pack` reads `section.records` AS-IS and never re-runs
    /// `scrub_log_node_text`. The frame binding derives only
    /// `frame_index`/`resolution`/`target_id` and ignores `summary`, and pack
    /// Safety only runs `detect_secret` + a Node-only `first_unscrubbed_field`, so
    /// a NON-secret raw backtrace injected into such an edge summary — with the
    /// row hash recomputed so Integrity passes — would verify clean without an
    /// explicit verify-side check. Safety must reject a nonempty co-located
    /// frame-edge summary.
    #[test]
    fn verify_rejects_raw_text_in_co_located_frame_edge_summary() {
        const RAW_FRAME_SENTINEL: &str = "RAW_FRAME_SENTINEL_backtrace_line";
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);

        // A clean assembled pack (summaries already cleared) verifies ok.
        assert!(
            verify_pack(&pack).ok,
            "clean assembled pack verifies before tampering"
        );

        // Inject raw backtrace text into a co-located FRAME_RESOLVES_TO edge row's
        // summary and RECOMPUTE that row's content hash so Integrity still passes
        // — only the new Safety check can catch it.
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
            .expect("error_signatures section");
        let mut injected = false;
        for br in &mut sec.records {
            if let GraphRecord::Edge { label, summary, .. } = &mut br.record
                && label.as_str() == "FRAME_RESOLVES_TO"
            {
                *summary = format!("{RAW_FRAME_SENTINEL} at src/db.rs:42 in app::db::connect");
                br.hash =
                    blake3::hash(serde_json::to_string(&br.record).unwrap().as_bytes()).to_string();
                injected = true;
                break;
            }
        }
        assert!(
            injected,
            "clean pack co-locates a FRAME_RESOLVES_TO edge row to inject on"
        );

        let report = verify_pack(&pack);
        // Integrity still passes — the row hash was recomputed to match, so only
        // the new Safety check stands between the tamper and a clean verdict.
        assert!(
            report.integrity.passed,
            "row hash recomputed so Integrity passes: {}",
            report.integrity.detail
        );
        // Safety must reject the nonempty co-located frame-edge summary.
        assert!(
            !report.safety.passed,
            "a nonempty co-located FRAME_RESOLVES_TO summary must fail Safety"
        );
        assert!(
            report.safety.detail.contains("summary"),
            "Safety detail names the frame-edge summary: {}",
            report.safety.detail
        );
        assert!(!report.ok, "verify rejects the tampered pack");
    }

    /// Codex round-4 P2: the `error_signatures` section's hashed records must NOT
    /// carry the `ErrorSignature` node's raw `template_excerpt` or backtrace-frame
    /// path text. The summary already carries the redaction-safe
    /// `template_hash` / `frame_chain_hash` fingerprints, so nothing functional
    /// depends on the excerpt/frame text, and stripping it keeps verify clean.
    #[test]
    fn error_signatures_section_records_carry_no_raw_template_or_frame_text() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        let sec = section(&pack, EvidenceClass::ErrorSignatures);
        let sec_json = serde_json::to_string(&sec.records).expect("serializes section records");
        for raw in [
            "connection refused to HOST",
            "deprecated config key KEY",
            "panic at NUMBER",
            "app::db",
            "app::main",
            "src/db.rs",
            "src/main.rs",
        ] {
            assert!(
                !sec_json.contains(raw),
                "error_signatures section record leaks raw log text `{raw}`"
            );
        }
        // The scrubbed node's `template_excerpt` now holds the fingerprint that the
        // summary carries as `template_hash`, and the section still verifies clean.
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = &sec.log_summary else {
            panic!("error_signatures summary present");
        };
        for br in &sec.records {
            if let GraphRecord::Node { log: Some(p), .. } = &br.record
                && let crate::ir::LogPayload::ErrorSignature(payload) = p.as_ref()
            {
                let row = signatures
                    .iter()
                    .find(|r| r.signature_id == br.record.id())
                    .expect("every section signature has a summary row");
                assert_eq!(
                    payload.template_excerpt, row.template_hash,
                    "scrubbed node carries the template fingerprint, not raw text"
                );
                // Frame path text, if any, is a BLAKE3 fingerprint (64 hex chars),
                // never a readable path.
                if let Some(frames) = &payload.frames {
                    for f in frames {
                        for text in [f.module_path.as_deref(), f.file_path.as_deref()]
                            .into_iter()
                            .flatten()
                        {
                            assert_eq!(text.len(), 64, "frame path scrubbed to a hash");
                            assert!(
                                text.chars().all(|c| c.is_ascii_hexdigit()),
                                "frame path fingerprint is hex"
                            );
                        }
                    }
                }
            }
        }
        assert!(verify_pack(&pack).ok, "scrubbed pack still verifies clean");
    }

    #[test]
    fn verify_rejects_smuggled_remediation_row() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        // Smuggle a hashed row into the remediation_links section.
        let commit = records
            .iter()
            .find(|r| r.id() == "codegraph:v5:fixc")
            .expect("commit")
            .clone();
        let scrubbed = crate::bundle::scrub_record(commit);
        let json = serde_json::to_string(&scrubbed).unwrap();
        let hash = blake3::hash(json.as_bytes()).to_string();
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::RemediationLinks.as_wire())
            .expect("remediation section");
        sec.records.push(BundleRecord {
            record: scrubbed,
            hash,
        });
        sec.record_count = sec.records.len();
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a smuggled hashed remediation row must fail Integrity"
        );
    }

    // ── AC7: verify BINDS the derived log summaries (tamper-evidence, issue #340)
    // The `log_summary` values are the derived evidence consumers read, but they
    // ride OUTSIDE the hashed `records`. Without an Integrity bind an attacker can
    // edit an occurrence total, a template fingerprint, or a remediation commit id
    // in a serialized pack and every other check (record hash, manifest counts,
    // window, safety) still passes. These tests tamper a summary value WITHOUT
    // updating its binding hash and assert `verify_pack` now reports an Integrity
    // defect — exactly as `review_coverage.measurement` is bound.

    #[test]
    fn assembled_log_sections_carry_a_binding_hash() {
        let records = build_log_incident_records();
        let pack = assemble_cc73(&records);
        for class in [
            EvidenceClass::ErrorSignatures,
            EvidenceClass::OccurrenceBuckets,
            EvidenceClass::RemediationLinks,
        ] {
            let sec = section(&pack, class);
            assert!(sec.log_summary.is_some(), "{class:?} carries a summary");
            let stored = sec
                .log_summary_hash
                .as_deref()
                .unwrap_or_else(|| panic!("{class:?} carries a binding log_summary_hash"));
            assert_eq!(
                stored,
                hash_log_summary(sec.log_summary.as_ref().unwrap()),
                "{class:?} log_summary_hash binds its summary"
            );
        }
        // Baseline: the untampered pack verifies clean over the new bind.
        assert!(verify_pack(&pack).ok, "untampered pack verifies clean");
    }

    #[test]
    fn verify_rejects_tampered_in_window_occurrences() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
            .expect("occurrence_buckets section");
        let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) =
            sec.log_summary.as_mut()
        else {
            panic!("occurrence_buckets summary present");
        };
        // Inflate a signature's occurrence total (leave the binding hash stale).
        signature_totals[0].in_window_occurrences += 1_000;
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a tampered in_window_occurrences must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_tampered_template_hash() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
            .expect("error_signatures section");
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = sec.log_summary.as_mut()
        else {
            panic!("error_signatures summary present");
        };
        // Swap a template fingerprint (leave the binding hash stale).
        signatures[0].template_hash = blake3::hash(b"forged template").to_string();
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a tampered template_hash must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_tampered_frame_chain_hash() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
            .expect("error_signatures section");
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = sec.log_summary.as_mut()
        else {
            panic!("error_signatures summary present");
        };
        // sig1 carries frames -> a frame_chain_hash; forge it.
        let sig1 = signatures
            .iter_mut()
            .find(|s| s.signature_id == "log:v1:sig1")
            .expect("sig1 carries frames");
        sig1.frame_chain_hash = Some(blake3::hash(b"forged frames").to_string());
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a tampered frame_chain_hash must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_forged_remediation_commit_id() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::RemediationLinks.as_wire())
            .expect("remediation_links section");
        let Some(LogEvidenceSummary::RemediationLinks { links }) = sec.log_summary.as_mut() else {
            panic!("remediation_links summary present");
        };
        // Forge the remediation commit id — the highest-risk tamper: the link has
        // NO backing hashed row, so only the whole-summary bind can catch it.
        links[0].commit_id = "codegraph:v5:attacker-commit".to_owned();
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a forged remediation commit_id must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_stripped_log_summary_hash() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::RemediationLinks.as_wire())
            .expect("remediation_links section");
        // Stripping the binding hash while keeping the summary is itself tamper.
        sec.log_summary_hash = None;
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a stripped log_summary_hash must fail Integrity: {}",
            report.integrity.detail
        );
    }

    // ── Codex round-2 P1: reverse bucket-coverage bijection ───────────────────
    // `bind_occurrence_totals` must reject a hashed `LogOccurrenceBucket` node that
    // is dropped from the summary. Recomputing `log_summary_hash` over the reduced
    // summary defeats the whole-summary (layer-1) bind, so only the reverse
    // coverage bijection can catch the under-report.

    #[test]
    fn verify_rejects_bucket_dropped_from_summary() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
            .expect("occurrence_buckets section");
        {
            let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) =
                sec.log_summary.as_mut()
            else {
                panic!("occurrence_buckets summary present");
            };
            let total = signature_totals
                .iter_mut()
                .find(|t| t.signature_id == "log:v1:sig1")
                .expect("sig1 total");
            // Drop one in-window bucket and reduce the reported total to match, so
            // the sum check passes and the reverse coverage guard is the ONLY
            // failing check. Its hashed row lingers in `section.records`.
            let dropped = total.buckets.remove(0);
            total.in_window_occurrences -= dropped.occurrence_count;
        }
        // Recompute the binding hash over the reduced summary (defeats layer 1).
        sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a hashed bucket dropped from the summary must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_whole_signature_total_dropped_from_summary() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
            .expect("occurrence_buckets section");
        {
            let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) =
                sec.log_summary.as_mut()
            else {
                panic!("occurrence_buckets summary present");
            };
            // Drop an entire signature's total; every one of its hashed buckets
            // lingers in `section.records` but is now uncovered by the summary.
            let idx = signature_totals
                .iter()
                .position(|t| t.signature_id == "log:v1:sig1")
                .expect("sig1 total");
            signature_totals.remove(idx);
        }
        sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a whole signature total dropped from the summary must fail Integrity: {}",
            report.integrity.detail
        );
    }

    // ── Codex round-2 P2: variant <-> section.class bind ──────────────────────
    // Each `log_summary` variant must sit on its matching section class. The
    // genuinely exploitable hole is the `RemediationLinks` arm, which returned
    // `Ok(())` unconditionally: a `RemediationLinks` summary (with its hash
    // recomputed) rides ANY section — a mismatched log section or a non-log
    // section — with no backing hashed row to catch it, so ONLY the variant<->class
    // bind can reject it (the node-backed `ErrorSignatures`/`OccurrenceBuckets`
    // variants are additionally netted by the section-membership and backing-row
    // checks above).

    #[test]
    fn verify_rejects_remediation_summary_on_mismatched_log_section() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let remediation = pack
            .sections
            .iter()
            .find(|s| s.class == EvidenceClass::RemediationLinks.as_wire())
            .and_then(|s| s.log_summary.clone())
            .expect("remediation_links summary");
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
            .expect("error_signatures section");
        // A RemediationLinks summary on the error_signatures log section: recompute
        // its hash so layer 1 passes and only the variant<->class bind can reject it
        // (the arm formerly returned Ok unconditionally).
        sec.log_summary = Some(remediation);
        sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a RemediationLinks summary on the error_signatures section must fail \
             Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_occurrence_summary_on_wrong_section() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let buckets = pack
            .sections
            .iter()
            .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
            .and_then(|s| s.log_summary.clone())
            .expect("occurrence_buckets summary");
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::RemediationLinks.as_wire())
            .expect("remediation_links section");
        // An OccurrenceBuckets summary on the remediation_links section: the
        // variant<->class bind rejects it (and the backing-row net would too — this
        // node-backed variant cannot slip past on the wrong section).
        sec.log_summary = Some(buckets);
        sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "an OccurrenceBuckets summary on the remediation_links section must fail \
             Integrity: {}",
            report.integrity.detail
        );
    }

    // ── CC7.2 also carries the two available log classes ──────────────────────
    #[test]
    fn cc72_carries_error_signatures_and_buckets() {
        let records = build_log_incident_records();
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
        assert_eq!(
            section(&pack, EvidenceClass::ErrorSignatures).status,
            "present"
        );
        assert_eq!(
            section(&pack, EvidenceClass::OccurrenceBuckets).status,
            "present"
        );
        // CC7.2 maps no remediation_links class.
        assert!(
            !pack.sections.iter().any(|s| s.class == "remediation_links"),
            "CC7.2 does not map remediation_links"
        );
        assert!(verify_pack(&pack).ok);
    }

    // ── Codex round-3 P1/P2: bucket <-> signature binding + assemble<->verify ──
    // The occurrence-bucket summary attributes each bucket to a signature via
    // `total.signature_id`, but the bucket NODE payload carries no signature field
    // (the signature is only an identity input of the bucket's stable ID hash). The
    // fix co-locates the `LogOccurrenceBucket --AGGREGATES--> ErrorSignature` edges
    // as hash-bound rows so verify can re-derive the binding offline, and excludes
    // an in-window bucket that lacks an attribution edge from BOTH the section and
    // the summary under a counted `unattributed_bucket` diagnostic.

    /// A minimal fixture: one signature with one ATTRIBUTED in-window bucket and one
    /// UNATTRIBUTED in-window bucket (no `AGGREGATES` edge).
    fn records_with_unattributed_bucket() -> Vec<GraphRecord> {
        let mut records = vec![error_signature(
            "log:v1:siga",
            "error",
            "boom on start",
            "2026-03-02T00:00:00Z",
            "2026-03-02T05:00:00Z",
            10,
            Some(Vec::new()),
        )];
        // An attributed in-window bucket.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:ba-00",
            "2026-03-02T00:00:00Z",
            3,
        ));
        records.push(super::fixture::aggregates("log:v1:ba-00", "log:v1:siga"));
        // An UNATTRIBUTED in-window bucket: no AGGREGATES edge names its signature.
        records.push(super::fixture::occurrence_bucket(
            "log:v1:ba-unattr",
            "2026-03-02T01:00:00Z",
            99,
        ));
        records
    }

    #[test]
    fn assemble_with_unattributed_bucket_excludes_it_and_passes_verify() {
        let records = records_with_unattributed_bucket();
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
        // REGRESSION GUARD (P2): every pack `assemble_pack` produces MUST pass its
        // own offline `verify_pack`. Before the fix the unattributed bucket rode
        // section.records but was skipped from the summary, so the reverse-coverage
        // guard rejected the freshly-assembled pack.
        let report = verify_pack(&pack);
        assert!(
            report.ok,
            "assemble<->verify consistency: {:?} / {:?}",
            report.integrity, report.window_consistency
        );
        // The unattributed bucket is excluded from the section records.
        let sec = section(&pack, EvidenceClass::OccurrenceBuckets);
        assert!(
            !sec.records
                .iter()
                .any(|br| br.record.id() == "log:v1:ba-unattr"),
            "unattributed bucket excluded from section records"
        );
        // ... and surfaced under a counted `unattributed_bucket` diagnostic.
        assert!(
            pack.diagnostics
                .iter()
                .any(|d| d.code == "unattributed_bucket"
                    && d.evidence_class.as_deref() == Some("occurrence_buckets")
                    && d.record_ids.iter().any(|id| id == "log:v1:ba-unattr")),
            "unattributed bucket tallied under a diagnostic: {:?}",
            pack.diagnostics
        );
        // ... and excluded from the summary totals.
        let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) = &sec.log_summary
        else {
            panic!("occurrence_buckets summary present");
        };
        assert!(
            signature_totals
                .iter()
                .all(|t| t.buckets.iter().all(|b| b.bucket_id != "log:v1:ba-unattr")),
            "unattributed bucket excluded from the summary"
        );
        // The attributed bucket is retained and its AGGREGATES edge co-located.
        assert!(
            sec.records
                .iter()
                .any(|br| br.record.id() == "log:v1:ba-00"),
            "attributed bucket retained"
        );
        assert!(
            sec.records.iter().any(|br| matches!(&br.record,
                GraphRecord::Edge { label, source, target, .. }
                    if label.as_str() == "AGGREGATES"
                        && source == "log:v1:ba-00"
                        && target == "log:v1:siga")),
            "attribution edge co-located as a hashed row"
        );
    }

    // ── Codex round-6 P2: concatenated multi-scan coalescing ─────────────────
    // A `LogSource` is a NON-identity input, so the SAME repo's `scan-logs` output
    // concatenated (a documented, legitimate multi-scan workflow) carries the same
    // stable `ErrorSignature` / `LogOccurrenceBucket` ID once per scan. Before the
    // fix, assemble emitted one summary row per PHYSICAL record and its own offline
    // `verify_pack` (one row per signature) rejected the freshly assembled pack.
    // The fix coalesces duplicate log records by stable ID at assemble time.

    /// The `build_log_incident_records()` graph concatenated with itself: every
    /// stable log ID appears TWICE, exactly as `cat scanA.jsonl scanA.jsonl` of one
    /// repo's IDENTICAL scan would produce. Signature aggregate `occurrence_count`
    /// still doubles (signature identity omits `LogSource`), but bucket totals DEDUP
    /// (issue #361: a shared bucket ID is a byte-identical rescan → collapsed).
    /// Merged first/last-seen are unchanged (identical copies).
    fn concatenated_multi_scan_records() -> Vec<GraphRecord> {
        let mut records = build_log_incident_records();
        let dup = build_log_incident_records();
        records.extend(dup);
        records
    }

    /// Two DISTINCT scans of one repo producing the SAME stable `ErrorSignature`
    /// ID (`log:v1:sigX`) with DIFFERENT `LogSource`, DIFFERENT first/last-seen, and
    /// two same-hour buckets that (since issue #361) carry DISTINCT source-aware
    /// bucket IDs (`log:v2:bx-00-a` / `log:v2:bx-00-b`), so both survive and their
    /// per-source counts sum. Proves the merge keeps the earliest `first_seen`, the
    /// latest `last_seen`, and the SUMMED occurrence counts across distinct sources.
    fn two_scan_differing_extents_records() -> Vec<GraphRecord> {
        vec![
            // Scan A (source A): narrow span, count 10; bucket count 4.
            error_signature(
                "log:v1:sigX",
                "error",
                "flaky upstream TIMEOUT",
                "2026-03-03T09:00:00Z",
                "2026-03-03T12:00:00Z",
                10,
                None,
            ),
            super::fixture::occurrence_bucket("log:v2:bx-00-a", "2026-03-03T00:00:00Z", 4),
            super::fixture::aggregates("log:v2:bx-00-a", "log:v1:sigX"),
            // Scan B (source B): EARLIER first_seen, LATER last_seen, count 25; a
            // DISTINCT source-aware bucket ID for the same hour, count 6.
            error_signature(
                "log:v1:sigX",
                "error",
                "flaky upstream TIMEOUT",
                "2026-03-02T08:00:00Z",
                "2026-03-04T15:00:00Z",
                25,
                None,
            ),
            super::fixture::occurrence_bucket("log:v2:bx-00-b", "2026-03-03T00:00:00Z", 6),
            super::fixture::aggregates("log:v2:bx-00-b", "log:v1:sigX"),
        ]
    }

    #[test]
    fn assemble_coalesces_concatenated_multi_scan_and_passes_verify() {
        // REGRESSION GUARD (Codex round-6 P2): a concatenated multi-scan graph must
        // assemble into a pack that passes its OWN offline verify. Before the fix the
        // duplicate physical records produced duplicate summary rows that
        // `verify_pack` rejected.
        let records = concatenated_multi_scan_records();
        let pack = assemble_cc73(&records);
        let report = verify_pack(&pack);
        assert!(
            report.ok,
            "concatenated multi-scan pack must pass its own verify: {:?} / {:?}",
            report.integrity, report.window_consistency
        );

        // Exactly ONE error_signatures row per stable signature ID (three, not six).
        let sig_sec = section(&pack, EvidenceClass::ErrorSignatures);
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = &sig_sec.log_summary else {
            panic!("error_signatures summary present");
        };
        let mut ids: Vec<&str> = signatures.iter().map(|r| r.signature_id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            ["log:v1:sig1", "log:v1:sig2", "log:v1:sig3"],
            "one coalesced row per signature ID"
        );
        assert_eq!(
            sig_sec.record_count, 4,
            "one hashed ErrorSignature node per stable ID (3) plus sig1's single \
             coalesced FRAME_RESOLVES_TO attribution edge (issue #371)"
        );

        // Summed (doubled) occurrence counts on the merged section nodes.
        for br in &sig_sec.records {
            if let Some(crate::ir::LogPayload::ErrorSignature(p)) = node_log_payload(&br.record) {
                let expected = match br.record.id() {
                    "log:v1:sig1" => 240, // 120 x 2 scans
                    "log:v1:sig2" => 60,  // 30 x 2
                    "log:v1:sig3" => 6,   // 3 x 2
                    other => panic!("unexpected signature {other}"),
                };
                assert_eq!(
                    p.occurrence_count,
                    expected,
                    "merged occurrence_count summed across scans for {}",
                    br.record.id()
                );
            }
        }

        // Occurrence-bucket totals DEDUP byte-identical rescan buckets by record ID
        // (issue #361, source-aware identity): this fixture concatenates the IDENTICAL
        // scan, so every bucket ID is duplicated with identical content and collapses
        // to one physical record. sig1's 15 in-window buckets carry counts (1..15),
        // counted ONCE.
        let occ_sec = section(&pack, EvidenceClass::OccurrenceBuckets);
        let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) = &occ_sec.log_summary
        else {
            panic!("occurrence_buckets summary present");
        };
        let sig1_total = signature_totals
            .iter()
            .find(|t| t.signature_id == "log:v1:sig1")
            .expect("sig1 total");
        // Single-scan sum is 1+2+..+15 = 120; a byte-identical rescan collapses, so
        // the total stays 120 (buckets no longer double-count on concatenation).
        assert_eq!(
            sig1_total.in_window_occurrences, 120,
            "sig1 in_window_occurrences dedups the byte-identical rescan buckets"
        );
        // One bucket row per stable bucket ID (15 for sig1), never doubled rows.
        assert_eq!(sig1_total.buckets.len(), 15, "one row per stable bucket ID");
    }

    #[test]
    fn assemble_merges_differing_extents_across_two_scans() {
        // Two DISTINCT scans of the same signature ID: earliest first_seen, latest
        // last_seen, SUMMED occurrence counts.
        let records = two_scan_differing_extents_records();
        let pack = assemble_cc73(&records);
        assert!(
            verify_pack(&pack).ok,
            "two-scan merged pack must pass its own verify"
        );

        let sig_sec = section(&pack, EvidenceClass::ErrorSignatures);
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = &sig_sec.log_summary else {
            panic!("error_signatures summary present");
        };
        assert_eq!(signatures.len(), 1, "one coalesced signature row");
        let row = &signatures[0];
        assert_eq!(row.signature_id, "log:v1:sigX");
        assert_eq!(
            row.first_seen_in_window, "2026-03-02T08:00:00Z",
            "merged first_seen is the EARLIEST across scans"
        );
        assert_eq!(
            row.last_seen_in_window, "2026-03-04T15:00:00Z",
            "merged last_seen is the LATEST across scans"
        );

        // Merged node occurrence_count = 10 + 25 = 35 (summed, not halved/doubled).
        let merged_count = sig_sec
            .records
            .iter()
            .find_map(|br| match node_log_payload(&br.record) {
                Some(crate::ir::LogPayload::ErrorSignature(p)) => Some(p.occurrence_count),
                _ => None,
            })
            .expect("merged signature node");
        assert_eq!(
            merged_count, 35,
            "occurrence_count summed across the two scans"
        );

        // Bucket total = 4 + 6 = 10 (distinct source-aware bucket IDs both sum).
        let occ_sec = section(&pack, EvidenceClass::OccurrenceBuckets);
        let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) = &occ_sec.log_summary
        else {
            panic!("occurrence_buckets summary present");
        };
        assert_eq!(signature_totals.len(), 1);
        assert_eq!(
            signature_totals[0].in_window_occurrences, 10,
            "bucket count summed across the two distinct sources"
        );
        assert_eq!(
            signature_totals[0].buckets.len(),
            2,
            "one row per DISTINCT source-aware bucket ID (issue #361)"
        );
    }

    #[test]
    fn assemble_output_always_passes_verify_across_fixtures() {
        // The assemble->verify-clean invariant across every #340 fixture and both
        // log-bearing controls: assemble must never produce a pack its own offline
        // verify rejects. Includes the concatenated multi-scan fixtures — the hole
        // that let the round-6 regression through (the suite had only single-scan
        // fixtures).
        let fixtures = [
            build_log_incident_records(),
            records_with_unattributed_bucket(),
            concatenated_multi_scan_records(),
            two_scan_differing_extents_records(),
        ];
        for records in &fixtures {
            for control in ["CC7.2", "CC7.3"] {
                let pack = assemble_pack(
                    records,
                    &load_default_catalog(),
                    control,
                    &win(),
                    1.0,
                    "test-0.0.0",
                    None,
                )
                .expect("assembles");
                let report = verify_pack(&pack);
                assert!(
                    report.ok,
                    "assemble<->verify invariant broken for {control}: {:?} / {:?}",
                    report.integrity, report.window_consistency
                );
            }
        }
    }

    #[test]
    fn verify_rejects_bucket_moved_to_wrong_signature() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
            .expect("occurrence_buckets section");
        {
            let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) =
                sec.log_summary.as_mut()
            else {
                panic!("occurrence_buckets summary present");
            };
            // Move one of sig1's buckets under sig2, adjusting BOTH sums so every
            // per-bucket count/hour/id still binds its node and both `sum` checks
            // still pass. The ONLY inconsistency is the signature the bucket is
            // filed under — which nothing bound before the AGGREGATES co-location.
            let sig1_idx = signature_totals
                .iter()
                .position(|t| t.signature_id == "log:v1:sig1")
                .expect("sig1 total");
            let moved = signature_totals[sig1_idx].buckets.remove(0);
            signature_totals[sig1_idx].in_window_occurrences -= moved.occurrence_count;
            let sig2 = signature_totals
                .iter_mut()
                .find(|t| t.signature_id == "log:v1:sig2")
                .expect("sig2 total");
            sig2.in_window_occurrences += moved.occurrence_count;
            sig2.buckets.push(moved);
            sig2.buckets.sort_by(|a, b| {
                a.hour
                    .cmp(&b.hour)
                    .then_with(|| a.bucket_id.cmp(&b.bucket_id))
            });
        }
        // Recompute the binding hash over the tampered summary (defeats layer 1).
        sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a bucket moved to the wrong signature must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn verify_rejects_resignatured_bucket_total() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
            .expect("occurrence_buckets section");
        {
            let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) =
                sec.log_summary.as_mut()
            else {
                panic!("occurrence_buckets summary present");
            };
            // Re-label a whole total's signature_id. Its buckets still bind their
            // nodes, but the AGGREGATES edges name the ORIGINAL signature.
            let total = signature_totals
                .iter_mut()
                .find(|t| t.signature_id == "log:v1:sig2")
                .expect("sig2 total");
            total.signature_id = "log:v1:sig1".to_owned();
            // Merge into one total per signature id would break bijection; keep it a
            // second sig1-labelled total to isolate the attribution check.
        }
        sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a re-signatured bucket total must fail Integrity: {}",
            report.integrity.detail
        );
    }

    #[test]
    fn systematic_occurrence_bucket_summary_mutations_fail_verify() {
        // For EVERY node-backed occurrence-bucket summary field and row operation,
        // mutate it, recompute `log_summary_hash` (so the whole-summary hash cannot
        // be what catches it), and assert verify reports an Integrity defect.
        type Mutation = fn(&mut Vec<SignatureOccurrenceTotal>);
        let cases: &[(&str, Mutation)] = &[
            ("inflate_total", |t| {
                t[0].in_window_occurrences += 1_000;
            }),
            ("deflate_total", |t| {
                t[0].in_window_occurrences = t[0].in_window_occurrences.saturating_sub(1);
            }),
            ("bucket_occurrence_count", |t| {
                t[0].buckets[0].occurrence_count += 7;
            }),
            ("bucket_hour", |t| {
                t[0].buckets[0].hour = "2026-03-20T00:00:00Z".to_owned();
            }),
            ("phantom_bucket_id", |t| {
                t[0].buckets[0].bucket_id = "log:v1:ghost-bucket".to_owned();
            }),
            ("drop_bucket", |t| {
                let dropped = t[0].buckets.remove(0);
                t[0].in_window_occurrences -= dropped.occurrence_count;
            }),
            ("drop_total", |t| {
                t.remove(0);
            }),
            ("rename_signature", |t| {
                t[0].signature_id = "log:v1:ghost-signature".to_owned();
            }),
            ("add_phantom_bucket_entry", |t| {
                let ghost = BucketCount {
                    bucket_id: "log:v1:ghost-bucket".to_owned(),
                    hour: "2026-03-02T00:00:00Z".to_owned(),
                    occurrence_count: 5,
                };
                t[0].in_window_occurrences += ghost.occurrence_count;
                t[0].buckets.push(ghost);
            }),
            ("move_bucket_between_signatures", |t| {
                let moved = t[0].buckets.remove(0);
                t[0].in_window_occurrences -= moved.occurrence_count;
                t[1].in_window_occurrences += moved.occurrence_count;
                t[1].buckets.push(moved);
                t[1].buckets.sort_by(|a, b| {
                    a.hour
                        .cmp(&b.hour)
                        .then_with(|| a.bucket_id.cmp(&b.bucket_id))
                });
            }),
            // Split one signature's buckets across two totals for the SAME
            // `signature_id`. Before the fix each partial total equalled its own
            // bucket sum and the reverse bucket-coverage guard still saw every
            // bucket, so Integrity passed while a consumer reading per-signature
            // totals saw the count under-reported by whichever total it read —
            // the round-3 under-reporting failure via row duplication (issue #373,
            // Codex round-5 P2).
            ("duplicate_total_split_buckets", |t| {
                let half = t[0].buckets.len() / 2;
                let moved: Vec<BucketCount> = t[0].buckets.split_off(half);
                let moved_sum: u64 = moved.iter().map(|b| b.occurrence_count).sum();
                let mut dup = t[0].clone();
                t[0].in_window_occurrences -= moved_sum;
                dup.in_window_occurrences = moved_sum;
                dup.buckets = moved;
                t.push(dup);
            }),
            // Append an empty zero total for an existing `signature_id`. Its sum
            // (0) trivially equals `in_window_occurrences` and it touches no bucket,
            // so before the fix Integrity passed even though the summary now carries
            // two totals for one signature.
            ("duplicate_total_empty_zero", |t| {
                let mut dup = t[0].clone();
                dup.buckets.clear();
                dup.in_window_occurrences = 0;
                t.push(dup);
            }),
        ];
        for (name, mutate) in cases {
            let mut pack = assemble_cc73(&build_log_incident_records());
            let sec = pack
                .sections
                .iter_mut()
                .find(|s| s.class == EvidenceClass::OccurrenceBuckets.as_wire())
                .expect("occurrence_buckets section");
            if let Some(LogEvidenceSummary::OccurrenceBuckets { signature_totals }) =
                sec.log_summary.as_mut()
            {
                mutate(signature_totals);
            }
            sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
            let report = verify_pack(&pack);
            assert!(
                !report.integrity.passed,
                "occurrence-bucket mutation `{name}` must fail Integrity (hash recomputed)"
            );
        }
    }

    #[test]
    fn systematic_error_signature_summary_mutations_fail_verify() {
        // Every NODE-BACKED error-signature summary field + row operation must fail
        // verify even with the whole-summary hash recomputed. (The exemplar fields
        // ride the LogEvent node, which CANNOT be co-located — it carries
        // redaction-scrubbed excerpt text whose presence would violate the
        // zero-raw-log Safety invariant — so those are bound solely by the
        // whole-summary hash and are exercised separately. The `frame_resolutions`
        // field IS now independently bound to co-located FRAME_RESOLVES_TO edges
        // (issue #371) and is exercised by its own systematic suite.)
        type Mutation = fn(&mut Vec<ErrorSignatureRow>);
        let cases: &[(&str, Mutation)] = &[
            ("template_hash", |s| {
                s[0].template_hash = blake3::hash(b"forged template").to_string();
            }),
            ("frame_chain_hash", |s| {
                let sig1 = s
                    .iter_mut()
                    .find(|r| r.signature_id == "log:v1:sig1")
                    .expect("sig1 carries frames");
                sig1.frame_chain_hash = Some(blake3::hash(b"forged frames").to_string());
            }),
            ("severity", |s| {
                s[0].severity = "warn".to_owned();
            }),
            ("first_seen", |s| {
                s[0].first_seen_in_window = "2026-03-15T00:00:00Z".to_owned();
            }),
            ("last_seen", |s| {
                s[0].last_seen_in_window = "2026-03-15T00:00:00Z".to_owned();
            }),
            ("drop_signature_row", |s| {
                s.remove(0);
            }),
            ("add_phantom_signature_row", |s| {
                let mut ghost = s[0].clone();
                ghost.signature_id = "log:v1:ghost-signature".to_owned();
                s.push(ghost);
            }),
            // Duplicate an existing signature row verbatim (same `signature_id`).
            // Before the fix the `BTreeSet` of summary IDs collapsed the pair, so
            // the bijection still balanced and each duplicate bound the same node —
            // yet the summary no longer had one row per signature and a duplicate
            // could carry divergent exemplar/frame-resolution fields bound only by
            // the whole-summary hash (issue #373, Codex round-5 P2).
            ("duplicate_signature_row", |s| {
                let dup = s[0].clone();
                s.push(dup);
            }),
        ];
        for (name, mutate) in cases {
            let mut pack = assemble_cc73(&build_log_incident_records());
            let sec = pack
                .sections
                .iter_mut()
                .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
                .expect("error_signatures section");
            if let Some(LogEvidenceSummary::ErrorSignatures { signatures }) =
                sec.log_summary.as_mut()
            {
                mutate(signatures);
            }
            sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
            let report = verify_pack(&pack);
            assert!(
                !report.integrity.passed,
                "error-signature mutation `{name}` must fail Integrity (hash recomputed)"
            );
        }
    }

    #[test]
    fn systematic_frame_resolution_summary_mutations_fail_verify() {
        // Issue #371: the `frame_resolution` label and `frame_index` on an
        // error_signatures row are now INDEPENDENTLY bound to the co-located
        // `FRAME_RESOLVES_TO` edges (mirroring the #340/#365 AGGREGATES bind), not
        // just the whole-summary hash. Every case ALSO recomputes the whole-summary
        // `log_summary_hash` (and `record_count` when it touches records), so the
        // independent edge recompute — never the whole-summary hash — is the only
        // thing that can catch it. Each mutation moves, drops, adds, relabels, or
        // retargets a frame resolution from either the summary side or the co-located
        // edge side, and must fail Integrity.
        type Mutation = fn(&mut EvidenceSection);
        let cases: &[(&str, Mutation)] = &[
            // Relabel the resolution in the SUMMARY only: the co-located edge still
            // says `resolved`, so the per-row recompute diverges.
            ("relabel_resolution_in_summary", |sec| {
                if let Some(LogEvidenceSummary::ErrorSignatures { signatures }) =
                    sec.log_summary.as_mut()
                {
                    let sig1 = signatures
                        .iter_mut()
                        .find(|r| r.signature_id == "log:v1:sig1")
                        .expect("sig1 carries a frame join");
                    sig1.frame_resolutions[0].resolution = Some("unresolved".to_owned());
                }
            }),
            // Move the `frame_index` in the SUMMARY only.
            ("move_frame_index_in_summary", |sec| {
                if let Some(LogEvidenceSummary::ErrorSignatures { signatures }) =
                    sec.log_summary.as_mut()
                {
                    let sig1 = signatures
                        .iter_mut()
                        .find(|r| r.signature_id == "log:v1:sig1")
                        .expect("sig1 carries a frame join");
                    sig1.frame_resolutions[0].frame_index = Some(9);
                }
            }),
            // Retarget the resolution in the SUMMARY only.
            ("retarget_resolution_in_summary", |sec| {
                if let Some(LogEvidenceSummary::ErrorSignatures { signatures }) =
                    sec.log_summary.as_mut()
                {
                    let sig1 = signatures
                        .iter_mut()
                        .find(|r| r.signature_id == "log:v1:sig1")
                        .expect("sig1 carries a frame join");
                    sig1.frame_resolutions[0].target_id = "codegraph:v5:ghost".to_owned();
                }
            }),
            // Drop the co-located FRAME_RESOLVES_TO edge but keep the summary join —
            // the row now claims a frame resolution with no backing edge.
            ("drop_frame_edge", |sec| {
                sec.records.retain(|br| {
                    !matches!(&br.record,
                        GraphRecord::Edge { label, .. } if label.as_str() == "FRAME_RESOLVES_TO")
                });
                sec.record_count = sec.records.len();
            }),
            // Relabel the co-located EDGE (rehashing its row) while leaving the
            // summary join untouched — the summary no longer matches the edge it is
            // bound to.
            ("relabel_frame_edge_row", |sec| {
                sec.records.retain(|br| {
                    !matches!(&br.record,
                        GraphRecord::Edge { label, .. } if label.as_str() == "FRAME_RESOLVES_TO")
                });
                let relabeled = super::fixture::frame_resolves_to(
                    "log:v1:sig1",
                    "codegraph:v5:sym-db",
                    crate::ir::FrameResolution::Unresolved,
                    0,
                );
                let scrubbed = scrub_log_node_text(scrub_record(relabeled));
                let json = serde_json::to_string(&scrubbed).unwrap();
                let hash = blake3::hash(json.as_bytes()).to_string();
                sec.records.push(BundleRecord {
                    record: scrubbed,
                    hash,
                });
                sec.records
                    .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
                sec.record_count = sec.records.len();
            }),
            // Smuggle a NEW FRAME_RESOLVES_TO edge sourced at sig2 (which the summary
            // says has NO frames) into the section. Membership admits it (sourced at a
            // present signature), but the per-row recompute rejects sig2's now-nonempty
            // reconstruction against its empty summary join.
            ("add_smuggled_frame_edge", |sec| {
                let edge = super::fixture::frame_resolves_to(
                    "log:v1:sig2",
                    "codegraph:v5:sym-db",
                    crate::ir::FrameResolution::Resolved,
                    0,
                );
                let scrubbed = scrub_log_node_text(scrub_record(edge));
                let json = serde_json::to_string(&scrubbed).unwrap();
                let hash = blake3::hash(json.as_bytes()).to_string();
                sec.records.push(BundleRecord {
                    record: scrubbed,
                    hash,
                });
                sec.records
                    .sort_by(|a, b| section_sort_key(&a.record).cmp(&section_sort_key(&b.record)));
                sec.record_count = sec.records.len();
            }),
        ];
        for (name, mutate) in cases {
            let mut pack = assemble_cc73(&build_log_incident_records());
            let sec = pack
                .sections
                .iter_mut()
                .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
                .expect("error_signatures section");
            mutate(sec);
            // Recompute the whole-summary hash so it can NEVER be what catches the
            // tamper — only the independent frame-edge recompute can.
            sec.log_summary_hash = Some(hash_log_summary(sec.log_summary.as_ref().unwrap()));
            let report = verify_pack(&pack);
            assert!(
                !report.integrity.passed,
                "frame-resolution mutation `{name}` must fail Integrity (hash recomputed)"
            );
        }
    }

    // ── Binding-surface boundary for the non-node-backed fields ───────────────
    // The exemplar (`protected_handle`/`content_hash`/`source_line`) fields on an
    // `error_signatures` row, and EVERY `remediation_links` field, ride edges/nodes
    // that CANNOT be co-located as hashed rows: the `LogEvent` exemplar node carries
    // redaction-scrubbed excerpt text whose presence would violate the zero-raw-log
    // Safety invariant, and the `remediation_links` section is defined to carry ZERO
    // hashed rows (a remediation commit may legitimately fall OUTSIDE the evidence
    // window). Their SOLE binding surface is therefore the whole-summary
    // `log_summary_hash`: a tamper that does NOT recompute that hash fails Integrity
    // (asserted here); a tamper that also recomputes it is equivalent to re-deriving
    // the summary and is the pack's general fabricate-a-consistent-artifact threat,
    // out of scope for an offline internal-consistency check. (The `frame_resolutions`
    // field is the EXCEPTION closed by issue #371 — it now co-locates its
    // FRAME_RESOLVES_TO edges and is independently bound, tested above.)

    #[test]
    fn exemplar_handle_tamper_is_caught_by_whole_summary_hash() {
        let records = build_log_incident_records();
        let mut pack = assemble_cc73(&records);
        let sec = pack
            .sections
            .iter_mut()
            .find(|s| s.class == EvidenceClass::ErrorSignatures.as_wire())
            .expect("error_signatures section");
        let Some(LogEvidenceSummary::ErrorSignatures { signatures }) = sec.log_summary.as_mut()
        else {
            panic!("error_signatures summary present");
        };
        let sig1 = signatures
            .iter_mut()
            .find(|s| s.signature_id == "log:v1:sig1")
            .expect("sig1 carries an exemplar");
        // Rewrite the exemplar content hash WITHOUT recomputing log_summary_hash.
        sig1.exemplars[0].content_hash = "cd".repeat(32);
        // (Deliberately leave `sec.log_summary_hash` stale — layer 1 must catch it.)
        let report = verify_pack(&pack);
        assert!(
            !report.integrity.passed,
            "a rewritten exemplar handle must fail Integrity via the whole-summary hash: {}",
            report.integrity.detail
        );
    }

    // ── Whole-summary tamper suite (Codex round-4 P1) ─────────────────────────
    // The prior mutation suites tamper INDIVIDUAL summary fields; they never
    // stripped a present log section's `log_summary` / `log_summary_hash`
    // wholesale. Stripping BOTH on a present log section — especially
    // `remediation_links`, whose evidence exists ONLY in the summary (zero hashed
    // rows) — silently dropped all that derived evidence yet still verified. Verify
    // must reject a present log section missing either the summary or its hash.

    #[test]
    fn verify_rejects_present_log_section_with_summary_stripped() {
        for class in [
            EvidenceClass::ErrorSignatures,
            EvidenceClass::OccurrenceBuckets,
            EvidenceClass::RemediationLinks,
        ] {
            for (name, mutate) in [
                (
                    "summary_only",
                    (|s: &mut EvidenceSection| s.log_summary = None) as fn(&mut EvidenceSection),
                ),
                (
                    "hash_only",
                    (|s: &mut EvidenceSection| s.log_summary_hash = None)
                        as fn(&mut EvidenceSection),
                ),
                (
                    "both",
                    (|s: &mut EvidenceSection| {
                        s.log_summary = None;
                        s.log_summary_hash = None;
                    }) as fn(&mut EvidenceSection),
                ),
            ] {
                let mut pack = assemble_cc73(&build_log_incident_records());
                let sec = pack
                    .sections
                    .iter_mut()
                    .find(|s| s.class == class.as_wire())
                    .unwrap_or_else(|| panic!("{} section", class.as_wire()));
                assert_eq!(sec.status, "present", "{} is present", class.as_wire());
                mutate(sec);
                let report = verify_pack(&pack);
                assert!(
                    !report.integrity.passed,
                    "present {} section with `{name}` stripped must fail Integrity: {}",
                    class.as_wire(),
                    report.integrity.detail
                );
            }
        }
    }
}
