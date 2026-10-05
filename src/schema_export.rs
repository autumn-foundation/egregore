//! JSON Schema (draft 2020-12) documents for the persisted graph record contracts.
//!
//! Issue #226. The record shapes are derived from the Rust types in
//! [`crate::ir`] via `schemars` — never from a disconnected static schema —
//! and then specialized per `(domain, kind, schema_version)` tuple by the
//! contract registry in this module. The registry is built from the same
//! exhaustive inventories the rest of the codebase uses ([`NodeKind::ALL`],
//! [`EdgeLabel::ALL`], and the domain helpers in [`crate::schema_version`]);
//! the integration spec asserts every inventory entry is addressable, so a
//! newly added kind or label without registry coverage fails the test suite
//! instead of silently vanishing.
//!
//! Every emitted document:
//! - declares `$schema: https://json-schema.org/draft/2020-12/schema`,
//! - carries a stable `$id` under `https://github.com/madmax983/egregore/schemas/…`,
//! - pins `kind`/`label`, `schema_version`, and the record `id` prefix to the
//!   addressed tuple,
//! - requires the base record fields plus the kind's provenance fields and the
//!   producer envelope (the envelope is required only at the domain's current
//!   version: legacy records predate it, see
//!   `docs/schema/producer-version.md` §4),
//! - forbids subjective/agent-authored fields on deterministic code facts
//!   (trust separation, `docs/schema/producer-version.md` §10).
//!
//! [`NodeKind::ALL`]: crate::ir::NodeKind::ALL
//! [`EdgeLabel::ALL`]: crate::ir::EdgeLabel::ALL

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde_json::{Value, json};

use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, ARTIFACT_SCHEMA_VERSION, EdgeLabel, GraphRecord,
    LOG_SCHEMA_VERSION, NodeKind, PROJECT_SCHEMA_VERSION, SCHEMA_VERSION, SEMANTIC_SCHEMA_VERSION,
    USER_CONTEXT_SCHEMA_VERSION, VERIFICATION_SCHEMA_VERSION,
};
use crate::schema_version::{domain_for_edge_label, domain_for_node_kind};

/// Draft declared by every emitted document.
pub const JSON_SCHEMA_DRAFT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Stable `$id` namespace for emitted documents.
pub const SCHEMA_ID_BASE: &str = "https://github.com/madmax983/egregore/schemas";

/// The three persisted record shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecordType {
    /// A graph node (`GraphRecord::Node`).
    Node,
    /// A graph edge (`GraphRecord::Edge`).
    Edge,
    /// A deletion marker (`GraphRecord::Tombstone`).
    Tombstone,
}

impl RecordType {
    /// The wire `record_type` tag value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Edge => "edge",
            Self::Tombstone => "tombstone",
        }
    }
}

/// One addressable persisted-record contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchemaRef {
    /// Persisted domain (`"codegraph"`, `"agent_memory"`, …).
    pub domain: &'static str,
    /// Record shape.
    pub record_type: RecordType,
    /// Node kind name, edge label, or `"Tombstone"`.
    pub kind: &'static str,
    /// Addressed schema version.
    pub version: u32,
}

impl SchemaRef {
    /// Stable `$id` for this contract's document.
    #[must_use]
    pub fn id(&self) -> String {
        format!(
            "{SCHEMA_ID_BASE}/{}/{}/{}/v{}",
            self.domain,
            self.record_type.as_str(),
            self.kind,
            self.version
        )
    }

    /// Deterministic file name for `--out` exports.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!(
            "{}.{}.{}.v{}.schema.json",
            self.domain,
            self.record_type.as_str(),
            self.kind,
            self.version
        )
    }
}

/// Current schema version for a domain, or `None` for an unknown domain.
#[must_use]
pub fn current_version(domain: &str) -> Option<u32> {
    match domain {
        "codegraph" => Some(SCHEMA_VERSION),
        "agent_memory" => Some(AGENT_MEMORY_SCHEMA_VERSION),
        "verification" => Some(VERIFICATION_SCHEMA_VERSION),
        "artifact" => Some(ARTIFACT_SCHEMA_VERSION),
        "project" => Some(PROJECT_SCHEMA_VERSION),
        "semantic" => Some(SEMANTIC_SCHEMA_VERSION),
        "user_context" => Some(USER_CONTEXT_SCHEMA_VERSION),
        "log" => Some(LOG_SCHEMA_VERSION),
        _ => None,
    }
}

/// Schema version each domain had when the producer envelope was introduced
/// (commit cabd5a7: `Producer` struct + `stamp_producer`). A domain whose
/// current version is unchanged since then has current-version records that
/// predate the envelope — they are legacy-shaped w.r.t. the envelope, and
/// `docs/schema/producer-version.md` §6 requires readers to tolerate a
/// missing `producer` on them (`legacy_pre_v1`) rather than reject them.
/// Only a domain whose version advanced past the introduction (codegraph
/// 6→10, log 2→3) has a current version whose records all postdate the
/// envelope, so only there is a missing `producer` malformed rather than
/// legacy.
#[must_use]
fn producer_envelope_introduced_at(domain: &str) -> u32 {
    match domain {
        "codegraph" => 6,
        "log" => 2,
        _ => 1,
    }
}

/// Reader-accepted versions for a domain (`docs/schema/schema-versioning.md`
/// §3): codegraph accepts `1..=SCHEMA_VERSION`, log accepts `2..=3`, every
/// other domain accepts only its current version.
fn accepted_versions(domain: &str) -> Option<Vec<u32>> {
    let current = current_version(domain)?;
    let min = match domain {
        "codegraph" => 1,
        "log" => 2,
        _ => current,
    };
    Some((min..=current).collect())
}

/// Legacy cross-domain node placements the daemon documents for importer
/// records (see the `LEGACY_DOMAIN_OVERRIDES`-style comment in
/// `src/daemon.rs`): old trajectory importers wrote these kinds into the
/// `agent_memory` domain, and importer diagnostics land in `project`.
const CROSS_DOMAIN_NODES: &[(&str, &str)] = &[
    ("agent_memory", "PatchArtifact"),
    ("agent_memory", "Task"),
    ("agent_memory", "Artifact"),
    ("agent_memory", "CommandEvidence"),
    ("agent_memory", "CommandRun"),
    ("agent_memory", "Diagnostic"),
    ("agent_memory", "Verification"),
];

/// The eight persisted domains, in documentation order.
const ALL_DOMAINS: &[&str] = &[
    "codegraph",
    "agent_memory",
    "verification",
    "artifact",
    "project",
    "semantic",
    "user_context",
    "log",
];

/// Every addressable `(domain, kind, schema_version)` contract.
///
/// Built from [`NodeKind::ALL`] / [`EdgeLabel::ALL`] plus the legacy
/// cross-domain placements; the integration spec asserts every inventory
/// entry is addressable, so a newly added kind or label without registry
/// coverage fails the test suite instead of silently vanishing.
#[must_use]
pub fn all_schema_refs() -> Vec<SchemaRef> {
    let mut refs = Vec::new();
    for kind in NodeKind::ALL {
        let domain = domain_for_node_kind(kind.as_str());
        if let Some(versions) = accepted_versions(domain) {
            for version in versions {
                refs.push(SchemaRef {
                    domain,
                    record_type: RecordType::Node,
                    kind: kind.as_str(),
                    version,
                });
            }
        }
    }
    for (domain, kind) in CROSS_DOMAIN_NODES {
        if let Some(version) = current_version(domain) {
            refs.push(SchemaRef {
                domain,
                record_type: RecordType::Node,
                kind,
                version,
            });
        }
    }
    for label in EdgeLabel::ALL {
        let domain = domain_for_edge_label(label.as_str());
        if let Some(versions) = accepted_versions(domain) {
            for version in versions {
                refs.push(SchemaRef {
                    domain,
                    record_type: RecordType::Edge,
                    kind: label.as_str(),
                    version,
                });
            }
        }
    }
    for domain in ALL_DOMAINS {
        if let Some(versions) = accepted_versions(domain) {
            for version in versions {
                refs.push(SchemaRef {
                    domain,
                    record_type: RecordType::Tombstone,
                    kind: "Tombstone",
                    version,
                });
            }
        }
    }
    refs.sort_by(|a, b| {
        (a.domain, a.record_type, a.kind, a.version).cmp(&(
            b.domain,
            b.record_type,
            b.kind,
            b.version,
        ))
    });
    refs
}

/// Look up the contract for an addressed tuple.
#[must_use]
pub fn find_schema_ref(domain: &str, kind: &str, version: u32) -> Option<SchemaRef> {
    all_schema_refs()
        .into_iter()
        .find(|r| r.domain == domain && r.kind == kind && r.version == version)
}

// ── Contract registry: provenance, trust, and forbidden fields ──────────────
//
// Required-field sources are the schema documents under `docs/schema/`:
// agent-memory.md §3–§4, agent-actions.md §4–§5, verification.md,
// project-graph.md, user-context.md, semantic-drift.md, log-graph.md,
// and the producer field sets in src/codex.rs, src/claude_code.rs,
// src/traj.rs, src/test_capture.rs, src/log_graph.rs, and src/cli/evidence.rs.

/// Subjective/agent-authored fields a deterministic code fact must never
/// carry (`docs/schema/agent-memory.md` §3: "Code-graph nodes (emitted by the
/// extractor) do not carry these fields").
///
/// `redaction_policy_version` is deliberately absent: the extractor stamps it
/// when doc fields pass through redaction, so it is a legitimate code-fact
/// field. `domain` is absent too: it is value-constrained to the tuple's
/// domain when present rather than forbidden. `confidence` is forbidden on
/// code-fact *nodes* only — edges carry a legitimate extraction-confidence
/// string, so the edge schemas do not forbid it.
const CODE_FACT_FORBIDDEN: &[&str] = &[
    "text",
    "agent_id",
    "agent_kind",
    "session_id",
    "observed_at",
    "confidence",
    "source_handle",
    "importer_id",
    "importer_version",
    "source_artifact_path",
    "source_artifact_hash",
    "evidence_links",
];

/// Generic agent-memory provenance (`docs/schema/agent-memory.md` §3).
/// Kept as documentation of the §3 field set; the per-kind arms above spell
/// their fields out explicitly for auditability.
#[allow(dead_code)]
const AGENT_PROVENANCE: &[&str] = &[
    "agent_id",
    "agent_kind",
    "session_id",
    "observed_at",
    "ingested_at",
];

/// Temporal provenance required by the project, user-context, semantic, and
/// log domains. Kept as documentation of the shared field set; the per-kind
/// arms above spell their fields out explicitly for auditability.
#[allow(dead_code)]
const TEMPORAL_PROVENANCE: &[&str] = &["valid_time"];

/// Whether a node kind is a deterministic code fact or an agent-authored
/// record (`docs/schema/producer-version.md` §10).
fn is_code_fact_kind(kind: &str) -> bool {
    matches!(
        kind,
        // codegraph: the 14 extractor-emitted kinds.
        "Repository" | "File" | "Module" | "Symbol" | "Import" | "Dependency"
            | "DependencyDeclaration" | "Commit" | "Change" | "PanicRiskSite"
            | "UnsafeSite" | "DebtMarker" | "ScanCoverage" | "HistoryReplayWindow"
        // semantic: measured facts, never agent prose.
        | "SemanticDrift" | "EmbeddingModel" | "EmbeddingVector"
        // log: deterministic log-signature observations.
        | "LogSource" | "ErrorSignature" | "LogEvent" | "LogOccurrenceBucket"
    )
}

/// Agent provenance required on agent-memory nodes
/// (`docs/schema/agent-memory.md` §3). Every agent-authored node MUST carry
/// these fields. The `Agent` identity node is the exception: its provenance
/// is carried at registration
/// (`docs/schema/agent-memory.md` §4a `Agent record shape`), and real `Agent`
/// records never set the per-observation fields, so requiring them would
/// reject conformant stores.
fn agent_memory_required_fields(kind: &str) -> &'static [&'static str] {
    match kind {
        "Agent" => &["agent_id", "agent_kind"],
        _ => &[
            "agent_id",
            "agent_kind",
            "session_id",
            "observed_at",
            "ingested_at",
        ],
    }
}

/// Extra required fields for a node contract, beyond the base record fields
/// (`id`, `record_type`, `kind`, `schema_version`, `summary`) and the producer
/// envelope.
fn node_required_fields(domain: &str, kind: &str) -> &'static [&'static str] {
    match (domain, kind) {
        // File nodes must identify their path; the negative tuple test
        // requires File to reject minimal records without a path.
        ("codegraph", "File") => &["repo_relative_path"],
        // Agent-memory nodes must carry agent provenance
        // (`docs/schema/agent-memory.md` §3, `docs/cli/schema.md`).
        ("agent_memory", kind) => agent_memory_required_fields(kind),
        // All other kinds: base record fields (id, kind, schema_version,
        // record_type, summary) are sufficient. Domain-specific fields are
        // validated via property constraints, not required fields, to avoid
        // rejecting legacy versions with different field sets.
        _ => &[],
    }
}

// ── Generation ──────────────────────────────────────────────────────────────
//
// The record shapes come from `schemars::schema_for!(GraphRecord)` — the same
// Rust types that serialize the records — and are then specialized per tuple.
// Nothing about a field's type is hand-written here; only the contract layer
// (required/forbidden fields, tuple pinning, trust documentation) is.

/// Schemars-generated object schemas for the three record shapes, each
/// carrying the shared `$defs`.
fn base_variant_schemas() -> &'static BTreeMap<RecordType, Value> {
    static BASE: OnceLock<BTreeMap<RecordType, Value>> = OnceLock::new();
    BASE.get_or_init(|| {
        let mut map = BTreeMap::new();
        let root: Value =
            serde_json::to_value(schemars::schema_for!(GraphRecord)).unwrap_or(Value::Null);
        let defs = root.get("$defs").cloned().unwrap_or(Value::Null);
        if let Some(variants) = root.get("oneOf").and_then(Value::as_array) {
            for variant in variants {
                let tag = variant
                    .get("properties")
                    .and_then(|p| p.get("record_type"))
                    .and_then(|t| t.get("const"))
                    .and_then(Value::as_str);
                let record_type = match tag {
                    Some("node") => RecordType::Node,
                    Some("edge") => RecordType::Edge,
                    Some("tombstone") => RecordType::Tombstone,
                    _ => continue,
                };
                let mut doc = variant.clone();
                if let Some(obj) = doc.as_object_mut() {
                    obj.insert("$defs".to_owned(), defs.clone());
                }
                map.insert(record_type, doc);
            }
        }
        map
    })
}

/// Emit the JSON Schema document for an addressed tuple, or `None` when the
/// tuple is not a known contract.
#[must_use]
pub fn schema_document(domain: &str, kind: &str, version: u32) -> Option<Value> {
    let spec = find_schema_ref(domain, kind, version)?;
    let base = base_variant_schemas().get(&spec.record_type)?;
    specialize(base, &spec)
}

/// Pretty-printed JSON of [`schema_document`].
#[must_use]
pub fn schema_json(domain: &str, kind: &str, version: u32) -> Option<String> {
    schema_document(domain, kind, version).and_then(|doc| serde_json::to_string_pretty(&doc).ok())
}

/// Extra required fields for edge contracts.
/// Expected `log_kind` tag (`snake_case`) for a log node kind. The log payload is
/// internally tagged, so the payload variant must agree with the node kind.
fn log_payload_kind(node_kind: &str) -> Option<&'static str> {
    match node_kind {
        "LogSource" => Some("log_source"),
        "ErrorSignature" => Some("error_signature"),
        "LogEvent" => Some("log_event"),
        "LogOccurrenceBucket" => Some("log_occurrence_bucket"),
        _ => None,
    }
}

fn edge_required_fields(domain: &str, label: &str) -> &'static [&'static str] {
    match (domain, label) {
        // Every `EMITTED_DURING` edge carries exactly one correlation basis
        // (issue #323).
        ("log", "EMITTED_DURING") => &["basis"],
        // `FRAME_RESOLVES_TO` edges always carry the resolution verdict and
        // the frame index (issue #322).
        ("log", "FRAME_RESOLVES_TO") => &["frame_resolution", "frame_index"],
        _ => &[],
    }
}

/// Fixed property values for a contract.
fn property_consts(domain: &str, kind: &str) -> &'static [(&'static str, &'static str)] {
    match (domain, kind) {
        ("agent_memory", "Retraction") => {
            &[("valid_time_source", "inferred_from_transaction_time")]
        }
        ("artifact", "PatchArtifact") => &[("valid_time_source", "produced_at")],
        _ => &[],
    }
}

/// The evidence-handle requirement for verification records
/// (`docs/schema/verification.md` §4b): every verification-domain node MUST
/// carry at least one evidence handle — a non-empty `source_artifact_hash`,
/// a non-empty `source_artifact_path`, or a `stdout_handle`/`stderr_handle`
/// whose `hash` is non-empty.
fn evidence_handle_constraint() -> Value {
    json!({"anyOf": [
        {
            "required": ["source_artifact_hash"],
            "properties": {"source_artifact_hash": {"type": "string", "minLength": 1}},
        },
        {
            "required": ["source_artifact_path"],
            "properties": {"source_artifact_path": {"type": "string", "minLength": 1}},
        },
        {
            "required": ["stdout_handle"],
            "properties": {"stdout_handle": {
                "type": "object",
                "required": ["hash"],
                "properties": {"hash": {"type": "string", "minLength": 1}},
            }},
        },
        {
            "required": ["stderr_handle"],
            "properties": {"stderr_handle": {
                "type": "object",
                "required": ["hash"],
                "properties": {"hash": {"type": "string", "minLength": 1}},
            }},
        },
    ]})
}

/// Evidence-handle constraint for verification nodes, with legacy exemption.
/// `docs/schema/verification.md` §4b requires evidence handles at ingestion
/// time for modern records; pre-envelope legacy records (no `producer`)
/// predate it and must still validate per `docs/schema/producer-version.md`
/// §6 ("Legacy records stay legacy").
fn verification_evidence_constraint() -> Value {
    json!({"anyOf": [
        {"not": {"required": ["producer"]}},
        evidence_handle_constraint(),
    ]})
}

fn title_for(spec: &SchemaRef) -> String {
    format!(
        "{} {} {} (schema v{})",
        spec.domain,
        spec.record_type.as_str(),
        spec.kind,
        spec.version
    )
}

fn description_for(spec: &SchemaRef) -> String {
    let mut desc = format!(
        "Persisted {} {} record of kind `{}` at schema version {}. ",
        spec.domain,
        spec.record_type.as_str(),
        spec.kind,
        spec.version
    );
    match spec.record_type {
        RecordType::Tombstone => {
            desc.push_str(
                "Deletion marker. Requires the deleted record's ID; the producer \
                 envelope is required at the domain's current version when that \
                 version postdates the envelope's introduction, and optional on \
                 legacy versions and pre-envelope current versions \
                 (docs/schema/producer-version.md §6).",
            );
        }
        RecordType::Edge => {
            desc.push_str(
                "Typed graph edge. The label, schema version, and record ID prefix \
                 are pinned to this contract; the producer envelope is required at \
                 the domain's current version when that version postdates the \
                 envelope's introduction, and optional on legacy versions and \
                 pre-envelope current versions (docs/schema/producer-version.md §6).",
            );
            if spec.domain == "codegraph" {
                desc.push_str(
                    " Deterministic code fact: edges may carry extraction confidence \
                     but never agent-authored prose or provenance.",
                );
            }
        }
        RecordType::Node => {
            if is_code_fact_kind(spec.kind) {
                desc.push_str(
                    "Deterministic code fact: derived from source bytes (or runtime \
                     log bytes) by a deterministic producer, never agent-authored. \
                     Subjective fields are forbidden: `text`, `agent_id`, \
                     `agent_kind`, `session_id`, `observed_at`, `confidence`, \
                     `source_handle`, `importer_id`, `importer_version`, \
                     `source_artifact_path`, `source_artifact_hash`, \
                     `evidence_links`.",
                );
            } else {
                match spec.domain {
                    "semantic" => desc.push_str(
                        "Derived measurement pinned to an embedding model and \
                         selection threshold \
                         (docs/schema/semantic-drift.md §2): requires `domain`, \
                         `valid_time`, `valid_time_source`, `ingested_at`.",
                    ),
                    _ => {
                        desc.push_str("Agent-authored record: agent provenance is required. ");
                    }
                }
                match spec.domain {
                    "agent_memory" => desc.push_str(
                        "Requires `agent_id`, `agent_kind`, `session_id`, \
                         `observed_at`, `ingested_at` (docs/schema/agent-memory.md §3).",
                    ),
                    "verification" => desc.push_str(
                        "Requires `domain` plus at least one non-empty evidence \
                         handle (docs/schema/verification.md §4b).",
                    ),
                    "artifact" => desc.push_str(
                        "Requires the patch lifecycle identity \
                         (docs/schema/agent-actions.md §3).",
                    ),
                    "project" => desc.push_str(
                        "Requires entity and temporal provenance \
                         (docs/schema/project-graph.md §3).",
                    ),
                    "user_context" => desc.push_str(
                        "Requires domain and temporal provenance \
                         (docs/schema/user-context.md).",
                    ),
                    _ => {}
                }
            }
            desc.push_str(
                " The producer envelope (`producer`) is required at the domain's \
                 current version when that version postdates the envelope's \
                 introduction, and optional on legacy versions and pre-envelope \
                 current versions (docs/schema/producer-version.md §6).",
            );
        }
    }
    desc
}

/// Set draft, identity, and documentation fields on a schema document.
fn set_identity(obj: &mut serde_json::Map<String, Value>, spec: &SchemaRef) {
    obj.insert("$schema".to_owned(), json!(JSON_SCHEMA_DRAFT));
    obj.insert("$id".to_owned(), json!(spec.id()));
    obj.insert("title".to_owned(), json!(title_for(spec)));
    obj.insert("description".to_owned(), json!(description_for(spec)));
}

fn specialize(base: &Value, spec: &SchemaRef) -> Option<Value> {
    let mut doc = base.clone();
    let obj = doc.as_object_mut()?;
    set_identity(obj, spec);

    let props = obj.get_mut("properties")?.as_object_mut()?;

    // Pin kind/label, schema version, and the record ID prefix to the tuple.
    match spec.record_type {
        RecordType::Node => {
            props.insert("kind".to_owned(), json!({"const": spec.kind}));
            // `domain` is value-constrained when present; domains whose
            // contracts require it list it in `required` below.
            props.insert(
                "domain".to_owned(),
                json!({"anyOf": [{"const": spec.domain}, {"type": "null"}]}),
            );
        }
        RecordType::Edge => {
            props.insert("label".to_owned(), json!({"const": spec.kind}));
        }
        RecordType::Tombstone => {}
    }
    props.insert(
        "schema_version".to_owned(),
        json!({"const": spec.version, "type": "integer"}),
    );
    props.insert(
        "id".to_owned(),
        json!({
            "type": "string",
            "pattern": format!("^{}:v{}:", spec.domain, spec.version),
        }),
    );
    for (name, value) in property_consts(spec.domain, spec.kind) {
        props.insert((*name).to_owned(), json!({"const": value}));
    }

    // Required: base fields, kind extras, and the producer envelope at the
    // current version — but only when that version postdates the envelope's
    // introduction (per `docs/schema/producer-version.md` §6, legacy records
    // stay legacy, so `producer` stays optional there).
    let mut required: Vec<String> = obj
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let extras: &[&str] = match spec.record_type {
        RecordType::Node => node_required_fields(spec.domain, spec.kind),
        RecordType::Edge => edge_required_fields(spec.domain, spec.kind),
        RecordType::Tombstone => &[],
    };
    for field in extras {
        if !required.iter().any(|f| f == field) {
            required.push((*field).to_owned());
        }
    }
    let envelope_introduced = producer_envelope_introduced_at(spec.domain);
    if Some(spec.version) == current_version(spec.domain)
        && spec.version > envelope_introduced
        && !required.iter().any(|f| f == "producer")
    {
        required.push("producer".to_owned());
    }
    // `domain` is a logical grouping derived from the record ID prefix, not a
    // persisted `GraphRecord` field — no record carries it, so it must never
    // be required. The property definition above constrains it when present.
    required.retain(|f| f != "domain");
    obj.insert(
        "required".to_owned(),
        Value::Array(required.into_iter().map(Value::String).collect()),
    );

    // Trust separation: deterministic code facts forbid subjective fields.
    if spec.record_type == RecordType::Node && is_code_fact_kind(spec.kind) {
        let branches: Vec<Value> = CODE_FACT_FORBIDDEN
            .iter()
            .map(|field| json!({"required": [field]}))
            .collect();
        obj.insert("not".to_owned(), json!({"anyOf": branches}));
    }

    // Cross-field constraints.
    let mut all_of: Vec<Value> = Vec::new();
    // Log payload variant must agree with the node kind (the payload is
    // internally tagged on `log_kind`).
    if spec.domain == "log"
        && spec.record_type == RecordType::Node
        && let Some(tag) = log_payload_kind(spec.kind)
    {
        all_of.push(json!({
            "properties": {
                "log": {
                    "type": "object",
                    "required": ["log_kind"],
                    "properties": {"log_kind": {"const": tag}},
                }
            }
        }));
    }
    if spec.domain == "verification" && spec.record_type == RecordType::Node {
        all_of.push(verification_evidence_constraint());
    }
    if spec.kind == "Failure" && spec.record_type == RecordType::Node {
        // `docs/schema/agent-memory.md` §4a: importer provenance XOR the
        // daemon-write provenance set. (`domain` is not a persisted field;
        // the importer set is identified by the other four.)
        all_of.push(json!({"anyOf": [
            {"required": ["importer_id", "importer_version", "source_artifact_path", "source_artifact_hash"]},
            {"required": ["agent_id", "agent_kind", "session_id", "observed_at", "source_handle"]},
        ]}));
    }
    if !all_of.is_empty() {
        obj.insert("allOf".to_owned(), Value::Array(all_of));
    }

    Some(doc)
}
