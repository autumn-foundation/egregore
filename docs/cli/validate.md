# eg validate

Validate a graph JSONL for **referential integrity** before it becomes the
source of query answers. One read-only pass, local and offline — no network
access, hosted indexing, or embeddings — that fails loudly at the gate instead
of letting a corrupt or incomplete graph silently produce lossy answers
downstream.

> **Structural reference closure only.** `eg validate` does not check parse
> correctness, semantic accuracy, schema-version compatibility (issue #16), or
> whether extraction was complete (issue #87). A graph can pass this gate and
> still describe the wrong code; what it cannot do is dangle references that
> make `eg query file` return less than the graph itself contains.

## Where it fits

`eg validate` sits between `scan` and `ingest`:

```sh
eg scan . --out graph.jsonl          # or: eg scan-history . --out graph.jsonl
eg validate graph.jsonl              # gate: exit 0 = referentially closed
eg ingest graph.jsonl --adapter embedded --data-dir .egregore
```

`eg inspect` counts records but never asks whether an edge points at a node
that exists; `eg ingest --adapter dry-run` validates per-record schema version
and write/read-back ordering, not cross-record reference closure. This command
owns the closure check.

## Synopsis

```text
eg validate <GRAPH> [--format json|text]
```

| Condition | Exit | Output |
|-----------|------|--------|
| Clean graph — zero defects | `0` | Summary line only, `ok:true` |
| Any referential defect | `1` | One machine-readable diagnostic per defect, then a summary line, `ok:false` |
| Unreadable / malformed graph input | `2` | `{"code":"graph_read_error"\|"graph_parse_error",...}` on stderr |

Output is JSONL by default (one JSON object per line, matching the query
output convention); `--format text` renders the same fields one line per
defect. Diagnostics are emitted in a deterministic canonical order (defect
category, then offending record IDs): repeating the same validation on the
same input is byte-identical.

## Checks

1. **Edge endpoint resolution** — every edge `source` and `target` resolves to
   a node present in the graph (`dangling_edge_endpoint`).
2. **Typed edge target kinds** — every `DEFINES`, `CONTAINS`, `CALLS`,
   `IMPORTS`, and `MENTIONS` edge targets a node of an allowed kind
   (`edge_target_kind_violation`):

   | Relation | Allowed target kinds |
   |----------|----------------------|
   | `DEFINES` | `Symbol` |
   | `CONTAINS` | `File`, `Module`, `Commit`, `Change`, `PanicRiskSite`, `DebtMarker` (issue #218 debt-comment markers) |
   | `CALLS`, `MENTIONS` | `Symbol`, `Diagnostic` (unresolved-call markers) |
   | `IMPORTS` | `Import` |

3. **Edges to tombstoned records** — no edge references a
   tombstoned-and-unsuperseded record: an ID named by a tombstone with no
   surviving node record of the same ID (`edge_to_tombstoned_record`). A
   surviving node record supersedes the tombstone for this edge-side check —
   the reference still resolves — and the conflict is reported on the
   tombstone instead (check 4).
4. **Tombstones stranding live edges** — no record is named by a tombstone yet
   still referenced by a live edge as source or target
   (`tombstone_strands_live_edge`).
5. **Orphan nodes** — no topology node (`File`, `Module`, `Symbol`, `Import`)
   has zero incident edges (`orphan_node`). An orphaned symbol is invisible to
   edge-walking queries such as `eg query file`. `Repository` (the containment
   root) and `Diagnostic` markers legitimately stand alone and are exempt, as
   are non-code-graph node kinds.

Clean `eg scan` and `eg scan-history` outputs pass all checks.

## Diagnostics

One JSON object per defect. Every diagnostic carries a stable `code`, the
offending record ID(s), the relation label where applicable, and the
repo-relative path/span when present. Output never includes raw transcript
text, command output, patch hunks, tokens, record summaries, or protected
raw-artifact payloads — only record IDs, categories, relation labels, paths,
spans, and counts.

```json
{"code":"dangling_edge_endpoint","edge_id":"codegraph:v5:…","relation":"CALLS","endpoint":"target","missing_id":"codegraph:v5:…"}
{"code":"edge_target_kind_violation","edge_id":"codegraph:v5:…","relation":"DEFINES","target_id":"codegraph:v5:…","target_kind":"Import","allowed_kinds":["Symbol"],"repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":12,"start_line":1,"end_line":1}}
{"code":"edge_to_tombstoned_record","edge_id":"codegraph:v5:…","relation":"CALLS","endpoint":"target","tombstoned_id":"codegraph:v5:…","tombstone_id":"codegraph:v5:…"}
{"code":"orphan_node","record_id":"codegraph:v5:…","kind":"Symbol","repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
{"code":"tombstone_strands_live_edge","tombstone_id":"codegraph:v5:…","deleted_id":"codegraph:v5:…","stranded_edge_ids":["codegraph:v5:…"],"repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
```

The final stdout line is always a machine-readable summary:

```json
{"defects":0,"edges":33,"nodes":23,"ok":true,"records":56,"tombstones":0}
```

## Out of scope

- Auto-repair or rewriting of a broken graph (`eg repair`, issue #72 family).
- Schema-version compatibility validation (issue #16): a graph line with an
  unknown future schema version is a load error (exit 2), not a defect
  classification.
- Parse/extraction-completeness signals (issue #87), semantic correctness,
  drift quality (issue #55), and agent-memory composition health (issue #94).
