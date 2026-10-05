# eg query complexity

Rank Rust callable symbols by deterministic structural complexity, highest first (issue #162).

Every Rust callable (`function` / `method` / `test` symbol kind) carries a
source-derived integer `complexity`: **1 plus one per decision point in the
item's own body**. The score is a code fact — never agent confidence — and it
is the complexity half of the CodeScene-style hotspot pair whose other half is
[`eg query churn`](churn.md): churn says *where* change happens, complexity
says *where* the risk concentrates. Multiplying the two into one fused
hotspot score is out of scope for this slice; the multiplicands are both
available, the fusion is not.

## Synopsis

```text
eg query complexity --graph <PATH>    [--repo <SELECTOR>] [--limit N] [--format json|text]
eg query complexity --data-dir <DIR>  [--repo <SELECTOR>] [--limit N] [--format json|text]
```

```sh
eg scan . --out graph.jsonl
eg query complexity --graph graph.jsonl
eg query complexity --graph graph.jsonl --limit 10 --format text
```

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded`. Providing both `--graph` and `--data-dir` is an error. |
| `--repo <SELECTOR>` | no | Restrict the ranking to one repository (see [Repository scope](query.md#repository-scope---repo-issue-67)). Unknown or ambiguous selectors exit `1` with the standard machine-readable stderr diagnostic. |
| `--limit N` | no | Maximum ranked symbols returned. **Default `50`, maximum `500`.** Values outside `1..=500` are rejected with an `invalid_limit` diagnostic on stderr and exit `1`. |
| `--format` | no | `json` (default) or `text`. |

## What counts as complexity

`complexity = 1 + (decision points in the item's own body)`, where each of
the following counts one:

- `if` / `else if`
- `for`
- `while`
- `loop`
- each `match` arm
- each `?`
- each `&&`
- each `||`

Rules:

- Closure bodies count toward the enclosing callable.
- Nested `fn` items get their own symbol; their bodies do not count toward the
  enclosing callable.
- Signature-only trait methods score `1` (the documented minimum — no body, no
  decision points).
- Only Rust `Symbol` nodes of kind `function`, `method`, or `test` carry a
  score. Other languages' callables are outside the metric's domain.

## Ordering (deterministic)

Rows are sorted by:

1. `complexity` descending;
2. qualified symbol name ascending — the documented stable tie-break;
3. `symbol_record_id` ascending (only reachable for identical qualified names
   across repositories in an unscoped multi-repository store).

The full ranking is byte-identical across repeated runs on an unchanged store:
LF and CRLF sources score identically, and two checkouts of the same tree
produce identical `(complexity, name, rank)` projections. All paths in the
output are repository-relative (`/` separators); no absolute checkout prefix
ever appears.

## Output

`--format json` (default) prints **one JSON envelope on one line**, keeping
the one-JSON-object-per-line contract of [`query.md`](query.md):

```json
{"ok":true,"result":{
  "ranking_basis":"structural_complexity",
  "tie_break":"qualified_name",
  "limit":50,
  "total_symbol_count":8,
  "returned_symbol_count":8,
  "truncated":false,
  "corpus_mode":"single_snapshot",
  "symbols":[
    {"rank":1,"name":"gnarly","symbol_kind":"function","complexity":11,
     "symbol_record_id":"…","schema_version":11,"repo_relative_path":"src/lib.rs",
     "span":{"start_line":10,"start_byte":120,"end_line":40,"end_byte":900},
     "repository_id":"repo:abc","repository":"egregore"}
  ]
}}
```

Every row is a stable, citable graph handle an agent can pivot on with
`eg query symbol`, `eg query file`, or `eg query callers`. The `complexity`
field is also exposed on every `eg query symbol` row for callable symbols;
it is omitted for non-callables and for records produced before issue #162 —
a missing field means *unknown/inapplicable*, never zero.

`--format text` prints one ranked line per symbol plus an explicit truncation
notice when `--limit` cut the ranking:

```text
Symbol complexity ranking: structural complexity (1 + decision points), highest first
1. gnarly [function] complexity=11 (codegraph:v11:…)
2. chain_6 [function] complexity=7 (codegraph:v11:…)
truncated: showing 50 of 213 symbols (raise --limit, max 500)
corpus: single_snapshot
```

## Exit codes

- `0` — ranking returned (possibly truncated; see the `truncated` flag).
- `1` — load error, `--limit` outside `1..=500`, or unknown/ambiguous
  `--repo` selector.
- `2` — no scored callable symbols in scope (`no_match` envelope).

## Determinism

The metric is computed from the Tree-sitter parse of the source at scan
time, so it is stable across runs, machines, and line-ending styles. The
answer states explicitly whether `--limit` truncated it, and the JSON
envelope is one line so machine consumers keep the NDJSON contract.
