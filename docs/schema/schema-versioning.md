# Record Schema Versioning

**Status:** Active. This document is the single source of truth for Egregore
record-level `schema_version` semantics.

**Applies to:** persisted `GraphRecord` JSONL records and the same record shapes
stored in embedded AletheiaDB.

**Does not apply to:** the daemon HTTP envelope `api_version`. The daemon wire
contract is versioned separately in [`daemon-api.md`](daemon-api.md). A daemon
response may be `"api_version": "v1"` while the records inside its `payload` or
`records` array contain several record `schema_version` values.

## 1 - Version Tuple

Readers interpret `schema_version` as scoped to:

```text
(domain, record_kind, schema_version)
```

Examples:

```text
(codegraph, Symbol, 7)
(project, Task, 1)
(agent_memory, ToolCall, 1)
(semantic, SemanticDrift, 1)
```

`schema_version` is **not global**. The current `SCHEMA_VERSION` constant in
`src/ir.rs` is the code-graph-domain default used by current scan, history, and
incremental producers. New domains use their own constants:

| Domain | Rust constant | Scope |
|--------|---------------|-------|
| `codegraph` | `SCHEMA_VERSION` | code-graph `Node`, `Edge`, and `Tombstone` records |
| `agent_memory` | `AGENT_MEMORY_SCHEMA_VERSION` | agent-memory records |
| `verification` | `VERIFICATION_SCHEMA_VERSION` | verification records |
| `artifact` | `ARTIFACT_SCHEMA_VERSION` | artifact records |
| `project` | `PROJECT_SCHEMA_VERSION` | project-graph records |
| `semantic` | `SEMANTIC_SCHEMA_VERSION` | semantic-drift records |
| `log` | `LOG_SCHEMA_VERSION` | log-signature records (`LogSource`, `ErrorSignature`, `LogEvent`, `LogOccurrenceBucket`) — version 3 |
| `control_catalog` | `CONTROL_CATALOG_SCHEMA_VERSION` | the SOC2 control→evidence-class catalog **document** (`ControlCatalog`, issue #337) — version 1. A standalone document contract, **not** a `GraphRecord` domain: it is recognized by `is_known_control_catalog_schema_version` in `src/schema_version.rs`, deliberately outside `is_known_record_version`, because the catalog is never ingested as a record and adding a `control_catalog` arm to the `GraphRecord` reader path would corrupt it. |

The `log` domain was bumped 1 → 2 by issue #361 (source-aware
`LogOccurrenceBucket` identity: the owning `LogSource` is folded into the
bucket's stable ID and carried as a required `source_id` payload field), then
2 → 3 by issues #362 / #364: **#362** persists repository attribution as a
retrievable `repository_id` field on all four log payloads (so
`eg query log-deltas --repo` can filter log signatures), and **#364** adds a
sorted `occurrence_timestamps` list to `LogOccurrenceBucket` (so per-window
occurrence counts are endpoint-exact). Both v3 fields are `#[serde(default)]`,
so a legacy `log:v2:` record still deserializes and degrades honestly — a
deliberate divergence from #361's required-field stance. Each bump is
`breaking` — a bucket's stable ID prefix differs across versions, so old readers
reject cross-version mismatches with `unknown_schema_version` rather than
silently coercing. No `migrating` one-way transform manifest is provided because
the remedy is a **re-scan / re-ingest**: `eg scan-logs` regenerates every log
record under the current identity deterministically from the source log.

The `codegraph` domain (`SCHEMA_VERSION`) was bumped 5 → 6 by issue #135 (the
`ScanCoverage` node), then 6 → 7 by issue #443: the `CONSTRUCTS` struct-literal
edge label (constructing Symbol → constructed type's definition Symbol) plus an
optional `is_exhaustive` marker field on edge records (the E0063 exhaustiveness
of the collapsed construction sites). Both additions are additive; the optional
`is_exhaustive` field is `#[serde(default, skip_serializing_if)]` so a legacy
edge record without it still deserializes. It was then bumped 7 → 8 by issue
#445: the `REGISTERS_ROUTE` route-registration edge label (registration-site
Symbol → registered handler Symbol) plus an optional `route` field on `Symbol`
nodes (`Vec<RouteAnnotation>` of method + path, captured from routing attributes
such as `#[get("/path")]`). Both additions are additive; the optional `route`
field is `#[serde(default, skip_serializing_if = "Option::is_none")]` so a legacy
node record without it still deserializes, and is never an identity input. The
paired incremental `CACHE_SCHEMA_VERSION` bumped 24 → 25 for the new
`FileFacts.route_registration_sites` cached fact shape. Re-extraction from source
regenerates every codegraph record under the current version deterministically.

It was then bumped 8 → 9 by issue #117: the optional `crate_attribution` field
on every path-bearing code-graph node (`File`, `Module`, `Symbol`, `Import`,
`Diagnostic`, `PanicRiskSite`, `DebtMarker`, `UnsafeSite`,
`DependencyDeclaration`, `Change`), carrying the owning Cargo package's name and the
repo-relative path of the owning `Cargo.toml`, or a closed-set reason no package
owns the node. The addition is `additive`: the field is
`#[serde(default, skip_serializing_if = "Option::is_none")]`, so a legacy v8 node
record with no `crate_attribution` key still deserializes (to `None`, meaning
attribution UNKNOWN — never "provably unowned", which is the distinct present
`status: unattributed` value), and it is **never an identity input**, so
`stable_id`'s preimage is unchanged. The paired incremental
`CACHE_SCHEMA_VERSION` bumped 25 → 26: the `SCHEMA_VERSION` bump changes every
`codegraph:v<N>:` record-ID prefix, so a cache holding v8 IDs would replay
records whose endpoints no longer match freshly-minted v9 ones. Attribution
itself is deliberately NOT cached — it is recomputed on every refresh, so a
source file byte-identical to its cached version whose owning `Cargo.toml` was
renamed, added, or deleted is still re-attributed. Re-extraction from source
regenerates every codegraph record under the current version deterministically.

Rationale: per-domain and per-kind scoping lets #6, #11, #13, #14, and #15 land
independently. A new project `Task` shape must not force a version bump for
unrelated codegraph `Symbol` records in the same store. For the code-graph
domain, `SCHEMA_VERSION` remains the domain-wide default until a future slice
splits individual codegraph record kinds.

## 2 - Compatibility Classes

Every producer change to a record kind must declare one of these classes before
it writes records.

| Class | Examples | Producer rule | Reader rule |
|-------|----------|---------------|-------------|
| `additive` | New optional field, new reserved enum value, new optional edge label | May keep the current version when old readers can safely ignore it. May bump the version when operators need visible rollout tracking. | Old readers MUST accept by ignoring unknown fields or reserved variants. New readers MUST tolerate the field being absent as `None`. |
| `breaking` | Field rename, semantic change to an existing field, removed required field, narrowed value domain | MUST bump `schema_version` for the affected `(domain, kind)`. | Old readers MUST reject with `unknown_schema_version`. New readers MUST NOT silently coerce old records into the new shape. |
| `migrating` | Breaking change plus a declared one-way transform from the prior version | MUST bump `schema_version` and add a migration manifest before writing new records. | New readers MAY transparently up-convert only when the transform is declared. No silent migrations. |

Adding #3's top-level `domain` field is additive only if old readers can ignore
the field and new readers treat missing `domain` as the legacy inferred domain.
If #3 changes stable IDs or field meaning at the same time, that specific change
must be classified as `breaking` or `migrating`.

## 3 - Reader Contract

All reader paths must form and check a `RecordVersion` before accepting a record
as a current `GraphRecord`. The centralized Rust entry points are:

- `src/schema_version.rs::read_record_line` for JSONL line parsing.
- `src/schema_version.rs::validate_record_version` for already-deserialized
  records entering adapters, daemon writes, or read-back paths.
- `src/schema_version.rs::record_version` for inspect and operator summaries.

Current callsites that must stay wired through those helpers:

| Callsite | Required route |
|----------|----------------|
| `src/adapters/mod.rs::records_from_jsonl` | `read_record_line` rejects unknown versions before returning records |
| `src/adapters/mod.rs::records_from_jsonl_report` | `read_record_line` records unknown-version counts for inspect |
| `src/adapters/mod.rs::ingest_records` | `validate_record_version` gates all adapter writes |
| `src/adapters/aletheiadb.rs::write_record` | `validate_record_version` gates embedded writes |
| `src/adapters/aletheiadb.rs::{read_node_record,read_edge_record,read_tombstone_record}` | reconstructed records are validated before read-back returns |
| `src/daemon.rs::apply_write` | daemon ingest rejects unknown versions before domain-specific validation |
| `src/cli.rs::inspect` | report mode surfaces recognized and unknown tuples |
| `src/cli.rs::load_records_from_jsonl` | default query/ingest JSONL loading rejects unknown versions |
| `src/cli/audit.rs::evidence_pack_assemble_cmd` | evidence-pack (#338) reads records via `load_query_records`, which routes JSONL through `read_record_line` |
| `src/incremental.rs::scan_repository_incremental_at` | cache records are validated before reuse; incompatible cache schema falls back to rebuild |

The list is intentionally grep-able. New reader surfaces must update this table
and route through the same helper.

## 4 - Unknown-Version Policy

Default mode rejects any unknown tuple with typed error code:

```text
unknown_schema_version
```

The rejection must surface the full tuple:

```json
{
  "code": "unknown_schema_version",
  "version": {
    "domain": "codegraph",
    "kind": "Symbol",
    "version": 5
  }
}
```

The daemon uses the same code in its v1 HTTP error taxonomy. Future tolerant read
mode may skip-and-log unknown records, but it is reserved and disabled by
default. Silent skipping on a graph substrate creates phantom missing-data bugs
that look like model failures.

## 5 - Mixed-Store Reads

Readers may encounter a result set containing multiple versions of the same
record kind. This happens naturally with bi-temporal `as_of` reads and with
stores that outlive binary upgrades.

Rules:

- Every returned record MUST keep its `schema_version` field visible to
  consumers. It is a stable consumer-visible field, not an implementation detail.
- Query and inspect surfaces MUST NOT collapse records across versions without
  retaining the per-record `schema_version`.
- `eg inspect` MUST print per-`(domain, kind, version)` counts for recognized
  records and separate `unknown_schema_version` counts for unknown tuples.

Example inspect lines:

```text
schema_version codegraph Symbol v5: 12
schema_version codegraph Symbol v4: 2
unknown_schema_version codegraph Symbol v6: 1
```

## 6 - Migration Manifests

Every `migrating` bump requires a manifest at:

```text
docs/schema/migrations/<domain>/<kind>/v<from>-to-v<to>.md
```

Each manifest must include:

- the breaking field or semantic change
- the one-way transform
- whether the transform is lossless
- whether re-ingestion from source is preferred instead
- fixture names proving the old and new shapes

Template:

```markdown
# <domain>.<kind> v<from> to v<to>

## Change

## Transform

## Losslessness

## Preferred Operator Path

## Fixtures
```

For `codegraph`, re-extraction from source is always a valid alternative to
in-place migration because codegraph records are deterministic over repository
state and producer version. Agent-memory, project, artifact, verification, and
semantic records do not have that escape hatch by default; their migrations must
treat the persisted record as source data.

## 7 - Producer Envelope

The `producer` field is a **non-versioned envelope field** on every `GraphRecord`. It does not participate in the per-`(domain, kind)` `schema_version` bump cycle. Adding a new `producer_kind` is additive; changing a `producer_components` key shape is breaking and requires a `PRODUCER_ENVELOPE_SCHEMA_VERSION` bump (separate constant in `src/ir.rs`).

See [`docs/schema/producer-version.md`](producer-version.md) for the full `Producer` shape, the non-identity rule, the legacy-record policy, and per-kind `producer_components` requirements.

## 8 - Coordination Notes

- #3: the `domain` field bump from `SCHEMA_VERSION` 1 to 2 must classify itself
  under `additive`, `breaking`, or `migrating` and update this document.
- #5: the daemon HTTP error-code taxonomy reserves `unknown_schema_version`.
- #6, #11, #13, #14, #15: each new record-kind spec must declare its initial
  `(domain, kind)` `schema_version` and compatibility class up front; future
  bumps inherit this policy.
- #8: bi-temporal `as_of` reads must surface each returned record's
  `schema_version` because historical reads can return older versions by
  construction.
- #337: introduces the `(control_catalog, ControlCatalog, 1)` tuple — a
  **deliberate exception to the no-new-vocabulary norm**, called out here per
  that norm. It is a versioned *document* contract (the SOC2 control→
  evidence-class catalog, `docs/controls/soc2-v1.json`), not graph vocabulary:
  the slice adds **no** new graph domain, node kind, edge label, trust class,
  importer, network access, or LLM-generated content. The tuple follows the
  standard reader contract (`unknown_schema_version` rejection for future
  versions) via `is_known_control_catalog_schema_version`, kept off the
  `GraphRecord` reader path (see the §1 table row and
  `docs/controls/README.md`). Future bumps are `breaking` by default: a pack
  built under one catalog version is comparable to another iff their
  `control_catalog:v1:<hex>` hash pins match.

## 9 - Conformance Fixtures

Executable fixtures live in `tests/integration/schema_versioning.rs`.

- `future_schema_version_is_typed_and_inspect_reports_mixed_counts` builds JSONL
  containing one known `codegraph.Symbol` record and one unknown future
  `codegraph.Symbol` record. The default reader rejects the future record with
  `unknown_schema_version`; report mode still parses the known record and
  `eg inspect` prints separate version counts.
- `additive_unknown_field_parses_and_inspects_without_warning` adds an unknown
  optional field to a current record. The reader accepts it, inspect succeeds,
  and stderr stays empty.

## 10 - Declared Store Constraints and the Bump Rule

`AletheiaDB` 0.2.0 ships opt-in, per-label schema constraints enforced at the
pre-apply commit hook. `eg audit schema-constraints --declare` (issue #486) can
declare them on an embedded store as a commit-time backstop for this contract.
Doing so imposes one — and only one — additional obligation on a schema bump.

**The rule.** A `schema_version` bump is unaffected by a declared store, but a
change to the adapter's universal `base_properties` keys is not. Concretely:

| change | effect on a declared store | required action |
|---|---|---|
| bump a per-domain `SCHEMA_VERSION` | **none** — the bump changes the `schema_version` VALUE, and the constraint declares its TYPE (`Integer`) | none |
| add a `NodeKind` (and therefore a new store-side node label) | **none** — the new label carries no declaration and is fully schemaless upstream | none for correctness, but the backstop silently NARROWS: the new label is unconstrained until `--declare` is re-run |
| add an `EdgeLabel` | **none**, same reason | none |
| add a new optional per-kind payload field | **none** — no profile declares a per-kind payload field, by construction and by test | none |
| **rename or remove a `base_properties` key** (`codegraph_id`, `record_type`, `schema_version`, `summary`) | **breaking** — every write of every label aborts at the commit hook | `eg audit schema-constraints --drop` on every declared store **before** shipping the change; re-declare after |
| **change a `base_properties` key's stored type** (e.g. writing `schema_version` as a string) | **breaking**, same reason | as above |
| rename or remove an edge routing key (`label`, `source_codegraph_id`, `target_codegraph_id`, `egregore_seq`) | **breaking** for edge writes | as above |

The first four rows are why the declared profiles constrain **only** the
universal identity/routing spine and never a per-kind payload field: a
constraint that fires on a legitimate future write is worse than no constraint,
because the whole transaction aborts with zero partial application. A unit test
(`schema_constraints::tests::no_profile_declares_a_per_kind_payload_field`)
prevents a profile from growing a payload-field constraint by accident, and
`schema_version_is_declared_integer_in_every_profile` pins the type that makes
row 1 safe.

Declaration is **opt-in and reversible**: the default `eg audit
schema-constraints` action is a read-only report that declares nothing, and
`--drop` retracts. Nothing in Egregore's write path declares constraints
automatically, so an undeclared store — the default — is entirely unaffected by
this section.

Two upstream properties bound how much a declared constraint can be relied on,
and neither is a defect to be fixed here:

* **Durability rides on index persistence.** The `schema_constraints.dat`
  sidecar is written only when `persistence.enabled` (which
  `durable_config_for_data_dir` sets, so it holds for every Egregore store) — but
  a store opened without it keeps declarations in memory only.
* **A corrupt sidecar is quarantined, not fatal.** Upstream renames it aside and
  starts with **no** constraints rather than bricking startup, so enforcement can
  silently disappear.

Together these make a declared constraint a **backstop that catches mistakes**,
never a proof obligation the rest of the system may lean on. `eg validate`, the
adapter's read-back verification, and the citation audits remain the primary
enforcement of this contract.

See [`docs/cli/schema-constraints.md`](../cli/schema-constraints.md) for the
full Phase 1 findings, the profile definitions, and the output contract.
