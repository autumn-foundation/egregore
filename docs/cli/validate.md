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
> make `eg query file` return less than the graph itself contains. For the log
> domain (issue #327) this is doubly true: the gate asserts structural
> reference closure only — never fingerprint correctness, timestamp accuracy,
> or correlation validity.

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
   | `FINGERPRINTED_AS` | `ErrorSignature` (issue #319 log domain) |
   | `CAPTURED_FROM` | `LogSource` |
   | `AGGREGATES` | `ErrorSignature` |
   | `FRAME_RESOLVES_TO` | `Symbol`, `File`, `Diagnostic` (issue #322 resolution ladder) |
   | `EMITTED_DURING` | `CommandRun`, `AgentTurn`, `AgentSession` (reserved for issue #323) |

3. **Edges to tombstoned records** — no edge references a
   tombstoned-and-unsuperseded record: an ID named by a tombstone with no
   surviving node record of the same ID (`edge_to_tombstoned_record`). A
   surviving node record supersedes the tombstone for this edge-side check —
   the reference still resolves — and the conflict is reported on the
   tombstone instead (check 4).
4. **Tombstones stranding live edges** — no record is named by a tombstone yet
   still referenced by a live edge as source or target
   (`tombstone_strands_live_edge`).
5. **Orphan nodes** — no topology node (`File`, `Module`, `Symbol`, `Import`,
   `DependencyDeclaration`, `LogEvent`, `LogOccurrenceBucket`) has zero incident
   edges (`orphan_node`). An orphaned symbol is invisible to edge-walking
   queries such as `eg query file`; an unattached dependency declaration has
   lost the `File —CONTAINS→ DependencyDeclaration` chain repository scoping
   walks; a `LogEvent`/`LogOccurrenceBucket` is always emitted attached to its
   `ErrorSignature`/`LogSource` (issue #319). `Repository` (the containment
   root) and `Diagnostic` markers legitimately stand alone and are exempt, as
   are `LogSource` (a root/sink that may legitimately be edge-less on an
   empty-log scan), `ErrorSignature`, and non-code-graph node kinds.
6. **Dependency containment** — every `DependencyDeclaration` with incident
   edges is the target of a `CONTAINS` edge from a `File` node whose
   repo-relative path equals the dependency's declared manifest handle
   (`missing_containment_edge`). Any other edge — or containment by a source
   file or a different manifest — is not enough: without the declaring
   manifest's `File —CONTAINS→ DependencyDeclaration` attribution chain,
   repository scoping silently drops the fact while the graph would
   otherwise validate clean. The containing Files must additionally belong
   to ONE repository (direct `Repository —CONTAINS→ File` ownership):
   same-path manifests exist across repos in a merged store, so a
   dependency whose containing Files span two owners has ambiguous
   attribution and is the same defect. A dependency contained only by a
   single (possibly foreign) repo's manifest is topologically
   indistinguishable from a legitimate row of that repo — record IDs are
   opaque — and graphs without `Repository`-owned Files keep the
   path-equality-only behavior, so legacy/partial graphs are never
   mass-flagged.
7. **Log-domain structural completeness** (issue #327) — every log-domain node
   with incident edges carries exactly one of each required OUTBOUND structural
   edge, matching what the issue #319/#320 extractor emits:

   | Log node | Required outbound edges |
   |----------|-------------------------|
   | `LogEvent` | one `FINGERPRINTED_AS` and one `CAPTURED_FROM` |
   | `LogOccurrenceBucket` | one `AGGREGATES` (no bucket `CAPTURED_FROM` — its `LogSource` is reached via the signature) |

   A missing required edge is `missing_log_structural_edge`; a surplus (more
   than one distinct edge record of a required relation — e.g. a `LogEvent`
   captured from two `LogSource`s) is `duplicate_log_structural_edge`, listing
   the offending edge IDs. Counting is by distinct edge record ID, so an
   identical re-emitted edge record is not a duplicate. Only incident nodes are
   evaluated — a zero-edge log node stays a single `orphan_node` (check 5) and
   is never double-reported.

Clean `eg scan`, `eg scan-history`, and `eg scan-logs` outputs pass all checks.

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
{"code":"missing_log_structural_edge","relation":"CAPTURED_FROM","record_id":"log:v1:…","kind":"LogEvent","repo_relative_path":"app.log","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
{"code":"duplicate_log_structural_edge","relation":"CAPTURED_FROM","stranded_edge_ids":["log:v1:…","log:v1:…"],"record_id":"log:v1:…","kind":"LogEvent","repo_relative_path":"app.log","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
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
