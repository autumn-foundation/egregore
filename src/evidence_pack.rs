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
//     merged — detected at runtime and, when absent, degraded to a single
//     `capability_unavailable` diagnostic naming #334 with zero rows.
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
use crate::ir::GraphRecord;
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
    /// Review coverage met `--min-review-coverage`.
    pub review_coverage: VerificationVerdict,
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

/// Maps a record to its catalog evidence class, when it maps to one.
#[must_use]
pub fn evidence_class_for_record(record: &GraphRecord) -> Option<EvidenceClass> {
    let GraphRecord::Node {
        kind, source_kind, ..
    } = record
    else {
        return None;
    };
    match kind.as_str() {
        "Commit" => Some(EvidenceClass::Commits),
        "PR" => Some(EvidenceClass::PullRequests),
        "Task" if source_kind.as_deref() == Some("github_pr") => Some(EvidenceClass::PullRequests),
        "Review" => Some(EvidenceClass::Reviews),
        "Verification" | "CommandRun" | "TestRun" | "CIStatus" | "CommandEvidence"
        | "BenchmarkRun" | "CoverageReport" | "ProofResult" => {
            Some(EvidenceClass::VerificationEvidence)
        }
        "ErrorSignature" => Some(EvidenceClass::ErrorSignatures),
        "LogOccurrenceBucket" => Some(EvidenceClass::OccurrenceBuckets),
        _ => None,
    }
}

/// Stable unavailable reason for a class whose domain is absent.
#[must_use]
const fn unavailable_reason(class: EvidenceClass) -> &'static str {
    match class {
        EvidenceClass::Commits => "commit_domain_absent",
        EvidenceClass::PullRequests => "pull_request_domain_absent",
        EvidenceClass::Reviews => "review_domain_absent",
        EvidenceClass::ReviewCoverage => "no_pull_requests_to_measure",
        EvidenceClass::StructuralDeltas | EvidenceClass::PublicApiDeltas => "delta_domain_absent",
        EvidenceClass::ValidationRuns => "validation_domain_absent",
        EvidenceClass::VerificationEvidence => "verification_domain_absent",
        EvidenceClass::ErrorSignatures
        | EvidenceClass::OccurrenceBuckets
        | EvidenceClass::RemediationLinks => "log_domain_absent",
    }
}

/// True when the record is a merged PR task (`source_kind == github_pr` and a
/// merge marker is present).
fn is_merged_pr(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node {
            merged_at: Some(_),
            source_kind,
            ..
        } | GraphRecord::Node {
            merge_commit_sha: Some(_),
            source_kind,
            ..
        } if source_kind.as_deref() == Some("github_pr")
    )
}

/// True when the record is an approving pull-request review.
fn is_approving_review(record: &GraphRecord) -> bool {
    matches!(
        record,
        GraphRecord::Node {
            kind,
            review_state: Some(state),
            ..
        } if kind.as_str() == "Review" && state == "approved"
    )
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
    let mut per_class: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
    let mut code_total = 0usize;
    let mut code_cited = 0usize;
    let mut non_code_ok = true;
    for br in rows {
        let classified = classify_record_external(&br.record);
        let trust = classified.trust_class.to_owned();
        let cited = classified.status != CitationStatus::MissingRequiredHandle;
        let entry = per_class.entry(trust.clone()).or_insert((0, 0, 0));
        entry.0 += 1;
        if cited {
            entry.1 += 1;
        } else {
            entry.2 += 1;
        }
        if classified.trust_class == "source_fact" {
            code_total += 1;
            if cited {
                code_cited += 1;
            }
        } else if !cited {
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
            |(trust_class, (total, cited, missing))| ClassCitationTally {
                trust_class,
                total,
                cited,
                missing,
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
        match resolve_valid_time(record) {
            Some(vt) if in_window(&vt, window) => {
                in_window_by_class
                    .entry(class.as_wire())
                    .or_default()
                    .push(record.clone());
            }
            Some(_) => {} // out of window: excluded, no diagnostic
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
    let mut merged_pr_ids: Vec<String> = Vec::new();
    for record in records {
        if is_merged_pr(record)
            && let Some(vt) = resolve_valid_time(record)
            && in_window(&vt, window)
        {
            merged_pr_ids.push(record.id().to_owned());
        }
    }
    merged_pr_ids.sort();
    merged_pr_ids.dedup();
    // A PR is approved when an approving Review references it via REFERENCES_TASK
    // AND that review resolves inside the same half-open pack window. An
    // approving review whose valid time falls before `from` or at/after `to`,
    // or that has no resolvable valid time, is omitted from the windowed
    // `reviews` section, so it must not count toward approval either — otherwise
    // the pack would suppress the gap while showing zero in-window approval.
    let approving_targets: BTreeSet<String> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Edge {
                label,
                source,
                target,
                ..
            } if label.as_str() == "REFERENCES_TASK" => {
                let approving = records.iter().any(|rec| {
                    rec.id() == source
                        && is_approving_review(rec)
                        && resolve_valid_time(rec).is_some_and(|vt| in_window(&vt, window))
                });
                approving.then(|| target.clone())
            }
            _ => None,
        })
        .collect();
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
    let review_measurement = ReviewCoverageMeasurement {
        merged_pr_count,
        approved_pr_count,
        coverage,
        min_required: min_review_coverage,
        passed: review_coverage_passed,
        unapproved_pr_ids,
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
            Vec::new()
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
        window,
        &merged_pr_ids,
        &approving_targets,
        &mut diagnostics,
    );

    // --- citation verdict ---
    let row_refs: Vec<&BundleRecord> = all_section_rows.iter().collect();
    let (citation_tallies, code_pass, non_code_pass) = citation_view(&row_refs);
    let citation_ok = code_pass && non_code_pass;

    // --- integrity + safety are structurally guaranteed at assemble time ---
    let integrity = VerificationVerdict {
        passed: true,
        detail: "records hashed over scrubbed form; sections canonically ordered".to_owned(),
    };
    let (safety_passed, safety_detail) = pack_safety(&all_section_rows);
    let safety = VerificationVerdict {
        passed: safety_passed,
        detail: safety_detail,
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
    let review_coverage = VerificationVerdict {
        passed: review_coverage_passed,
        detail: format!("review coverage {coverage:.4} vs minimum {min_review_coverage:.4}"),
    };

    let ok = required_passed
        && citation_ok
        && review_coverage_passed
        && integrity.passed
        && safety.passed;

    let verdicts = PackVerdicts {
        ok,
        required_classes,
        citation,
        review_coverage,
        integrity,
        safety,
        citation_tallies,
    };

    // --- manifest counts ---
    let mut included_record_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut tuple_counts: BTreeMap<String, usize> = BTreeMap::new();
    for br in &all_section_rows {
        let tc = citation_trust_class(&br.record).to_owned();
        *included_record_counts.entry(tc).or_insert(0) += 1;
        *tuple_counts.entry(tuple_key(&br.record)).or_insert(0) += 1;
    }

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

    Ok(EvidencePack {
        manifest,
        sections,
        gaps,
        verdicts,
        diagnostics,
    })
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

fn diagnostic_sort_key(d: &PackDiagnostic) -> (String, String, String) {
    (
        d.code.clone(),
        d.evidence_class.clone().unwrap_or_default(),
        d.record_ids.first().cloned().unwrap_or_default(),
    )
}

/// Derives the closed set of gap rows (AC5). Two classes require issue #334
/// facts; when those facts are absent a single `capability_unavailable`
/// diagnostic is emitted and zero rows are produced for them.
fn derive_gaps(
    records: &[GraphRecord],
    window: &Window,
    merged_pr_ids: &[String],
    approving_targets: &BTreeSet<String>,
    diagnostics: &mut Vec<PackDiagnostic>,
) -> Vec<GapRow> {
    let mut gaps: Vec<GapRow> = Vec::new();
    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();

    // merged_pr_without_approving_review
    for pr_id in merged_pr_ids {
        if !approving_targets.contains(pr_id) {
            let vt = by_id
                .get(pr_id.as_str())
                .and_then(|r| resolve_valid_time(r));
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

    // commit_outside_any_pr: in-window commit not targeted by any MERGED_AS edge
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

    // missing_valid_time: class-relevant records with no resolvable valid time
    for record in records {
        if evidence_class_for_record(record).is_some() && resolve_valid_time(record).is_none() {
            gaps.push(GapRow {
                gap_class: GapClass::MissingValidTime.as_wire().to_owned(),
                record_ids: vec![record.id().to_owned()],
                valid_time: None,
                detail: "class-relevant record has no resolvable valid time".to_owned(),
            });
        }
    }

    // #334-dependent classes: detect the backing facts; degrade when absent.
    let has_issue_334_facts = records.iter().any(record_has_reviewed_commit_fact);
    if has_issue_334_facts {
        // (Populated automatically once #334 lands; no facts to derive from yet.)
    } else {
        diagnostics.push(PackDiagnostic {
            code: "capability_unavailable".to_owned(),
            evidence_class: None,
            unavailable_reason: Some("issue_334_reviewed_commit_facts_absent".to_owned()),
            record_ids: Vec::new(),
            detail: "gap classes review_unanchored_no_commit_sha and \
                     approval_precedes_final_head require issue #334 reviewed-commit \
                     facts (review_commit_sha / REVIEWS_COMMIT), which are not present"
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

/// Detects whether a record carries issue #334 reviewed-commit facts.
///
/// #334 is not merged: there is no `review_commit_sha` field and no
/// `REVIEWS_COMMIT` edge. This probes for both so the two dependent gap classes
/// populate automatically once #334 lands, and degrade cleanly until then.
fn record_has_reviewed_commit_fact(record: &GraphRecord) -> bool {
    match record {
        GraphRecord::Edge { label, .. } => label.as_str() == "REVIEWS_COMMIT",
        GraphRecord::Node { .. } => {
            serde_json::to_string(record).is_ok_and(|json| json.contains("\"review_commit_sha\""))
        }
        GraphRecord::Tombstone { .. } => false,
    }
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
        if let GraphRecord::Node {
            text,
            validation_summary,
            arguments_summary,
            arguments_handle,
            result_handle,
            stdout_handle,
            stderr_handle,
            patch_handle,
            body_handle,
            diff_hunk_handle,
            ..
        } = &br.record
        {
            if text.is_some() || validation_summary.is_some() || arguments_summary.is_some() {
                return (false, format!("record {record_id} retains raw prose"));
            }
            let inline_leak = [
                arguments_handle,
                result_handle,
                stdout_handle,
                stderr_handle,
                body_handle,
                diff_hunk_handle,
            ]
            .into_iter()
            .flatten()
            .any(|h| h.inline.is_some())
                || patch_handle.as_ref().is_some_and(|h| h.inline.is_some());
            if inline_leak {
                return (false, format!("record {record_id} retains inline payload"));
            }
        }
    }
    (
        true,
        "no raw sensitive classes; scrubbed prose/handle fields are None".to_owned(),
    )
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
        for pair in section.records.windows(2) {
            if section_sort_key(&pair[0].record) > section_sort_key(&pair[1].record) {
                integrity_passed = false;
                integrity_detail =
                    format!("section {} rows are not canonically ordered", section.class);
                break 'integrity;
            }
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

    // Safety.
    let owned_rows: Vec<BundleRecord> = all_rows.iter().map(|br| (*br).clone()).collect();
    let (safety_passed, safety_detail) = pack_safety(&owned_rows);
    let safety = VerificationVerdict {
        passed: safety_passed,
        detail: safety_detail,
    };

    // Window consistency.
    let mut window_ok = true;
    let mut window_detail = "every row's valid time is inside the manifest window".to_owned();
    'window: for section in &pack.sections {
        for br in &section.records {
            match resolve_valid_time(&br.record) {
                Some(vt) if in_window(&vt, &pack.manifest.window) => {}
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
        // pr05 has no review at all.
        let reviews = [
            ("project:v1:rv01", "project:v1:pr01", "approved"),
            ("project:v1:rv02", "project:v1:pr02", "approved"),
            ("project:v1:rv03", "project:v1:pr03", "approved"),
            ("project:v1:rv04", "project:v1:pr04", "commented"),
            ("project:v1:rv05", "project:v1:pr06", "changes_requested"),
        ];
        for (rid, pid, state) in reviews {
            records.push(review(rid, &march(4, 8), state));
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

    /// Positive companion: an in-window approving review DOES suppress the gap.
    #[test]
    fn in_window_approving_review_suppresses_gap() {
        use super::fixture::{pr, references_task, review};
        let records = vec![
            pr("project:v1:prX", "2026-03-15T12:00:00Z", "cX"),
            review("project:v1:rvX", "2026-03-16T08:00:00Z", "approved"),
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
