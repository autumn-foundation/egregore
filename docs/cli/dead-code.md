# eg query dead-code

List the indexed symbols that have **no live in-graph callers** — zero
recorded direct incoming `CALLS` edges — as dead-code triage candidates
with citable handles. Local-first, no network, no build, no per-symbol
grepping.

> **Candidates, not proof.** A row is *suspected* dead code, never proven
> dead. The graph cannot observe dynamic dispatch via trait objects,
> macro-generated callers, reflection-like usage, FFI consumers, or
> cross-crate consumers of this crate's items — any of those can keep a
> listed symbol live without a recorded in-graph `CALLS` edge. Deletion
> stays a human/agent decision; every response carries this soundness
> boundary in its metadata.

## Synopsis

```text
eg query dead-code --graph <PATH>    [--repo <SELECTOR>] [--limit N] [--format json|text]
eg query dead-code --data-dir <DIR>  [--repo <SELECTOR>] [--limit N] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the candidate set to one
repository in a multi-repo store; an unknown or ambiguous selector is
rejected with a machine-readable stderr diagnostic (exit 1), never resolved
implicitly. Strictly read-only: no records, indexes, or runtime files are
created, modified, or deleted.

| Condition | Exit | Output |
|-----------|------|--------|
| Candidate set computed — including an **empty** one | `0` | JSON (or text) on stdout, `ok:true` |
| `--limit` outside 1..=500 | `1` | `{"code":"invalid_limit",...}` on stderr |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input | `1` | Error message on stderr |

An empty candidate set is a distinct, documented signal — never conflated
with a store-absent, no-match, or incomplete-extraction condition:

- Symbols exist and none qualify → `ok:true`, empty `candidates`, a
  `no_candidates` diagnostic, exit 0.
- The store holds no live code `Symbol` records at all → `ok:true`, empty
  `candidates`, a `no_symbols` diagnostic, exit 0.
- The store path is missing or unreadable → exit 1 with a stderr error.

## What counts as unreferenced

A live `Symbol` record is a candidate when either

- **(a)** it has **zero recorded direct incoming `CALLS` edges**, or
- **(b)** every recorded direct caller is itself a rule-(a) candidate — the
  one-hop dead-cluster rule: if `a` is called only by `b` and `b` is
  unreferenced, both `a` and `b` are reported. Rule (b) is deliberately not
  recursive: a three-deep chain `c → b → a` reports `c` and `b` but not `a`.
  The lane keys on direct incoming edges; transitive whole-program
  reachability pruning (mark-and-sweep from public roots) is out of scope.

`impl`-block symbols are excluded from the candidate population — they are
unnameable declaration details, so a zero inbound count carries no pruning
signal (their methods are considered individually). Ambiguous call edges
count as references: a symbol that *might* be called is never reported.
Unresolved call edges (no in-repo target matched) target a `Diagnostic`
marker, not a symbol; when any exist the response carries an
`unresolved_call_edges_present` diagnostic with the count, attributed
through the source symbol's repository so a `--repo`-scoped run tallies
only the scoped repository's unresolved calls.

## Excluded from candidacy (counted, never silently dropped)

Excluded symbols are tallied in `counts.entry_points_excluded` and
`counts.public_surface_excluded` — they never appear as rows, and the
tallies prove the exclusion ran.

- **Recognized non-call entry points** (`entry_points_excluded`): the
  extractor's `entry_point` mark on the symbol — `#[test]` / `#[bench]`
  harness entries, `#[no_mangle]` / `#[export_name]` FFI exports, and a free
  `fn main` in a binary crate root (`src/main.rs`, `src/bin/**`) — plus
  `symbol_kind == "test"` so graphs scanned before issue #240 get the same
  exclusion.
- **Externally reachable symbols** (`public_surface_excluded`): the issue
  #213 public-surface reachability rule — top-level `pub` and
  visibility-widening `pub use` re-exports (both the declaring row and the
  resolved re-export target). An unused-internally `pub fn` may be a real
  external entry point, so the lane excludes it by default rather than
  trusting a noisy `pub` grep.

Entry-point and public-surface exclusions also gate rule (b): a symbol
called only by `fn main`, a `#[test]`, or a `pub` entry point is live —
its caller is a real (if unrecorded-caller) entry into the code.

## Output shape

Deterministic, byte-identical across repeated runs on an unchanged store.
Candidates are sorted by (`repo_relative_path`, `span.start_line`,
`record_id`) and truncated to `--limit` (default 100, max 500); tallies in
`counts` are totals, unaffected by the limit.

```json
{
  "ok": true,
  "lane": "dead-code",
  "repo_scope": "example",
  "classification": "candidate",
  "soundness": "Candidates are suspected dead code, not proven-dead. ...",
  "candidates": [
    {
      "record_id": "codegraph:v4:...",
      "schema_version": 4,
      "name": "orphan",
      "kind": "function",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 120, "end_byte": 180, "start_line": 12, "end_line": 14 },
      "classification": "candidate",
      "evidence": {
        "incoming_calls": 0,
        "callers_all_candidates": false,
        "on_public_surface": false
      }
    },
    {
      "record_id": "codegraph:v4:...",
      "schema_version": 4,
      "name": "helper",
      "kind": "function",
      "repo_relative_path": "src/lib.rs",
      "span": { "start_byte": 200, "end_byte": 240, "start_line": 20, "end_line": 22 },
      "classification": "candidate",
      "evidence": {
        "incoming_calls": 1,
        "callers_all_candidates": true,
        "on_public_surface": false
      }
    }
  ],
  "counts": {
    "symbols_considered": 9,
    "referenced": 5,
    "candidates": 2,
    "candidates_returned": 2,
    "entry_points_excluded": 1,
    "public_surface_excluded": 1
  },
  "diagnostics": []
}
```

Every candidate carries a stable `record_id`, `schema_version`, `name`,
`kind`, a repo-relative file/span handle, `git_commit` for temporal
records, and the concrete evidence asserted: `incoming_calls` (recorded
direct incoming `CALLS` edges), `callers_all_candidates` (true only for
rule-(b) rows, whose recorded callers are all themselves candidates), and
`on_public_surface` (always `false` — the asserted exclusion). `--format
text` renders one citable line per candidate plus the honesty header.

Output is redaction-safe: record IDs, names, kinds, paths, spans, counts,
diagnostic handles, and evidence flags only — never raw source text,
transcript text, command output, patch hunks, or env values.

## Shortest offline workflow

```sh
eg scan . --out graph.jsonl
eg query dead-code --graph graph.jsonl
```

## When to use which

| Question | Use |
|----------|-----|
| "What can I *consider* deleting?" — dead-code triage with entry-point and public-surface filtering | `eg query dead-code` (this page) |
| "Which symbols have no recorded references of any kind?" — broader prune-triage leads (any edge class counts) | `eg query unreferenced` ([unreferenced.md](unreferenced.md), issue #113) |
| "What is most load-bearing here?" — most-referenced orientation | `eg query orient` (top referenced symbols, issue #95) |
| "What would break if I change *this*?" — blast radius from a known handle | `eg query change-impact` ([change-impact.md](change-impact.md), issue #76) |
| "Is this item provably unused within the crate?" | rustc's `dead_code` lint — sound for private items in a compiling crate, but compiler-bound, silent on `pub` items, and not a durable, citable, whole-repo candidate set |

## Compared to the boring alternatives

- **`cargo build`'s `dead_code` lint**: only fires on items that are both
  private *and* truly unused by the compiler's view, silently misses `pub`
  items with no internal callers, requires a successful build, and emits
  warnings to stderr rather than a citable, queryable set.
- **`rg "fn foo"` then manual reference grep**: O(symbols) manual searches,
  no module-reachability awareness, no public-surface filter, no citable
  handle.
- **`eg query unreferenced`**: the sibling lane — zero inbound edges of *any*
  recorded reference class, with a disclaimer about the false-positive
  classes instead of exclusions. This lane keys specifically on `CALLS`
  edges and *excludes* entry points and the public surface by default.

## Out of scope (this slice)

Transitive / whole-program reachability pruning from public roots
(mark-and-sweep), automatic deletion or codemod application, cross-crate
consumer analysis, non-Rust languages, recency/dormancy ranking (issue
#219), and the unsafe/unwrap/TODO/`#[allow]` debt lanes (issues
#222/#223/#218/#227) — this lane is the distinct "no references" axis.

## Corpus scope

Over a `scan-history` store this lane is **HEAD-anchored** by default: it
reports the state current at each repository's stamped HEAD commit, so an
item removed before HEAD does not appear. The summary envelope discloses
`corpus_mode` (`head_anchored`, or `single_snapshot` over a snapshot-less
store), `corpus_mode_source`, and `corpus_disclaimer`. See
[Corpus scope for query lanes](corpus-modes.md).
