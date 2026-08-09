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

This set is closed too: a requirement outside `{required, optional}` is
rejected at load time with `invalid_requirement` (naming the control, class,
and offending value), never silently coerced.

## Vocabulary boundary (what this slice adds — and doesn't)

This slice introduces one new **document contract** and its
`(control_catalog, ControlCatalog, 1)` schema tuple — a deliberate, called-out
exception to the repo's no-new-vocabulary norm, registered in
`docs/schema/schema-versioning.md` (§1 table and the #337 Coordination Note).
It adds **no** new graph domain, node kind, edge label, trust class, importer,
network access, or LLM-generated content: control titles are quoted
descriptors, not generated prose, and the catalog is never ingested into the
graph as records — it is a document contract like the redaction policy.

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

### Uniqueness (schema / hash-normalization contract)

Two uniqueness rules keep the canonical hash order-independent (see the hash-pin
contract below):

- A `control_id` must be unique across the catalog. A repeated `control_id` is
  rejected at load time with `duplicate_control_id`.
- Within one control, an evidence class must appear at most once. Listing the
  same `class` twice — even with different `requirement` values (e.g. `commits`
  required and `commits` optional) — is rejected with `duplicate_evidence_class`.

Both are hard parse errors (exit 2, first offender in document order). They
exist because duplicate control IDs or duplicate classes would tie under the
canonical sort, so two catalogs differing only in the order of the duplicate
entries could otherwise hash differently, violating the order-independence
guarantee.

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

A future `soc2-v2` **catalog** is a content revision: it carries a new
`catalog_id` (and therefore a new hash pin) under the *same* document format.
The `schema_version.version` bumps only when the document **format** itself
changes; such a future-format catalog would be recognized by a widened
recognizer, and readers pinned to v1 reject it rather than silently misreading
it.

The parser gates on this tuple **first**: it probes only the `schema_version`
tuple leniently and returns `unknown_schema_version` for any unsupported tuple
before enforcing the strict v1 body shape. So an unsupported/newer catalog that
also adds or renames fields is still reported as `unknown_schema_version`, not
`malformed_json` — the version signal is never masked by the strict-shape check.
Only supported-version documents are then held to the strict v1 shape (where a
stray or wrong-type field is `malformed_json`). The `malformed_json` envelope is
redaction-safe: it carries a stable code plus a value-free failure location
(`{ "code": "malformed_json", "line": <n>, "column": <n>, "category": "…" }`) and
never echoes catalog field values. A wrong-type field would otherwise leak its
value through `serde_json`'s raw message, so that message is deliberately dropped.

## BLAKE3 canonical hash-pin contract

Every catalog has a deterministic content hash. The **canonical form** is:

- object keys in fixed field order,
- `controls` sorted by `control_id` (unique, so this is a total order),
- each control's `evidence_classes` sorted by `(class wire name, requirement)`.

Because duplicate control IDs and duplicate classes within a control are
rejected at load time (see Uniqueness above), the sort keys are unique and the
ordering is a genuine total order — canonical bytes never depend on input order.
Canonicalization is independent of the input's control/class ordering and of
serde_json's `preserve_order` feature, so the hash is stable across runs and
across equivalent re-orderings of the source document. The hash is a BLAKE3
digest over the canonical bytes, formatted as a handle:

```
control_catalog:v1:<hex>
```

mirroring the code-graph `stable_id` handle shape. This hash goes into every
evidence-pack manifest (issue #338, shipped as `eg audit evidence-pack
assemble`'s `catalog_pin`) so an assembled pack is tied to the exact catalog
content it was built against.

The pin is **content-addressed over the canonical (sorted) form**, so it is
deliberately order-independent: two catalog files that differ only in the
declaration order of controls or classes carry the same pin. Pack **section
order**, by contrast, follows the catalog's declaration order. Same pin
therefore means same catalog *content*, not byte-identical *packs* — comparing
two packs still means comparing their contents, with the pin guaranteeing the
control→class mapping behind them is identical.

## Three-way requirement semantics

When an evidence pack is assembled (issue #338, `eg audit evidence-pack
assemble`), each class requirement is evaluated against whether evidence of
that class was found:

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
(system monitoring) and CC7.3 (incident evaluation and response) depend on the
log/error-graph domain (the umbrella issue #319) — a soft dependency when this
catalog shipped, and one that has since landed: `eg scan-logs` mints
`ErrorSignature`/`LogOccurrenceBucket` records and issue #340 folds them into
the CC7.x pack sections. The CC7.x classes nonetheless **stay `optional` in
`soc2-v1`**, because flipping a class to `required` is a gate-breaking catalog
change: it would turn every existing pack assembly over a store without log
records from a pass into a gate failure under the *same* catalog identity. The
flip to `required` is therefore deferred to a future `soc2-v2`, whose new
`catalog_id`/hash pin makes the stricter gate visible in every pack built
under it.
