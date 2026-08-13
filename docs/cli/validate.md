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

2a. **Typed log-domain edge source kinds** (issue #327) — the log schema frames
   every log structural edge *directionally*, so the pre-ingest gate also
   constrains the SOURCE kind of the five log relations
   (`edge_source_kind_violation`). Code-graph relations are source-unconstrained
   and unaffected. Presenting the log constraints directionally
   (source kind → relation → target kind):

   | Source kind | Relation | Target kind |
   |-------------|----------|-------------|
   | `LogEvent` | `FINGERPRINTED_AS` | `ErrorSignature` |
   | `ErrorSignature`, `LogEvent` | `CAPTURED_FROM` | `LogSource` |
   | `LogOccurrenceBucket` | `AGGREGATES` | `ErrorSignature` |
   | `ErrorSignature` | `FRAME_RESOLVES_TO` | `Symbol`, `File`, `Diagnostic` |
   | `ErrorSignature` | `EMITTED_DURING` | `CommandRun`, `AgentTurn`, `AgentSession` (reserved) |

   A `LogOccurrenceBucket` is deliberately **not** an allowed `CAPTURED_FROM`
   source: a bucket's `LogSource` is reached transitively via its signature's
   own `CAPTURED_FROM`, so a `LogOccurrenceBucket —CAPTURED_FROM→ LogSource`
   edge — whose target is a legitimate `LogSource` — is invalid source
   attribution the gate rejects rather than accepting as clean.

2b. **Importer-edge source-kind attribution** (issues #369, #386) — the two
   reviewer-identity edges (issue #335) and the two commit-anchor project edges
   (issues #333/#334) additionally require the SOURCE node's importer
   `source_kind` string to match the daemon's `require_project_edge_source_kind`
   gate, not merely the source node kind
   (`edge_source_kind_attribution_violation`):

   | Relation | Required source node kind | Required source `source_kind` |
   |----------|---------------------------|-------------------------------|
   | `REVIEWED_BY` | `Review` | `github_review` |
   | `REQUESTED_REVIEW_FROM` | `Task` | `github_pr` |
   | `MERGED_AS` | `Task` | `github_pr` |
   | `REVIEWS_COMMIT` | `Review` | `github_review` |

   `MERGED_AS` and `REVIEWS_COMMIT` additionally require a `Commit` target,
   matching the daemon's target rule.

   Without this, a node-kind-correct but mis-attributed source — a hand-authored
   `Review` carrying no `source_kind`, or a `github_issue` `Task` merged as a
   commit — would pass the offline gate while the daemon's `validate_project_edge`
   rejects it, so direct embedded ingest could persist a merge/review binding
   `eg validate` called clean. The check runs only when the source node kind is already valid for the
   relation, so a wrong-kind source is reported once (as
   `edge_source_kind_violation`), never doubly. The diagnostic carries the
   observed `source_kind` (absent when the node has none) and the
   `required_source_kind`. The GitHub importer always stamps the correct
   `source_kind`, so this hardens hand-authored and third-party graphs.

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
   `DependencyDeclaration`, `LogEvent`, `LogOccurrenceBucket`, `ErrorSignature`)
   has zero incident edges (`orphan_node`). An orphaned symbol is invisible to
   edge-walking queries such as `eg query file`; an unattached dependency
   declaration has lost the `File —CONTAINS→ DependencyDeclaration` chain
   repository scoping walks; a `LogEvent`/`LogOccurrenceBucket`/`ErrorSignature`
   is always emitted attached to its `ErrorSignature`/`LogSource` (issue #319).
   `Repository` (the containment root) and `Diagnostic` markers legitimately
   stand alone and are exempt, as is `LogSource` (a root/sink that may
   legitimately be edge-less on an empty-log scan) and non-code-graph node
   kinds.
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
   with incident edges carries its required OUTBOUND structural edges, matching
   what the issue #319/#320 extractor emits, each with a per-requirement
   cardinality:

   | Log node | Required outbound edges |
   |----------|-------------------------|
   | `LogEvent` | exactly one `FINGERPRINTED_AS` **and** at least one `CAPTURED_FROM` |
   | `ErrorSignature` | **at least one** `CAPTURED_FROM` |
   | `LogOccurrenceBucket` | exactly one `AGGREGATES` (no bucket `CAPTURED_FROM` — its `LogSource` is reached via the signature) |

   A missing required edge (count 0) is `missing_log_structural_edge` for every
   requirement. A surplus (more than one distinct edge record of a relation)
   is `duplicate_log_structural_edge` — listing the offending edge IDs — **only
   for the exactly-one requirements** (`LogEvent`'s `FINGERPRINTED_AS` and
   `LogOccurrenceBucket`'s `AGGREGATES`). For a `LogEvent` that means
   `duplicate_log_structural_edge` now fires **only** on multiple distinct
   `FINGERPRINTED_AS` targets — an event fingerprinted as two different
   signatures is malformed.

   Both `ErrorSignature`'s and `LogEvent`'s `CAPTURED_FROM` are **at least one**,
   so `duplicate_log_structural_edge` never fires for either node's source edge.
   A signature ID is a repo/fingerprint aggregate that **excludes** the source,
   and a `LogEvent` ID likewise excludes the source (it is keyed on
   repo/signature/valid-time/content-hash), while each `LogSource` ID is
   path/hash-distinct. `scan-logs` emits a distinct `CAPTURED_FROM` per
   `LogSource`, so a graph combining two log files where the same exemplar
   appears in both legitimately gives one `LogEvent` a `CAPTURED_FROM` edge to
   each `LogSource` (and, sharing a normalized template/severity, one
   `ErrorSignature` multiple `CAPTURED_FROM` edges too). That is a valid
   multi-source aggregate, not a duplicate.

   Counting is by distinct edge record ID, so an identical re-emitted edge record
   is not a duplicate. Only incident nodes are evaluated — a zero-edge log node
   stays a single `orphan_node` (check 5) and is never double-reported.

   The bucket's `LogSource` is reached via the signature, but that transitive
   attribution is not merely assumed: the signature's own `CAPTURED_FROM` is
   itself a required, validated edge here, so a signature that is missing or
   duplicates its source edge is flagged rather than silently stranding both
   its own and its buckets' source attribution.

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
{"code":"edge_source_kind_violation","edge_id":"log:v2:…","relation":"CAPTURED_FROM","endpoint":"source","allowed_kinds":["ErrorSignature","LogEvent"],"record_id":"log:v2:…","kind":"LogOccurrenceBucket","repo_relative_path":"app.log","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
{"code":"edge_to_tombstoned_record","edge_id":"codegraph:v5:…","relation":"CALLS","endpoint":"target","tombstoned_id":"codegraph:v5:…","tombstone_id":"codegraph:v5:…"}
{"code":"orphan_node","record_id":"codegraph:v5:…","kind":"Symbol","repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
{"code":"tombstone_strands_live_edge","tombstone_id":"codegraph:v5:…","deleted_id":"codegraph:v5:…","stranded_edge_ids":["codegraph:v5:…"],"repo_relative_path":"src/lib.rs","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
{"code":"missing_log_structural_edge","relation":"CAPTURED_FROM","record_id":"log:v2:…","kind":"LogEvent","repo_relative_path":"app.log","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
{"code":"duplicate_log_structural_edge","relation":"FINGERPRINTED_AS","stranded_edge_ids":["log:v2:…","log:v2:…"],"record_id":"log:v2:…","kind":"LogEvent","repo_relative_path":"app.log","span":{"start_byte":0,"end_byte":10,"start_line":1,"end_line":1}}
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

## See also

`eg validate` gates a graph JSONL file **between `scan` and `ingest`**, so it
cannot see a bad record that reaches an embedded store by another path.
[`eg audit schema-constraints`](schema-constraints.md) (issue #486) is the
complementary surface: it evaluates — and optionally declares — `AletheiaDB`
per-label schema constraints, which are enforced at the store's own pre-apply
commit hook. The two are not substitutes: `eg validate` checks reference closure
across a whole graph, which a per-label constraint cannot express.
