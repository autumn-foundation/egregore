# eg query evidence-path

Trace **one deterministic cross-domain evidence witness path between two records**
(issue #247) — answer *"is record A grounded in record B, and by what chain?"* in
a single cited path instead of hand-walking the graph. Over a `--graph` JSONL or
an embedded `--data-dir` store. Local-first; no network access, hosted indexing,
remote crawling, or embeddings.

> **A witness path proves a live evidence-edge chain connects these two
> records.** It is *not* proof the cited code still matches current source, and
> `EMITTED_DURING` hops are correlation leads, never causation. Absence of a path
> over the traversed evidence-edge classes is not proof no grounding exists:
> code-graph topology and other excluded edge classes are not traversed by
> design.

This query is a **read-time traversal** — it mints no edge and introduces no new
node kind, edge label, or trust class. It reads only the edges already in the
graph, partitioned into a **traversed** evidence/provenance set and an
**excluded** code-topology set by an exhaustive compile-time classification of
every edge label (a new label forces a conscious decision — the completeness
invariant).

## Synopsis

```text
eg query evidence-path <SOURCE_ID> <TARGET_ID> --graph <PATH>   [--format json|text]
eg query evidence-path <SOURCE_ID> <TARGET_ID> --data-dir <DIR> [--format json|text]
```

`<SOURCE_ID>` and `<TARGET_ID>` are exact stable record IDs (e.g.
`agent_memory:v1:<hex>`, `codegraph:v5:<hex>`, `verification:v1:<hex>`,
`log:v2:<hex>`). The lane is **repo-agnostic**: endpoints are exact IDs and a
grounding chain may legitimately cross repositories, so there is no `--repo`
flag; multi-repo stores are handled by per-node repository attribution. There is
no `--at`/`--as-of` — the traversal is a current-state view (see *Liveness*).

The query is purely read-time: it reads only the supplied store, never Git state,
so it cannot mutate the working tree. Over `--data-dir` it reads from a throwaway
copy, so no store index files are created or mutated. No raw
source/transcript/command/patch text ever enters the response — only IDs,
domains, kinds, edge labels, paths, spans, counts, and basis strings.

## Traversed vs excluded edge classes

Reachability is **undirected** over the evidence-edge subgraph: a grounding chain
legitimately mixes edge directions (an `OBSERVES` edge points memory→code while a
`VALIDATED_BY` edge points memory→evidence), so each edge is usable in either
direction. Each emitted hop reports the edge's **native** `from`/`to` as stored
plus a `traversal_direction` (`forward` when the walk moved along the stored edge,
`reverse` when against it).

**Traversed** (cross-domain evidence / provenance / grounding edges):
`HAS_EVIDENCE`, `OBSERVES`, `MENTIONS_SYMBOL`, `TOUCHED_FILE`, `PRODUCED_PATCH`,
`PRODUCED_EVIDENCE`, `VALIDATED_BY`, `CLOSES_ACCEPTANCE_CRITERION`,
`OWNED_BY_TASK`, `EXTERNAL_HANDLE`, `TOUCHES_FILE`, `MERGED_AS`, `REVIEWS_COMMIT`,
`REVIEWED_BY`, `REQUESTED_REVIEW_FROM`, `TRANSITIONS_REVIEW`, `FAILED_ON`,
`EXPLAINS_CHANGE`, `REFERENCES_TASK`, `CONTRADICTS`, `SUPERSEDES`, `RELATES_TO`,
`FRAME_RESOLVES_TO`, `EMITTED_DURING`, `MATERIALIZED_AS`, `PROPOSED_BY`,
`PROMPTED_FOR`, `DECIDED_ON`, `REVOKED_BY`, `SCOPED_TO_REPO`, `FINGERPRINTED_AS`,
`CAPTURED_FROM`, `AGGREGATES`.

**Excluded** (code-graph structural topology and intra-memory scaffolding — never
grounding evidence): `CONTAINS`, `DEFINES`, `IMPORTS`, `REFERENCES`, `CALLS`,
`IMPLEMENTS`, `MENTIONS`, `CHANGED_IN`, `PARENT_OF`, `DRIFTS_FROM`,
`DRIFTS_PRIOR`, `MEASURED_BY`, `SESSION_OF`, `AUTHORED_BY`.

Both lists ride in the envelope so a `no_path` verdict is never presented as
proof no grounding exists.

## Determinism / tie-break

The path is the **deterministic shortest path**: fewest hops, then the
lexicographically smallest path compared as the **full ordered sequence of
`(neighbor_record_id, edge_record_id)` steps from the source** — so a difference
at the first step dominates any later step. Cycles terminate via a visited set.
Output is byte-identical across runs.

## Liveness

A record is **deleted** when it is tombstoned (a `Retraction`/tombstone names it)
and does not carry a bitemporal `temporal` version. Deleted records — and edges
touching a deleted endpoint — are excluded from the graph. A tombstoned id that
also carries a temporal version stays live. Over an append-only `--graph`, a
stable edge ID re-ingested with changed metadata resolves to its **latest write**
(mirroring embedded `latest_edge_versions`), so both transports surface identical
edge basis/confidence. This is a current-state view; there is no temporal
selector.

## Exit codes

| Condition | error_type | Exit |
| --- | --- | --- |
| A witness path was found (≥ 1 hop) | — (`ok: true`) | 0 |
| `source_id == target_id` | `identical_endpoints` | 1 |
| Both endpoints live, no evidence chain | `no_path` | 1 |
| An endpoint names no record | `endpoint_not_found` | 2 |
| An endpoint names a tombstoned record | `endpoint_tombstoned` | 2 |
| Both/neither of `--graph`/`--data-dir`, or load failure | (loader error) | 1 |

If **both** endpoints have problems, the source's problem is reported first
(deterministic). `endpoint_tombstoned` is a distinct label from
`endpoint_not_found`.

## Response shape

Success — a summary line, then one hop per line (NDJSON):

```jsonc
// line 1: summary
{
  "ok": true,
  "source": { "record_id": "agent_memory:v1:obs", "schema_version": 1,
              "domain": "agent_memory", "trust_class": "agent_observation",
              "kind": "Observation" },
  "target": { "record_id": "verification:v1:verif", "schema_version": 1,
              "domain": "verification", "trust_class": "verification",
              "kind": "Verification" },
  "hop_count": 2,
  "traversed_edge_classes": ["AGGREGATES", "CAPTURED_FROM", "..."],
  "excluded_edge_classes": ["AUTHORED_BY", "CALLS", "..."],
  "disclaimer": "A witness path proves a live evidence-edge chain ..."
}
// line 2: hop 0
{
  "index": 0,
  "from": { "record_id": "agent_memory:v1:obs", "domain": "agent_memory", "...": "..." },
  "to":   { "record_id": "codegraph:v5:sym", "domain": "codegraph",
            "trust_class": "source_fact", "kind": "Symbol",
            "repo_relative_path": "src/lib.rs", "span": { "...": 0 } },
  "edge": { "label": "OBSERVES", "edge_record_id": "codegraph:v5:...",
            "traversal_direction": "forward" }
}
// line 3: hop 1 (an EMITTED_DURING hop also carries basis + confidence)
{
  "index": 1,
  "from": { "record_id": "log:v2:sig", "...": "..." },
  "to":   { "record_id": "verification:v1:run", "...": "..." },
  "edge": { "label": "EMITTED_DURING", "edge_record_id": "log:v2:...",
            "traversal_direction": "forward",
            "basis": "content_hash_join", "confidence": "1.0" }
}
```

Each node **row** is a redaction-safe projection: `record_id`, `schema_version`,
`domain`, `trust_class` (derived from the domain), `kind`, and — for code nodes —
`repo_relative_path` + `span` (both omitted when absent). No free-text summary or
body field is ever emitted. An `EMITTED_DURING` hop's edge additionally carries
`basis` (`content_hash_join` / `temporal_correlation`) and its documented
`confidence` (`1.0` / `0.5`); other hops omit both.

Error / `no_path` — a single JSON line:

```jsonc
{ "ok": false, "error": {
    "error_type": "no_path",
    "source": { "record_id": "...", "...": "..." },
    "target": { "record_id": "...", "...": "..." },
    "traversed_edge_classes": ["..."],
    "excluded_edge_classes": ["..."],
    "disclaimer": "..." } }

{ "ok": false, "error": {
    "error_type": "endpoint_tombstoned", "side": "target",
    "handle": "codegraph:v5:dead", "disclaimer": "..." } }
```

`--format text` prints a compact human rendering: a header naming source/target,
one line per hop
(`0  <from_id> [<domain>/<kind>] --OBSERVES(forward)--> <to_id> [<domain>/<kind>]`),
and a footer stating hop count, traversed/excluded class counts, and the
disclaimer. Errors print a one-line human message. Both formats are
byte-identical across runs.

## When to use which tool

- **`eg query evidence-path A B`** — *verify one chosen claim is grounded
  end-to-end across domains*: you have two specific records and want the concrete
  evidence chain (or a typed `no_path` verdict) between them.
- **`eg audit citations` (#217)** — a store-wide broken-evidence-link sweep; use
  it to find *all* dangling citations, not to inspect one A→B chain.
- **`eg query context` / handle dereference (#160)** — resolve or expand *one*
  handle's immediate neighborhood; evidence-path instead connects *two* handles.
- **`eg query transitive-callers` / `transitive-callees` (#225)** — a **code-only**
  `CALLS`/`REFERENCES` call path; evidence-path deliberately **excludes** those
  topology edges and walks the cross-domain evidence/provenance edges instead.
## Corpus scope

**Changed default (issue #456).** With **neither** a corpus flag nor (where
offered) an `--at`/`--as-of` selector, over a `scan-history` store this lane now
defaults to the **HEAD-anchored** corpus — records current at each repository's
stamped HEAD commit — so a record removed before HEAD no longer appears. This
flips the pre-#456 default, which read the **union** of all commit snapshots.
Pass `--all-history` to opt back into that union; pass `--at-head` to force the
HEAD-anchored view explicitly. Over a snapshot-less (plain `scan`) store the
disclosure is `single_snapshot`. The envelope discloses `corpus_mode` /
`corpus_mode_source` / `corpus_disclaimer`. `--at-head` and `--all-history` are
mutually exclusive with each other (and, where offered, with `--at`/`--as-of`); a
conflict exits `1` with `unsupported_combination`. See
[Corpus scope for query lanes](corpus-modes.md).
