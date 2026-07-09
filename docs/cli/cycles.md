# eg query cycles

Enumerate **dependency cycles among files** — module A depends on B depends on
C depends on A — over edges the graph already holds. Local-first, no network,
no re-parsing. The pre-refactor question "is this module in a cycle, and what
is the exact loop?" becomes one deterministic query instead of hand-rolled
traversal over the JSONL (issue #138).

> **Structural leads, not runtime proof.** Cycles are derived from extracted,
> resolution-labeled graph edges. Absence of a reported cycle is not proof the
> modules are acyclic at runtime (excluded ambiguous edges are tallied in
> `counts`); presence is a lead to inspect before sequencing a refactor.

## Synopsis

```text
eg query cycles           --graph <PATH>    [--repo <SELECTOR>] [--format json|text]
eg query cycles           --data-dir <DIR>  [--repo <SELECTOR>] [--format json|text]
eg query cycles <SCOPE>   --graph <PATH>    [--repo <SELECTOR>] [--format json|text]
```

`<SCOPE>` is optional: a symbol record ID (`codegraph:vN:<hex>`), an exact
recorded symbol name, or a repo-relative file path. When present, only cycles
containing that node's file are returned — the targeted pre-refactor check.
Scope handles resolve through the same handle-resolution contract as
`eg query change-impact`.

| Condition | Exit | Output |
|-----------|------|--------|
| Cycles computed — including **zero** cycles | `0` | Cycles JSON on stdout, `ok:true` |
| Malformed / ambiguous / unsupported scope handle | `1` | `FailureHandleError` JSON on stderr |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |
| Scope handle resolves to no live record | `2` | `{"ok":false,"error":{"code":"no_match"\|"stale_handle",...}}` on stdout |

An acyclic graph — or a scoped node that participates in no cycle — is a
**real, successful, explicit answer**: exit 0 with `ok:true`, an empty
`cycles` array, and an `acyclic` diagnostic. Never an error, never a silent
empty payload.

## The dependency graph

Nodes are the graph's `File` records (one per repository + repo-relative
path). Directed dependency edges come from two already-extracted sources — no
new extraction in this slice:

- **Resolved `CALLS` edges** (issues #152/#134). A cross-file `CALLS` edge
  contributes a `caller-file → callee-file` dependency **only when labeled
  `resolved`**. Edges labeled `ambiguous` or `unresolved` are excluded from
  cycle detection and tallied in `counts` — an ambiguous edge must never
  fabricate a cycle. A cross-file edge carrying **no** resolution label (an
  older or third-party store predating the resolution field) is likewise
  excluded and tallied in `counts.calls_unlabeled_excluded` with an
  `unlabeled_calls_excluded` diagnostic: absence means "outside the
  resolution contract", never "resolved" — re-scan to label such edges.
  Excluded-edge tallies are scoped to the calling symbol's repository, so a
  `--repo`-scoped response never reports another repository's edges.
- **Import declarations** (`IMPORTS` edges to `Import` nodes). Each imported
  item — grouped (`a::{X, Y}`) and aliased (`X as Y`) imports expanded — is
  name-matched against symbol definitions in the **same repository**. Exactly
  one defining file → an `importing-file → defining-file` dependency; two or
  more candidate files → ambiguous, excluded and tallied; no in-repo
  candidate → external (e.g. `std::`), tallied. Name-based, so this mirrors
  the leads-not-proof contract of the import join in `change-impact`.
  **Import name resolution is Rust-only in this slice**: Python / TypeScript
  / Go `Import` nodes carry raw statement text that Rust path parsing cannot
  resolve, so they are excluded and tallied in
  `counts.imports_non_rust_excluded` with a `non_rust_imports_excluded`
  diagnostic — never silently treated as external, and their absence from
  the cycle set is never a bare acyclicity claim.

Same-file dependencies never form an edge, so self-loops are excluded by
construction. `--repo <SELECTOR>` restricts the whole dependency graph to one
repository; cross-repository name collisions never create edges either way.

## Determinism and canonical form

Output is byte-identical across repeated runs on an unchanged graph:

- Vertices are ordered by (`repo_relative_path`, repository).
- Each **elementary** cycle is reported exactly once — rotations and
  self-restarts are normalized by rooting every cycle at its
  lexicographically smallest member.
- The cycle set is sorted by the members' path sequence.
- Enumeration is capped at 100 cycles; hitting the cap sets a
  `cycles_truncated` diagnostic (the returned set is the canonical prefix),
  never a silent cutoff.
- Per-edge evidence is sorted by (relation, record ID) and capped at 8 rows,
  with the full count in `evidence_total`.

## Output shape

```json
{
  "ok": true,
  "edge_policy": "resolved CALLS edges and imports name-resolving to exactly one in-repo defining file ...",
  "disclaimer": "Cycles are derived from extracted, resolution-labeled graph edges. ...",
  "cycles": [
    {
      "length": 3,
      "path": "src/alpha.rs -> src/beta.rs -> src/gamma.rs -> src/alpha.rs",
      "members": [
        { "record_id": "codegraph:v4:...", "repo_relative_path": "src/alpha.rs" },
        { "record_id": "codegraph:v4:...", "repo_relative_path": "src/beta.rs" },
        { "record_id": "codegraph:v4:...", "repo_relative_path": "src/gamma.rs" }
      ],
      "edges": [
        {
          "from": "src/alpha.rs",
          "to": "src/beta.rs",
          "relations": ["CALLS"],
          "evidence": [
            { "relation": "CALLS", "record_id": "codegraph:v4:...", "resolution": "resolved" }
          ],
          "evidence_total": 1
        }
      ]
    }
  ],
  "counts": {
    "files": 3,
    "dependency_edges": 3,
    "cycles_total": 1,
    "cycles_returned": 1,
    "calls_resolved": 3,
    "calls_ambiguous_excluded": 0,
    "calls_unresolved_excluded": 0,
    "calls_unlabeled_excluded": 0,
    "imports_resolved": 0,
    "imports_ambiguous_excluded": 0,
    "imports_external": 0,
    "imports_non_rust_excluded": 0
  },
  "diagnostics": []
}
```

- `members` is the ordered node path; `edges[i]` connects `members[i]` to
  `members[(i + 1) % length]`, closing the loop. Every member carries a
  stable `File` record ID plus a repo-relative handle; every edge cites the
  contributing `CALLS` edge or `Import` node records it was derived from.
- The scoped form adds a `scope` object echoing the handle, resolved target
  type (`symbol` / `file`), and anchor record IDs; `counts.cycles_total`
  still reports the unfiltered count so "filtered to zero" is
  distinguishable from "graph is acyclic".
- Diagnostic codes: `acyclic`, `ambiguous_dependencies_excluded`,
  `unresolved_calls_excluded`, `unlabeled_calls_excluded`,
  `non_rust_imports_excluded`, `cycles_truncated`.

`--format text` prints one human-readable line per cycle (`cycle 1 (length
3): src/alpha.rs -> src/beta.rs -> src/gamma.rs -> src/alpha.rs`) or
`no dependency cycles detected`; the exact text format is not stable and must
not be parsed by scripts.

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Report every dependency cycle in the repo
eg query cycles --graph graph.jsonl

# Pre-refactor check: is this module part of a cycle?
eg query cycles src/adapters/mod.rs --graph graph.jsonl

# Scope to one repository in a multi-repo store
eg query cycles --graph graph.jsonl --repo acme/widget
```

## Relationship to neighboring tools

- **`rg` / `git grep` / IDE search** — "is there a cycle" is a reachability
  question, not a text pattern; a grep over imports still leaves the graph
  traversal to you.
- **`cargo` / rust-analyzer** — the compiler rejects *crate*-level circular
  deps, but intra-crate module import cycles are legal Rust and invisible to
  `cargo`; there is no scriptable "show me the cycle" query.
- **`eg query change-impact` (issue #76)** — direct-neighborhood leads for
  one handle; it does not close loops. `cycles` answers the loop question
  over the same edge classes.
- **Issue #123 (dependencies query)** — direct callees/imports of one
  symbol; cycle enumeration is the transitive closure this slice adds.
