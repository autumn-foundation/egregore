# eg query blind-spots

List the code targets — symbols and/or files — that **nothing outside the
code graph knows anything about**: zero recorded cross-domain evidence
edges into the agent-memory, verification, or project domains. The inverse
of every other query lane: instead of *"what do we know about this
target?"*, this lane answers *"which code do we know nothing about?"*

> **Leads, not proof.** "No recorded cross-domain evidence" is **not**
> evidence the code is unimportant, untested, or unsafe — and absence of
> evidence is never evidence of correctness. Every row is a target to
> *triage*: the silent regions of the graph where surprises hide. The
> decision about what to do with a blind spot stays with you.

## Synopsis

```text
eg query blind-spots --graph <PATH>    [--repo <SELECTOR>] [--kind symbol|file|both] [--format json|text]
eg query blind-spots --data-dir <DIR>  [--repo <SELECTOR>] [--kind symbol|file|both] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the evaluated targets to one
repository in a multi-repo store; an unknown or ambiguous selector is
rejected with a machine-readable stderr diagnostic (exit 1), never resolved
implicitly. `--kind symbol|file` restricts the evaluated population
(default: both). `--format text` renders one line per blind spot instead of
JSON. Strictly read-only: no records, indexes, or runtime files are created,
modified, or deleted.

| Condition | Exit | Output |
|-----------|------|--------|
| Blind-spot set computed — including an **empty** one | `0` | JSON on stdout, `ok:true` |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |

An empty blind-spot set is a distinct, documented signal — never conflated
with a store-absent, no-match, or incomplete-extraction condition:

- Targets exist and every one carries evidence → `ok:true`, explicit empty
  `blind_spots: []`, a `no_blind_spots` diagnostic, exit 0.
- The store holds no live code `Symbol`/`File` records at all → `ok:true`,
  empty `blind_spots`, a `no_targets` diagnostic, exit 0.
- The store path is missing or unreadable → exit 1 with a stderr error.

## What counts as evidence

"Evidence" is defined in terms of the existing cross-domain edge
vocabulary — no new node kinds, edge labels, or domains. A code target
(a `Symbol` or `File` node) **has evidence** when at least one live,
current-state edge satisfies all three of:

1. **Label** — the edge label is one of the five evidence classes:
   - `OBSERVES` — an agent-memory `Observation` observes the target.
   - `MENTIONS_SYMBOL` — an agent-memory, verification, or project node
     names the symbol.
   - `TOUCHED_FILE` — an agent-memory or verification node (`FileEdit`,
     `ToolCall`, `CommandRun`, `TestRun`, `CIStatus`) cites the file.
   - `FAILED_ON` — an agent-memory or verification node (`Failure`,
     `TestRun`, `CIStatus`) records a failure on the target.
   - `TOUCHES_FILE` — a project `Task` (or `Review`) intends to touch the
     file.
2. **Direction** — the edge targets the code node (schema direction is
   always other-domain → code; see `docs/schema/agent-memory.md` §6a).
3. **Source domain** — the edge's source node is a live record whose node
   kind classifies into the label's documented FROM domain(s)
   (`agent_memory`, `verification`, or `project`). A label alone is never
   trusted: a malformed code-internal edge carrying a cross-domain label
   cannot fake evidence. An evidence-labeled edge whose source record is
   absent or tombstoned, or whose kind falls outside the label's documented
   FROM domain, is not counted and raises the
   `unattributed_evidence_edges_present` diagnostic (with the count) rather
   than silently hiding a blind spot.

Deliberately **not** evidence:

- Structural reference edges (`CALLS`, `IMPORTS`, `MENTIONS`, `REFERENCES`,
  `IMPLEMENTS`, `REGISTERS_ROUTE`): being called is usage, not knowledge.
  These edges drive the *prominence ranking* instead.
- `EXPLAINS_CHANGE`: targets `Commit`/`Change` nodes, never `Symbol` or
  `File`, so it can never attach to this lane's targets.
- `HAS_EVIDENCE` / `VALIDATED_BY`: terminate at verification sinks, not at
  code targets (whether a target *is verified* is the
  verification-coverage lane's question, not this lane's).
- Generic `RELATES_TO` / `CONTRADICTS` / `SUPERSEDES` links: permitted
  cross-domain links, but not *evidence* in the documented sense. Counting
  them would let this lane's meaning drift silently — this definition is
  pinned so it cannot.

**No inheritance.** Evidence does not flow between a file and the symbols
it contains: a `TOUCHED_FILE` edge evidences the file, not its symbols,
and a `MENTIONS_SYMBOL` edge evidences the symbol, not its file. Each
target stands on its own edges — a file nobody ever touched is a blind
spot even when every symbol inside it is well-evidenced.

## Scope

Live `Symbol` records (impl blocks excluded — they are unnameable
declaration details; their methods are considered individually) and live
`File` records at the **current** graph state. Tombstoned targets and
tombstoned evidence sources are excluded, in parity with the other
current-state lanes. On `scan-history` graphs, only records and edges
valid at the repository's stamped snapshot HEAD commit shape the answer:
an evidence edge that existed in an older commit but was removed before
HEAD does **not** disqualify its target, and a target absent at HEAD is
deleted, not a blind spot. Snapshot-less stores (pre-#186 graphs) fall back
conservatively — the latest record per stable ID wins and every recorded
edge counts. A `--repo`-scoped run evaluates only the scoped repository's
targets; evidence edges are attributed by label and source domain, not by
source repository (an evidence link to the target counts wherever it was
recorded).

## Ranking

Rows are ranked **riskiest first**: `inbound_reference_count` — the number
of inbound edges of the structural reference classes (`CALLS`,
`IMPLEMENTS`, `IMPORTS`, `MENTIONS`, `REFERENCES`, `REGISTERS_ROUTE`) —
descending. A widely-referenced symbol nobody has ever recorded anything
about outranks an isolated one. The ranking key is included on every row.
Ties break deterministically by (`repo_relative_path`, `span.start_line`,
`record_id`); output is byte-identical across repeated runs on an
unchanged store.

## Output shape

```json
{
  "ok": true,
  "disclaimer": "Code targets with zero recorded cross-domain evidence edges ... not verdicts ...",
  "evidence_edge_classes": ["FAILED_ON", "MENTIONS_SYMBOL", "OBSERVES", "TOUCHED_FILE", "TOUCHES_FILE"],
  "blind_spots": [
    {
      "record_id": "codegraph:v6:...",
      "schema_version": 6,
      "target_kind": "symbol",
      "name": "helper",
      "kind": "function",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 120, "end_byte": 180, "start_line": 12, "end_line": 14 },
      "inbound_reference_count": 3
    },
    {
      "record_id": "codegraph:v6:...",
      "schema_version": 6,
      "target_kind": "file",
      "name": "src/extra.rs",
      "kind": "file",
      "repo_relative_path": "src/extra.rs",
      "inbound_reference_count": 0
    }
  ],
  "counts": {
    "targets_evaluated": 42,
    "targets_with_evidence": 35,
    "blind_spots": 7
  },
  "diagnostics": []
}
```

Every row carries a stable `record_id`, `schema_version`, `target_kind`
(`symbol`/`file`), `name`, `kind`, a repo-relative file/span handle
(`git_commit` for temporal records), and the `inbound_reference_count`
ranking key. `counts.targets_evaluated` keeps coverage interpretable: it is
reported even when the blind-spot set is empty. Output is redaction-safe:
record IDs, names, kinds, paths, spans, counts, and diagnostic handles
only — never raw source text, transcript text, command output, patch
hunks, or env values.

`--format text` renders the same answer human-readable: a one-line
summary, then one line per blind spot (`- <name> [<kind>] @
<path>:<line> refs=<n> (<record_id>)`, file rows omit the span), then any
diagnostics.

## Shortest offline workflow

```sh
eg scan . --out graph.jsonl
eg query blind-spots --graph graph.jsonl
```

## When to use which

| Question | Use |
|----------|-----|
| "Which code does the graph know *nothing* about?" — evidence-absence triage | `eg query blind-spots` (this page) |
| "What can I *consider* deleting?" — no inbound references | `eg query unreferenced` ([unreferenced.md](unreferenced.md), issue #113) |
| "What do we know about *this* target?" — evidence-backed context | `eg query context` / `eg query symbol` |
| "Is this target covered by tests/proofs?" — verification state | `eg query verification-coverage` |
| "Does this name appear anywhere as text?" | `rg <name>` — but it matches comments and strings, returns raw lines instead of typed records, and cannot reason over typed cross-domain edges |

## Out of scope (this slice)

Suggesting or generating the missing evidence (no auto-authored
observations, no "fill the gap" prose); quality scoring of *existing*
evidence — verified vs stale is the freshness lanes' job (issues #243,
#263, evidence-freshness); parser/extraction coverage gaps (#246) and
structurally dead code (#240) — this lane is about cross-domain
*knowledge* absence, not parse failure or unreferenced symbols; test
coverage percentages or line-level instrumentation; and daemon/MCP verbs.

## Corpus scope

Over a `scan-history` store this lane is **HEAD-anchored** by default: it
reports the state current at each repository's stamped HEAD commit, so an
item removed before HEAD does not appear. The summary envelope discloses
`corpus_mode` (`head_anchored`, or `single_snapshot` over a snapshot-less
store), `corpus_mode_source`, and `corpus_disclaimer`. See
[Corpus scope for query lanes](corpus-modes.md).
