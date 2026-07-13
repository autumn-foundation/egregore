# eg resolve-frames

Resolve **runtime backtrace stack frames** on `ErrorSignature` records to
code-graph `Symbol` nodes, emitting `FRAME_RESOLVES_TO` edges (issue #322).

Given a log graph (from [`scan-logs`](scan-logs.md)) whose `ErrorSignature`
records carry structured backtrace frames, plus a code graph (from
[`scan`](../../CLAUDE.md)), this command binds each frame to a code-graph target
and mints one `FRAME_RESOLVES_TO` edge per bound frame — each labeled with a
closed-set `frame_resolution` and a zero-based `frame_index`, and mirrored by an
`EvidenceLink` on the `ErrorSignature`.

> **A binding proves the frame NAMES the symbol — never that the symbol is at
> fault.** Resolution is deterministic span/name matching over already-extracted
> facts. It is not proof that the symbol caused the error, that it must change,
> or that it is buggy. A frame handle is never silently bound to an invented
> target.

## Synopsis

```text
eg resolve-frames <LOG_GRAPH> --graph <CODE_GRAPH> --out <OUT.jsonl> [--at <SHA> | --as-of <RFC3339>]
eg resolve-frames --data-dir <DIR> --out <OUT.jsonl> [--at <SHA> | --as-of <RFC3339>]
```

- `<LOG_GRAPH>` — the log graph JSONL (positional). Omit with `--data-dir`.
- `--graph <CODE_GRAPH>` — the code graph JSONL. Required with the positional
  log graph; mutually exclusive with `--data-dir`.
- `--data-dir <DIR>` — an embedded store holding both graphs.
- `--out <OUT.jsonl>` — output JSONL for the enriched records.
- `--at <SHA>` / `--as-of <RFC3339>` — resolve against the code-graph state at a
  commit or valid-time instant (requires a history graph). Mutually exclusive;
  passing both prints an `unsupported_combination` diagnostic and exits 1.

## Resolution ladder

Applied per frame, in order. Mirrors the `CallResolution` precedent
(issues #152/#134): ambiguity enumerates **all** candidates and never silently
picks one; an absent target becomes a `Diagnostic` marker, never an invented
symbol.

1. **Frame `file:line`** → the smallest enclosing `Symbol` (the #151
   span-containment resolver). A hit is **`resolved`** (edge → that symbol). A
   file that exists with no enclosing symbol (optimized-out / macro-generated
   frame) is **`path_only`** (edge → the `File` node).
2. **Module-path name only** (no usable `file:line`) → exact `Symbol`-name
   lookup. Exactly one match is **`resolved`**; two or more is **`ambiguous`**,
   and *every* candidate gets its own edge.
3. **A repo-relative path absent from the resolved view** (deleted / renamed
   since the log) is **`unresolved`** → the edge targets a `Diagnostic` node
   carrying the redacted frame text.
4. **A standard-library or dependency frame** (module root `std`/`core`/`alloc`
   /`proc_macro`/`test`/`backtrace`, or a file under `/rustc/`, `/registry/`,
   `.cargo/`, `.rustup/`) is **`external`**: counted in a per-signature tally and
   minting **no** edge.

`frame_resolution` is a **closed, stable** on-edge set:
`{resolved, ambiguous, path_only, unresolved}`. `external` is a tally class, not
an edge label.

## Output

`--out` receives the enriched **log-domain** records only (updated
`ErrorSignature` nodes, the original log records, new `Diagnostic` markers, and
new `FRAME_RESOLVES_TO` edges) — the code graph is never re-emitted. Records are
canonically ordered and byte-identical across runs.

stdout carries a deterministic summary envelope:

```json
{"ok":true,"command":"resolve-frames","at_commit":null,"as_of":null,
 "totals":{"signatures_with_frames":1,"resolved":1,"ambiguous":1,"path_only":1,"unresolved":1,"external":1},
 "signatures":[{"signature_id":"log:v2:…","resolved":1,"ambiguous":1,"path_only":1,"unresolved":1,"external":1}],
 "disclaimer":"A frame binding proves that the backtrace frame NAMES the symbol; …"}
```

## Determinism & redaction-safety

Output is byte-identical across runs on unchanged inputs: nodes sort by record
ID, edges by `(source, frame_index, target, id)`, and per-signature tallies by
signature ID. Raw log payload text never enters the output — frame text is the
scan-time redaction-normalized form, and the `unresolved` `Diagnostic` carries
only that redaction-safe text.

## Exit codes

- `0` — resolution completed (including an all-external or empty-frame run).
- `1` — `--at` combined with `--as-of` (`unsupported_combination`), or a
  malformed temporal selector.
- `2` — a temporal pin (`--at`/`--as-of`) names no resolvable commit.

## Honest limits

- Resolution proves the frame **names** the symbol, never that the symbol is at
  fault or caused the error.
- `external` is a coarse toolchain classification, not proof the fault lies
  outside your code.
- A frame's file/line is the runtime's own claim; matching it to a span is not
  verification of the runtime behavior.
