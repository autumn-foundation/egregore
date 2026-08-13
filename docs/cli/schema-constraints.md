# `eg audit schema-constraints`

Evaluate — and optionally declare — `AletheiaDB` 0.2.0 schema constraints as a
**commit-time backstop** for the Egregore schema contract (issue #486).

```powershell
# Phase 1 - read-only report (the default action)
cargo run -- audit schema-constraints --data-dir .egregore
cargo run -- audit schema-constraints --data-dir .egregore --profile full-base
cargo run -- audit schema-constraints --data-dir .egregore --format text

# Phase 2 - opt-in declaration, and its retraction
cargo run -- audit schema-constraints --data-dir .egregore --declare
cargo run -- audit schema-constraints --data-dir .egregore --drop
```

## Why this exists

`eg validate` (issue #103) is a **pre-ingest gate over a JSONL file**. It asserts
reference closure between `scan` and `ingest`, and it is the right tool for that
job — but it cannot see a bad record that reaches an embedded store by any other
path: a foreign writer sharing the data dir, an adapter regression, a
hand-edited store. The adapter's own invariants live only as adapter code, which
is precisely the wrong place for a backstop: code that is wrong cannot catch
itself being wrong.

The two halves of this command address two different halves of that problem, and
it is worth being precise about which does what. **Enforcement** (`--declare`)
sits at the pre-apply commit hook, so it catches a bad write *performed through
the engine* — a foreign writer, an adapter regression. It cannot catch a store
edited outside the engine, because no transaction ever runs. **The report** is
what catches that case, by reading what is actually there.

`AletheiaDB` 0.2.0 ships opt-in, per-label schema constraints enforced at the
**pre-apply commit hook**. A violation aborts the whole transaction with zero
partial application. That puts a check at the write boundary itself.

This command is how you evaluate that option against real data, and how you turn
it on if you decide to.

## Phase 1 findings

The issue's blocking question was whether the store-side label partition is
coarse enough to make constraints near-trivial. It is not.

### 1 - What labels does the adapter actually write?

`adapters::aletheiadb::node_label(kind)` returns `kind.as_str()` for **every**
`NodeKind`, so the store-side node-label partition is exactly the `NodeKind`
partition — **one label per kind**, not one shared label. `Symbol` and `Task` do
**not** collide. Tombstone records are written under the additional literal label
`Tombstone`. Edges are created with `create_edge(.., label.as_str(), ..)`, so
each `EdgeLabel` is its own store-side edge type.

| | count | source |
|---|---|---|
| node labels | `NodeKind::ALL.len() + 1` | `NodeKind::ALL` + `Tombstone` |
| edge types | `EdgeLabel::ALL.len()` | `EdgeLabel::ALL` |

The report emits the full sorted list under `inventory.node_labels` /
`inventory.edge_types`, and states the partition finding machine-readably as
`inventory.label_partition: "one_label_per_node_kind"`.

The inventory is **derived, never hand-maintained**, and pinned by three tests
that each use an oracle independent of the thing under test:

* `node_kind_all_matches_the_enum_definition` / `edge_label_all_matches_the_enum_definition`
  recover the enum's true variant list from `serde`'s unknown-variant error —
  which the derive macro regenerates from the enum definition itself — and
  compare it to `ALL`. A guard that merely iterated `ALL` and asserted membership
  in `ALL` would be circular and could not fail; this one does.
* `node_label_is_exactly_the_kind_string_for_every_kind` pins the remaining link
  at the write site: that the adapter really does write `kind.as_str()` as the
  store-side label. Without it, a future variant given its own `node_label` arm
  returning a different literal would silently desync the inventory from what is
  written.

### 2 - Which properties are genuinely universal?

`base_properties(id, record_type, schema_version, summary)` runs on **every**
node, **every** tombstone, and **every** edge, and inserts four keys
unconditionally — there is no code path that writes a record without them:

| property | type | notes |
|---|---|---|
| `codegraph_id` | String | the stable Egregore record handle; already read back and verified at write time |
| `record_type` | String | `node` / `tombstone` / `edge`; the read path's first dispatch |
| `schema_version` | Int | the per-domain version integer |
| `summary` | String | a display string; always present, possibly empty |

Edges are written by `write_edge`, which adds four more routing keys
unconditionally (note that `egregore_seq` is written on nodes and tombstones too;
only the *declaration* of it is edge-scoped, for the reason below):

| property | type | notes |
|---|---|---|
| `label` | String | the edge label duplicated into properties |
| `source_codegraph_id` | String | endpoint handle the read path resolves |
| `target_codegraph_id` | String | endpoint handle the read path resolves |
| `egregore_seq` | **String** | the write-sequence **number**, inserted as `seq.to_string()` |

`egregore_seq` is a concrete trap the audit exists to surface: it *reads* like an
integer and is *stored* as a string. Declaring it `DeclaredType::Integer` would
make **every** future edge write fail. The `full-base` profile therefore declares
it `String`, and a unit test pins that so nobody "fixes" it later.

All eight are non-null by construction: the adapter inserts real values, and an
empty string is still a `PropertyValue::String`, not `Null`.

### 3 - Schema-version bumps

**A declared constraint must never make a future schema bump un-writable.** Both
bump directions are safe for the declared profiles, for structural reasons:

* **A per-domain `SCHEMA_VERSION` bump changes the `schema_version` VALUE, not
  its TYPE.** `require_typed("schema_version", Integer)` is invariant under every
  bump that has ever happened or is planned.
* **A new `NodeKind` mints a NEW store-side label.** A label with no declaration
  is fully schemaless upstream, so records of a new kind cannot violate anything.
  Adding kinds can never break a declared store.

This is exactly why the profiles declare **only** the universal spine. A per-kind
payload field (`name`, `path`, spans, `status`, …) is precisely the thing a
future slice is free to make optional or drop, and a constraint on one would turn
a legitimate future write into a whole-transaction abort. A unit test
(`no_profile_declares_a_per_kind_payload_field`) enforces that no profile can
ever grow one by accident.

The one change that **would** break a declared store is renaming or removing a
`base_properties` key. That is recorded as a hard prerequisite in
[`docs/schema/schema-versioning.md`](../schema/schema-versioning.md) §10: drop
the constraints first.

### 4 - Does a real store conform?

Run the report and find out — that is the whole point of the default action, and
the issue was explicit that this must be measured rather than assumed.

Measured against this repository's own graph (`eg scan .` → `eg ingest --adapter
embedded`), the `spine` profile reports **zero non-conforming entities**; see
[the recorded run](#recorded-phase-1-run) below. That is the expected result —
the declared keys are exactly the ones `base_properties` writes unconditionally —
but it is now evidence rather than an assumption, and the report is how you check
any *other* store, including one a foreign writer has touched.

### 5 - How does a violation surface?

Through `AletheiaDB`'s `Error::Constraint(ConstraintError)`, which the adapter
maps to the ordinary `AdapterError::Rejected { record_id, message }` contract.
The upstream detail (`missing required key`, `expected int, got string`, the
offending property) is preserved verbatim in the message, so the diagnostic is
usable rather than an opaque commit abort. `enable()` over non-conforming state
returns `NonConformingOnEnable` and declares **nothing** — atomic.

### Recorded Phase 1 run

Against a real store built from this repository (`eg scan src/query` →
`eg ingest --adapter embedded`, 8,838 records), `--format text`:

```
action: report
profile: spine
label_partition: one_label_per_node_kind
writable labels: 61 node, 49 edge
conformance: 16 labels scanned, 16 conforming, 8838 entities checked, 0 non-conforming
  node DebtMarker: conforms (4 checked, 0 non-conforming)
  node Diagnostic: conforms (1321 checked, 0 non-conforming)
  node File: conforms (48 checked, 0 non-conforming)
  node Import: conforms (275 checked, 0 non-conforming)
  node Module: conforms (76 checked, 0 non-conforming)
  node PanicRiskSite: conforms (100 checked, 0 non-conforming)
  node Repository: conforms (1 checked, 0 non-conforming)
  node ScanCoverage: conforms (1 checked, 0 non-conforming)
  node Symbol: conforms (996 checked, 0 non-conforming)
  edge CALLS: conforms (3747 checked, 0 non-conforming)
  edge CONSTRUCTS: conforms (213 checked, 0 non-conforming)
  edge CONTAINS: conforms (229 checked, 0 non-conforming)
  edge DEFINES: conforms (996 checked, 0 non-conforming)
  edge IMPLEMENTS: conforms (31 checked, 0 non-conforming)
  edge IMPORTS: conforms (275 checked, 0 non-conforming)
  edge REFERENCES: conforms (525 checked, 0 non-conforming)
declared constraints: 0
```

Four things this run establishes that the unit tests cannot:

* **The store conforms.** Both `spine` and `full-base` report zero
  non-conforming entities, so `--declare` is not blocked on trunk's own data.
* **`entities_checked` (8,838) equals the record count ingested (8,838).** This
  is the direct evidence for the counting note above: because the adapter is
  append-only, every record is its own scanned entity — the scan does not
  deduplicate to current records, and the total is not "lower than
  `eg inspect`".
* **16 of the 110 writable labels are actually populated** by a code-graph-only
  scan. The other 94 are reported `not_present` — not conforming — which is
  exactly the distinction the status set exists to preserve.
* **Output is byte-identical across repeated runs**, verified by diffing two
  invocations against the unchanged store.

`--declare` on the same store declared all 110 labels with no refusal and
persisted the sidecar.

## Profiles

Two candidate profiles exist so the report can quantify what a stricter
declaration would cost against real data before anyone commits to it.

| profile | node keys | additional edge keys |
|---|---|---|
| `spine` (default) | `codegraph_id`, `record_type`, `schema_version` | `label`, `source_codegraph_id`, `target_codegraph_id` |
| `full-base` | spine + `summary` | spine edge keys + `egregore_seq` |

`spine` is the recommended declaration: it constrains exactly the keys every read
path dispatches on, and nothing else.

`full-base` is true of every record written by **current** Egregore — but not
necessarily of an old one. The adapter documents legacy edges predating the
`egregore_seq` system that carry no such property, so `--declare --profile
full-base` on a store holding them is **refused**. That refusal is the report
doing its job, and running the report first is how you find out. `summary` is
also a display string rather than something a read path dispatches on — so it
buys less invariant for the same
future-bump exposure. Both are declarable; pick with `--profile`.

Every declared key is `require_typed` (required **and** typed). No profile uses
optional-but-typed constraints in this slice.

## Flags

| flag | default | meaning |
|---|---|---|
| `--data-dir <dir>` | *(required)* | the embedded store. There is no `--graph` form: a JSONL file has no store-side labels to constrain, and `eg validate` already gates that. |
| `--profile <spine\|full-base>` | `spine` | which candidate profile to evaluate or declare |
| `--declare` | off | declare the profile (opt-in; takes the write lease) |
| `--drop` | off | retract declarations (takes the write lease) |
| `--include-foreign` | off | widen `--drop` past Egregore's own labels |
| `--format <json\|text>` | `json` | JSON is the complete contract; text is a summary view |

## Actions

### `report` (default)

**Strictly read-only.** The store is copied to a throwaway temporary directory
first (the same `readonly_audit_store` path `eg inspect --data-dir` and every
other read-only audit uses), so no write lease is taken and the original store is
never re-persisted. The scan itself is upstream's `.dry_run()`, which computes
the conformance report and declares nothing.

Only labels the store actually **holds** are scanned. Upstream's conformance scan
is per-label and an edge-type scan walks every edge, so probing all inventoried
labels would cost a full pass each; the store's observed-schema summary
(`db.schema()`, one call) tells us which labels are present, and every absent
label is reported as `not_present` — a zero-checked row that is identical in
content and free to synthesise.

### `--declare`

The opt-in Phase 2 action. Takes the **exclusive write lease** (it persists the
upstream `schema_constraints.dat` sidecar into the real store, so it must contend
with any other live writer — see
[`docs/cli/embedded-concurrency.md`](embedded-concurrency.md)). It runs the same
conformance scan first, and on any violation prints the identical report and
exits 1 without declaring anything.

Declaration covers **every** writable label, including ones the store holds
nothing of yet — declaring before the data exists is the highest-value case.

Two costs to know about. At **write** time a declared label costs one property
check per write and an undeclared one costs nothing, so the steady-state overhead
is negligible. At **declaration** time it is not free: upstream's `enable()` runs
its own conformance scan per label, and an edge-type scan walks every edge, so
`--declare` performs one pass per inventoried label — including the many that are
empty — all while holding the exclusive write lease. On a large store, plan for
it.

### `--drop`

Retracts declarations, restoring the schemaless posture.

**Scoped by default.** `--drop` is the inverse of `--declare`, and `--declare`
only ever touches labels in Egregore's own inventory. A declaration on any other
label was made by something else sharing the data dir, so it is **retained** and
reported under `foreign_constraints_retained` rather than silently destroyed.
`--include-foreign` widens the retraction to everything.

**It records what it removed.** Upstream atomically rewrites the sidecar on every
drop, so without a before-image a mistaken `--drop` would be unrecoverable by
inspection — you cannot re-declare what you can no longer enumerate. The report's
`dropped_constraints` carries the full descriptor of every retracted declaration.

"Full descriptor" means every field needed to re-declare, not just the key names:
`property`, `declared_type` (`null` meaning "any type"), `vector_dim`, `required`,
and `nullable`. The distinction only bites for `--include-foreign`. For a label in
Egregore's own inventory the key names would be enough, since re-running
`--declare <profile>` regenerates the types and optionality from the profile; a
foreign declaration is the one `--declare` can never rebuild, so the recorded
descriptor is the only route back. `vector_dim` is carried separately because
upstream's type token collapses every vector arm to `vector` — without it a
restored constraint would silently widen to accept any dimension.

The same descriptor shape is used by `declared_constraints` and
`foreign_constraints_retained`.

`--declare` and `--drop` are mutually exclusive (exit 2), and `--include-foreign`
without `--drop` is the same error.

## Output contract

One deterministic JSON line (or `--format text`), byte-identical across runs on
an unchanged store.

```json
{
  "ok": true,
  "action": "report",
  "data_dir": ".egregore",
  "profile": "spine",
  "profile_properties": {
    "node": [
      {"property": "codegraph_id", "declared_type": "string", "required": true},
      {"property": "record_type", "declared_type": "string", "required": true},
      {"property": "schema_version", "declared_type": "int", "required": true}
    ],
    "edge": ["... the three node specs, plus label / source_codegraph_id / target_codegraph_id ..."]
  },
  "inventory": {
    "label_partition": "one_label_per_node_kind",
    "writable_node_labels": 61,
    "writable_edge_types": 49,
    "node_labels": ["AcceptanceCriterion", "..."],
    "edge_types": ["AGGREGATES", "..."]
  },
  "observed": {
    "unknown_node_labels": [],
    "unknown_edge_types": []
  },
  "conformance": {
    "labels_scanned": 12,
    "labels_conforming": 12,
    "entities_checked": 123456,
    "entities_non_conforming": 0,
    "rows": [
      {
        "entity_kind": "node",
        "label": "Symbol",
        "status": "conforms",
        "checked": 12345,
        "non_conforming": 0,
        "violations": []
      }
    ]
  },
  "declared_constraints": [],
  "declared_labels": 0,
  "dropped_labels": 0,
  "disclaimer": "..."
}
```

### Field notes

* **`status`** is a closed set: `conforms` (entities present, all conform),
  `violates` (entities present, some do not), `not_present` (the store holds no
  current-state entity of this label). `not_present` is reported distinctly and
  never as a vacuous `conforms`: a label with nothing in it proves nothing, and
  calling it conforming would overstate what the audit actually checked.
* **`checked` counts store entities, not deduplicated Egregore records.**
  Upstream scans its current-state view — the set of live *engine* entities.
  Egregore's embedded adapter is **append-only**: every record version is its own
  `create_node` / `create_edge` (the sole `update_node` is the embedding
  backfill), so an Egregore-*superseded* version is a distinct live engine entity
  and **is** scanned. These totals therefore track `eg inspect --data-dir`'s
  physical record counts for the label. Upstream's "superseded versions are not
  re-scanned" rule is about *engine* entity versions, which Egregore barely
  creates.
* **`violations[].sample_record_ids`** cite offending entities by their Egregore
  `codegraph_id`. Upstream samples engine-internal `u64` entity ids; those are
  neither citable nor stable across a re-ingest, so each is resolved back to a
  record handle. A sample with no resolvable handle is **counted** under
  `unresolved_samples`, never emitted — which is the common case here, since a
  *missing* `codegraph_id` is itself one of the violations being reported. The
  list is sorted, de-duplicated, and capped.
* **`observed.unknown_node_labels` / `unknown_edge_types`** name labels present
  in the store that Egregore cannot write — a foreign writer, or a newer
  Egregore. Worth knowing before declaring, since a declaration does not cover
  them.
* Output is **allow-list only**: labels, property keys, type tokens, counts,
  upstream reason strings, and record-ID handles. Never a record's payload text,
  never an engine-internal entity id.

## Exit codes

| code | meaning |
|---|---|
| 0 | report produced with no violation, or `--declare` / `--drop` succeeded |
| 1 | non-conforming entities found, **or** `--declare` was refused — either up front by non-conforming current state, or part-way through by the store. The **full report is printed in every case**, so a partial declaration is always visible. |
| 2 | usage/load error (see the code table below) |

### Usage/load diagnostics

Every exit-2 path prints one JSON object on **stderr** carrying a stable `code`:

| code | meaning |
|---|---|
| `unsupported_combination` | `--declare` and `--drop` together, or `--include-foreign` without `--drop` |
| `unknown_profile` | `--profile` is not `spine` or `full-base`; the message lists the known profiles |
| `store_unreadable` | the `--data-dir` is missing, empty, or cannot be opened, or its schema summary cannot be read |
| `store_contended` | another live writer (embedded peer or daemon) holds the write lease — see [`embedded-concurrency.md`](embedded-concurrency.md) |
| `conformance_scan_failed` | the store opened but a conformance scan itself failed |
| `drop_failed` | a retraction failed |
| `embedded_adapter_unavailable` | built without the `embedded-aletheiadb` feature |

A **read or scan failure is never exit 1**. Exit 1 means the gate found something;
a CI job keying on it must never be told "schema violations found" for an I/O
failure.

### Declaration is not atomic across labels

Upstream's `enable()` is atomic *per label*, not across the whole run. A refusal
part-way therefore leaves the store constrained on the labels that already
landed. The report always names both halves of that state — `declared_labels`
counts what landed and `declaration_refusal` names the refusing label and
upstream's reason — and `--drop` retracts whatever did.

## Epistemic boundary

Conformance is a **structural** check of property presence and type on
**current-state** entities only. It is never proof that a record's content is
correct, that its domain schema version is semantically compatible, or that
extraction was complete. Superseded record versions are not scanned. A label
reported `not_present` holds no current-state entity and was therefore not
checked.

Enforcement is also **not a guarantee you can depend on**: upstream quarantines a
corrupt `schema_constraints.dat` sidecar and starts with no constraints rather
than bricking startup, and the sidecar is written only when index persistence is
enabled. Treat a declared constraint as a **backstop that catches mistakes**,
never as a proof obligation the rest of the system may lean on. The
`eg validate` gate, the adapter's own read-back verification, and the citation
audits remain the primary contract enforcement.

## Upstream behaviour this surface depends on

From `AletheiaDB` 0.2.0's `docs/guides/schema-constraints.md` and
`src/db/schema_constraint.rs`:

* A label with **no** declaration is fully schemaless — zero write-path overhead.
* `update_*` is PATCH: constraints validate against the **effective post-write**
  map, so a patch that doesn't touch a required key does not falsely fail.
* Constraints are **forward-only**: `enable()` scans current state only,
  pre-existing history is never re-scanned or invalidated, and time-travel reads
  of a superseded version that would violate a new constraint keep working.
* A **backdated** (`valid_time`) write is recorded at transaction-time *now*, so
  it is validated against the constraint set active *now*.
* Declared constraints ride the `.albk` backup payload, so a backup→restore round
  trip preserves them.

## See also

* [`docs/cli/validate.md`](validate.md) — the pre-ingest JSONL gate this backstops
* [`docs/cli/inspect.md`](inspect.md) — the read-only store inventory
* [`docs/cli/embedded-concurrency.md`](embedded-concurrency.md) — the write lease
* [`docs/schema/schema-versioning.md`](../schema/schema-versioning.md) §10 — the
  bump rule a declared store imposes
