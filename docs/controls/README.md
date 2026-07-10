# Control Catalog (SOC2) — evidence-class vocabulary and document format

Issue #337. A **control catalog** is a versioned, deterministic document that
maps a compliance control ID (e.g. a SOC2 Common Criteria control such as
`CC8.1`) to the set of Egregore **evidence classes** that can substantiate the
process behind that control. The catalog is a plain JSON document; it is loaded,
validated, and hash-pinned by the pure module `src/evidence_pack.rs` and surfaced
by `eg audit control-catalog` (see `docs/cli/control-catalog.md`).

## Epistemic boundary

A catalog entry maps a control ID to Egregore evidence classes; it is **not** an
interpretation of the AICPA Trust Services Criteria, **not** legal advice, and
inclusion of a control ID asserts nothing about an organization's compliance
obligations or the effectiveness of its controls. Evidence of process
execution — never proof of control effectiveness or compliance.

## Evidence-class vocabulary (closed, 11 values)

The evidence-class set is **closed**. A catalog naming any class outside this set
is rejected at load time with `unknown_evidence_class`. Wire names are stable
snake_case identifiers.

| Class | Meaning |
| --- | --- |
| `commits` | Git commit records. |
| `pull_requests` | Pull-request records. |
| `reviews` | Code-review records. |
| `review_coverage` | Review-coverage measurement over the changed surface. |
| `structural_deltas` | Structural (symbol/file) deltas across a change. |
| `public_api_deltas` | Public-API surface deltas across a change. |
| `validation_runs` | Validation-run records (e.g. `eg validate`). |
| `verification_evidence` | Verification evidence (command runs, test runs, CI status). |
| `error_signatures` | Error-signature records. |
| `occurrence_buckets` | Occurrence-bucket aggregates over error signatures. |
| `remediation_links` | Links from an incident to its remediation. |

## Requirement levels (closed, 2 values)

Each control lists its evidence classes with a `requirement`:

- `required` — the class must be present or the control gate fails.
- `optional` — the class is reported when present; its absence never fails the gate.

## Catalog document format

```json
{
  "catalog_id": "soc2-v1",
  "schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
  "controls": [
    {
      "control_id": "CC8.1",
      "title": "…",
      "evidence_classes": [
        { "class": "commits", "requirement": "required" }
      ]
    }
  ]
}
```

- `catalog_id` — stable catalog identifier (e.g. `soc2-v1`).
- `schema_version` — the `(domain, kind, version)` tuple. For this document it
  must be exactly `(control_catalog, ControlCatalog, 1)`.
- `controls[]` — each with a `control_id`, human-readable `title`, and an
  `evidence_classes[]` list of `{ class, requirement }` entries.

The embedded default catalog is `docs/controls/soc2-v1.json`.

## Versioning and the `unknown_schema_version` reader contract

The tuple `(control_catalog, ControlCatalog, 1)` is registered in
`src/schema_version.rs` (via `is_known_control_catalog_schema_version`), kept
separate from the `GraphRecord` reader path because the catalog is a standalone
document, not a graph record. A document whose tuple is anything other than the
recognized version is rejected with a machine-readable envelope matching the
repo-wide shape:

```json
{ "code": "unknown_schema_version", "version": { "domain": "…", "kind": "…", "version": 2 } }
```

A future `soc2-v2` catalog would carry a new `schema_version.version` and be
recognized by a widened recognizer; readers pinned to v1 reject it rather than
silently misreading it.

## BLAKE3 canonical hash-pin contract

Every catalog has a deterministic content hash. The **canonical form** is:

- object keys in fixed field order,
- `controls` sorted by `control_id`,
- each control's `evidence_classes` sorted by class wire name.

Canonicalization is independent of the input's control/class ordering and of
serde_json's `preserve_order` feature, so the hash is stable across runs and
across equivalent re-orderings of the source document. The hash is a BLAKE3
digest over the canonical bytes, formatted as a handle:

```
control_catalog:v1:<hex>
```

mirroring the code-graph `stable_id` handle shape. This hash goes into every
future evidence-pack manifest (issue #338) so an assembled pack is tied to the
exact catalog version it was built against.

## Three-way requirement semantics

When an evidence pack is later assembled (issue #338), each class requirement is
evaluated against whether evidence of that class was found:

| Requirement | Availability | Outcome | Gate |
| --- | --- | --- | --- |
| `required` | present | `Pass` | pass |
| `required` | unavailable | `GateFail` | **fail** |
| `optional` | unavailable | `ReportedOptionalUnavailable` | pass |
| `optional` | present | `Pass` | pass |

Only a `required` class that is unavailable fails the gate; an unavailable
`optional` class is reported but never fails it.

## Why CC7.2 / CC7.3 are optional-only in v1

CC8.1 (change management) is fully supported today: commits, pull requests,
reviews, and review coverage are all first-class Egregore evidence. CC7.2
(system monitoring) and CC7.3 (incident evaluation and response) depend on a
log/error-graph domain (the umbrella issue #319) that is **not yet implemented**;
it is a soft dependency. Until that domain lands, every CC7.2/CC7.3 evidence
class is `optional` so their absence never fails a gate. A future `soc2-v2`
flips the relevant classes to `required` once the log domain exists.
