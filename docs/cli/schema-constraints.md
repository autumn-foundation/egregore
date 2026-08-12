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
job — but it cannot see a bad write that reaches an embedded store by any other
path: a hand-edited store, a foreign writer sharing the data dir, an adapter
regression. The adapter's own invariants live only as adapter code, which is
precisely the wrong place for a backstop: code that is wrong cannot catch itself
being wrong.

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

The inventory is **derived, never hand-maintained**. `NodeKind::ALL` and
`EdgeLabel::ALL` are each pinned by a unit test whose `match` has **no wildcard
arm**, so adding a variant fails to compile until it is deliberately classified,
and the test then fails until it is also listed. The inventory therefore cannot
silently drift from `node_label()`.

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

Edges add four more, also unconditionally:

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

Run the report and find out — that is the whole point of the default action. On
a store written only by Egregore the answer is yes by construction, because the
declared keys are the ones `base_properties` writes unconditionally. The report
is the evidence, not the assumption.

### 5 - How does a violation surface?

Through `AletheiaDB`'s `Error::Constraint(ConstraintError)`, which the adapter
maps to the ordinary `AdapterError::Rejected { record_id, message }` contract.
The upstream detail (`missing required key`, `expected int, got string`, the
offending property) is preserved verbatim in the message, so the diagnostic is
usable rather than an opaque commit abort. `enable()` over non-conforming state
returns `NonConformingOnEnable` and declares **nothing** — atomic.

## Profiles

Two candidate profiles exist so the report can quantify what a stricter
declaration would cost against real data before anyone commits to it.

| profile | node keys | additional edge keys |
|---|---|---|
| `spine` (default) | `codegraph_id`, `record_type`, `schema_version` | `label`, `source_codegraph_id`, `target_codegraph_id` |
| `full-base` | spine + `summary` | spine edge keys + `egregore_seq` |

`spine` is the recommended declaration: it constrains exactly the keys every read
path dispatches on, and nothing else. `full-base` is equally true of every record
Egregore has ever written, but `summary` is a display string rather than
something a read path dispatches on — so it buys less invariant for the same
future-bump exposure. Both are declarable; pick with `--profile`.

Every declared key is `require_typed` (required **and** typed). No profile uses
optional-but-typed constraints in this slice.

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
nothing of yet — declaring before the data exists is the highest-value case, and
costs nothing.

### `--drop`

Retracts every schema constraint declared on the store, restoring the fully
schemaless posture. It retracts what is actually **declared** (read back from the
store) rather than what the current inventory would declare, so a store declared
by an older or newer Egregore is still fully cleaned.

`--declare` and `--drop` are mutually exclusive (exit 2).

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
    "node": [{"property": "codegraph_id", "declared_type": "string", "required": true}],
    "edge": []
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
* **`checked` is a CURRENT-STATE count.** Upstream scans the current-state view,
  so superseded record versions are not checked. On a re-ingested or
  `scan-history` store these totals will be lower than `eg inspect --data-dir`'s
  physical record counts. That is not a discrepancy — it is what enforcement
  covers, since enforcement is forward-only.
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
| 1 | non-conforming entities found (the **full report is still printed**), or `--declare` was refused by non-conforming current state |
| 2 | usage/load error: missing or unreadable `--data-dir`, both mode flags, unknown `--profile`, or a build without the `embedded-aletheiadb` feature |

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
