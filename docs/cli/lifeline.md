# eg query lifeline

Trace **one symbol's full evolution timeline across commit history** (issues
#96, #215) — answer the pre-edit question *"when was this function introduced,
which commits changed it and by how much, and was it ever removed?"* — from a
`scan-history` graph or an embedded store. Local-first; no network access,
hosted indexing, remote crawling, or mandatory remote embeddings.

> **Events are advisory temporal facts — "where and when this symbol
> changed" — never a truth or risk claim.** A high-churn lifeline does not
> mean the symbol is buggy or risky, and no event asserts that a change broke
> or preserved behavior.

## Synopsis

```text
eg query lifeline <SYMBOL> --graph <PATH>    [--repo <SELECTOR>] [--format json|text]
eg query lifeline <SYMBOL> --data-dir <DIR>  [--repo <SELECTOR>] [--format json|text]
```

`<SYMBOL>` is a stable symbol record ID or an exact symbol name. The query is
purely read-time: it reads only the supplied store, never Git state, so it
cannot mutate the working tree. (The store itself is produced by
`eg scan-history`, which reads Git objects only and leaves the checkout
byte-for-byte unchanged.)

## Shortest offline workflow

```sh
# Replay history into a temporal JSONL graph (reads Git objects only)
eg scan-history . --out history.graph.jsonl

# One symbol's full lifeline, chronologically ordered
eg query lifeline parse_header --graph history.graph.jsonl

# Scope an ambiguous name to one repository in a shared store
eg query lifeline parse_header --graph history.graph.jsonl --repo acme/widget

# Human-readable timeline
eg query lifeline parse_header --graph history.graph.jsonl --format text
```

## Event kinds

The lifeline is an ordered series of events for one logical symbol identity
(the stable-ID contract, ADR-0004): edits that preserve the symbol's identity
appear as `modified` events on one lifeline, never as separate
introduce/remove pairs.

| Kind | Meaning |
|------|---------|
| `introduced` | First commit in which the symbol is seen |
| `modified` | The symbol's recorded body changed, or a semantic-drift record exists for the step |
| `removed` | The symbol became absent (tombstoned) after being live at a parent commit |
| `reintroduced` | The symbol came back after a removal |

The first event is always `introduced`; if the symbol was tombstoned, the
lifeline ends with `removed`. A symbol present in only one commit returns a
single `introduced` event, not an error. Commits in which the symbol exists
but did not change produce no event.

## Output

Newline-delimited JSON by default: one standalone event object per line, in
chronological (deterministic topological commit) order. Re-running the query
against an unchanged store yields byte-for-byte identical output.

```json
{"event_type":"introduced","record_id":"codegraph:v5:...","commit":"4f0c2b1...","valid_time":"2026-01-01T00:00:00Z","repo_relative_path":"src/parse.rs","span":{"start_byte":120,"end_byte":540,"start_line":9,"end_line":24}}
{"event_type":"modified","record_id":"codegraph:v5:...","commit":"9ad3e77...","valid_time":"2026-01-03T00:00:00Z","repo_relative_path":"src/parse.rs","span":{"start_byte":120,"end_byte":610,"start_line":9,"end_line":27},"drift_record_id":"semantic:v1:...","drift_score":0.31}
{"event_type":"removed","record_id":"codegraph:v5:...","commit":"b77e102...","valid_time":"2026-01-05T00:00:00Z","absent_span_reason":"tombstone"}
```

Fields per event:

- `event_type` — `introduced` | `modified` | `removed` | `reintroduced`.
- `record_id` — the stable symbol record ID (the tombstone record ID for a
  `removed` event), so every row is citable.
- `commit` — the Git commit SHA of the event.
- `valid_time` — the commit (valid) time of the event's commit.
- `repo_relative_path` + `span` — the file/span handle at that commit;
  absent on removal events, with the documented `absent_span_reason`
  (`tombstone`, or `no_span_module_level` for module-level symbols).
- `drift_record_id` + `drift_score` — the `SemanticDrift` record for a
  modifying step **when one exists**. An event with no drift record omits
  both fields (drift-absent), never fabricating a `0` score.

Output is redaction-safe: record IDs, commit handles, repo-relative
file/spans, valid times, drift scores, and reasons only — never raw source
text, patch hunks, or commit-message bodies.

`--format text` prints a human-readable timeline instead, one event per line:

```text
Advisory temporal facts: where and when this symbol changed
[introduced] commit=4f0c2b1 valid_time=2026-01-01T00:00:00Z record_id=codegraph:v5:... @ src/parse.rs:9-24 drift=absent
[modified] commit=9ad3e77 valid_time=2026-01-03T00:00:00Z record_id=codegraph:v5:... @ src/parse.rs:9-27 drift=0.3100 (semantic:v1:...)
```

## Exit codes and diagnostics

Failures print a stable machine-readable diagnostic
(`{"ok":false,"error":{"code":...}}` on stdout in JSON mode), never a panic
or partial output:

| Condition | `code` | Exit |
|-----------|--------|------|
| At least one event returned | — | `0` |
| Symbol not found in the graph | `unknown_symbol` | `2` |
| Symbol matched but has zero commit-linked history records | `no_history` | `2` |
| Name matches multiple symbols (all candidate record IDs listed) | `ambiguous_symbol` | `6` |

Exit `2` is the established no-result exit code, consistent with
`eg query symbol`. Ambiguous names are reported with every candidate record
ID rather than silently picking one; re-run with the stable record ID or a
`--repo` scope to disambiguate.

## When to use which tool

- **`eg query lifeline`** — start from one symbol, want its whole history:
  introduction, every real change with its drift score, removal,
  reintroduction. Code facts only — no agent observations or task state.
- **`eg query deltas <BASE> <HEAD>` (issue #118)** — you already know the
  commits and want the delta set of the bounded range across the whole tree.
- **`eg query change-impact` (issue #76)** — spatial, present-tense
  blast-radius leads around a handle; no time axis.
- **`eg query symbol <NAME> --at <COMMIT>` / `--as-of <INSTANT>`** — one
  frozen snapshot of the symbol at one point in time, not its trajectory.
- **`eg query symbol --tx-as-of` (issue #66)** — what the *store* knew at a
  transaction instant, an orthogonal axis.
- **`eg query drift`** — whole-graph drift ranking, not one symbol's series.
- **`git log -S <name>` / `git log -L` (pickaxe)** — text-based: it counts
  comments, string literals, and every same-name symbol alike, and returns
  raw diffs with no drift score and no citable record handles. The lifeline
  tracks the typed symbol identity, so an unrelated same-name symbol never
  bleeds into the answer.
- **`git blame`** — line-level authorship of the current snapshot, not a
  symbol's event series over time.
- **rust-analyzer / LSP** — HEAD-only navigation; no temporal axis.
## Corpus scope

This is a history-analysis lane: it reads the **union of all commit snapshots**
by design and carries no `--at-head`/`--all-history` corpus flags. The summary
envelope discloses `corpus_mode: "union"` (or `single_snapshot` over a
snapshot-less store), `corpus_mode_source`, and `corpus_disclaimer` for
transparency. See [Corpus scope for query lanes](corpus-modes.md).
