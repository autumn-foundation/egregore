# eg query unreferenced

List the code symbols that **nothing in this graph references** — zero
recorded inbound reference edges — as prune-triage candidates with citable
handles. Local-first, no network, no build, no per-symbol grepping.

> **Leads, not proof.** "No recorded inbound reference" is **not** proof a
> symbol is dead. The graph cannot see: public API consumed outside this
> repository, trait-method dynamic dispatch, macro-generated call sites,
> FFI / `#[no_mangle]` / `#[export_name]` consumers, derive-generated use, or
> crate entry points (`main`, `#[test]`). Every row is a candidate to
> *inspect* for removal — the decision stays with you.

## Synopsis

```text
eg query unreferenced --graph <PATH>    [--repo <SELECTOR>]
eg query unreferenced --data-dir <DIR>  [--repo <SELECTOR>]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the candidate set to one
repository in a multi-repo store; an unknown or ambiguous selector is
rejected with a machine-readable stderr diagnostic (exit 1), never resolved
implicitly. Strictly read-only: no records, indexes, or runtime files are
created, modified, or deleted.

| Condition | Exit | Output |
|-----------|------|--------|
| Candidate set computed — including an **empty** one | `0` | JSON on stdout, `ok:true` |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |

An empty candidate set is a distinct, documented signal — never conflated
with a store-absent, no-match, or incomplete-extraction condition:

- Symbols exist and every one is referenced → `ok:true`, empty `candidates`,
  a `no_candidates` diagnostic, exit 0.
- The store holds no live code `Symbol` records at all → `ok:true`, empty
  `candidates`, a `no_symbols` diagnostic, exit 0.
- The store path is missing or unreadable → exit 1 with a stderr error.

## What counts as a reference

"Reference" is defined in terms of the existing edge vocabulary — no new
node kinds, edge labels, or domains:

- **Counted** (inbound, targeting the symbol): `CALLS`, `IMPORTS`,
  `MENTIONS` — the documented reference classes — plus the extractor's other
  recorded usage edges `REFERENCES` (identifier use that is not call-shaped,
  e.g. a type named in a body or signature) and `IMPLEMENTS` (an `impl`
  block binding to its trait or type). Excluding recorded usage would flatly
  misreport every used-but-never-called type as unreferenced.
- **Never counted**: the structural `DEFINES` / `CONTAINS` edge from a
  symbol's own file or module — every symbol has one, so it carries no usage
  signal. Agent-memory `MENTIONS_SYMBOL` edges are never counted either: an
  agent-authored observation is not a code fact (trust separation).
- **Ambiguous call edges count as references**: when a call site matches
  several definitions, every candidate target carries an edge — a symbol
  that *might* be called is never reported as unreferenced.
- **Unresolved call edges** (no in-repo definition matched; issue #152)
  target a `Diagnostic` marker, not a symbol. When any exist, the response
  carries an `unresolved_call_edges_present` diagnostic with the count: an
  unrecorded reference to a listed candidate may exist.

Scope: live `Symbol` records at the **current** graph state. Tombstoned
(deleted) symbols are excluded, in parity with `eg query file` and
`eg query symbols`. On `scan-history` graphs, only records valid at the
repository's stamped snapshot HEAD commit shape the answer (the
`resolve_head_symbols` rule): a call edge that existed in an older commit
but was removed before HEAD does **not** mark its target as referenced, and
a symbol absent at HEAD is deleted, not a candidate. Temporal candidates
carry their `git_commit`; snapshot-less stores (pre-#186 graphs) fall back
conservatively — the latest record per stable ID wins and every recorded
edge counts. `impl`-block symbols are excluded from the candidate
population — they are unnameable declaration details, so a zero inbound
count carries no pruning signal (their methods are considered
individually).

## Extraction-completeness caveat (issue #87)

A candidate whose file scope contains extractor `Diagnostic` markers
(unparsed macro invocations, unresolved call targets) carries an
`extraction_caveat`: a macro-hidden reference may exist in that scope, so
the candidate's confidence is lower. The caveat is **advisory** — it cites
the marker records and never rewrites or hides the code fact.

Markers are matched to candidates by file path. Under `--repo` in a
multi-repo store, each marker is attributed to its producing repository by
recomputing the extractor's stable-ID schemes, so a marker from another
repository that happens to share the same repo-relative path never caveats
the scoped repository's candidates — and the scoped repository always keeps
its own. A marker with an unrecognized ID scheme falls back to the
path-owner rule (kept when its path is recorded by the scoped repository):
conservative, because dropping a real marker would hide lower extraction
confidence.

## Output shape

Deterministic, byte-identical across repeated runs on an unchanged store.
Candidates are sorted by (`repo_relative_path`, `span.start_line`,
`record_id`).

```json
{
  "ok": true,
  "disclaimer": "Symbols with zero recorded inbound reference edges ... not proof of dead code ...",
  "reference_edge_classes": ["CALLS", "IMPLEMENTS", "IMPORTS", "MENTIONS", "REFERENCES"],
  "candidates": [
    {
      "record_id": "codegraph:v4:...",
      "schema_version": 4,
      "name": "orphan",
      "kind": "function",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 120, "end_byte": 180, "start_line": 12, "end_line": 14 },
      "inbound_reference_count": 0
    },
    {
      "record_id": "codegraph:v4:...",
      "schema_version": 4,
      "name": "macros::macro_neighbor",
      "kind": "function",
      "repo_relative_path": "src/macros.rs",
      "span": { "start_byte": 0, "end_byte": 60, "start_line": 1, "end_line": 3 },
      "inbound_reference_count": 0,
      "extraction_caveat": {
        "code": "diagnostics_in_file_scope",
        "diagnostic_count": 2,
        "diagnostic_record_ids": ["codegraph:v4:...", "codegraph:v4:..."],
        "detail": "file scope contains 2 extraction Diagnostic marker(s); ..."
      }
    }
  ],
  "counts": {
    "symbols_considered": 9,
    "referenced": 3,
    "candidates": 6,
    "files_with_diagnostic_markers": 1
  },
  "diagnostics": []
}
```

Every candidate carries a stable `record_id`, `schema_version`, `name`,
`kind`, a repo-relative file/span handle, `git_commit` for temporal records,
and the inbound-reference count that selected it (always `0`). Output is
redaction-safe: record IDs, names, kinds, paths, spans, counts, diagnostic
handles, and caveat markers only — never raw source text, transcript text,
command output, patch hunks, or env values.

## Shortest offline workflow

```sh
eg scan . --out graph.jsonl
eg query unreferenced --graph graph.jsonl
```

## When to use which

| Question | Use |
|----------|-----|
| "What can I *consider* deleting?" — pre-delete / pruning triage, no handle yet | `eg query unreferenced` (this page) |
| "What is most load-bearing here?" — most-referenced orientation | `eg query orient` (top referenced symbols, issue #95) |
| "What would break if I change *this*?" — blast radius from a known handle | `eg query change-impact` ([change-impact.md](change-impact.md), issue #76) |
| "Was this scope fully extracted?" | `extraction_completeness` on `eg query symbol` / `file` (issue #87) |
| "Does this name appear anywhere as text?" | `rg <name>` — but it matches comments, strings, and doc examples, returns raw lines instead of typed records, has no tombstone awareness, and must run once per symbol |
| "Is this item provably unused within the crate?" | rustc's `dead_code` lint / rust-analyzer — sound for private items in a compiling crate, but compiler-bound, silent on `pub` items, and not a durable, citable, whole-repo candidate set |

## Compared to the boring alternatives

- **`rg` per symbol**: O(symbols) sweeps with comment/string false positives
  and no citable record handles. This lane answers the whole-repo question
  in one deterministic call over typed edges.
- **rustc `dead_code` / rust-analyzer "unused"**: the soundness gold
  standard inside one compiling crate, but they require a build, stay silent
  on unused `pub` items, and emit editor/stderr output rather than a
  scriptable JSONL answer with record-ID/span handles.
- **Knip / ts-prune (JS/TS analogs)**: prove the demand for a
  "candidates a human reviews" report; this is the local-first,
  trust-separated SWE variant that is explicit about the false-positive
  classes the graph cannot see.

## Out of scope (this slice)

Sound dead-code analysis and reachability proofs, transitive
entry-point reachability, public-surface exclusion (issue #240 sharpens the
triage with the issue #213 reachability rule), automatic removal, semantic
ranking, and daemon/MCP verbs (issues #59/#53).
