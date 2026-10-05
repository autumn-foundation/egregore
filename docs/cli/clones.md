# eg query clones

Group exact-duplicate Rust symbol bodies into citable clone classes (issue #216).

When a bug or refactor touches duplicated logic, the expensive failure mode is
fixing one copy and missing the others. `eg query clones` answers *"where
else does this exact body live?"* deterministically: every live Rust `Symbol`
record whose normalized body (comment-stripped, whitespace-collapsed —
the same normalization the scan pipeline stamps into symbol summaries, issue
#206) hashes to the same `blake3:` content hash forms one clone class. This is
the deterministic counterpart to the fuzzy embedding-similarity lane
(`eg query semantic`, #154): no threshold, no similarity score — a body is
either byte-identical after normalization or it is not, so no guess is ever
presented as a clone.

Each member is reported with its existing citable handle (record ID,
repo-relative file path, span, qualified name). **No raw source body is
emitted** — only the shared normalized-content hash plus bounded handles.

## Synopsis

```text
eg query clones --graph <PATH>    [--repo <SELECTOR>] [--min-size N] [--limit N] [--format json|text]
eg query clones --data-dir <DIR>  [--repo <SELECTOR>] [--min-size N] [--limit N] [--format json|text]
eg query clones --data-dir <DIR> --daemon [--repo <SELECTOR>] [--min-size N] [--limit N] [--format json|text]
```

```sh
eg scan . --out graph.jsonl
eg query clones --graph graph.jsonl
eg query clones --graph graph.jsonl --min-size 3 --limit 10 --format text
```

## Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `--graph <PATH>` | one of | Graph JSONL produced by `eg scan`. |
| `--data-dir <DIR>` | one of | Embedded `AletheiaDB` store populated by `eg ingest --adapter embedded` from a scan graph. Providing both `--graph` and `--data-dir` is an error. |
| `--daemon` | no | Route the query through the running daemon (requires `--data-dir`, conflicts with `--graph`). The daemon computes the same report via the shared query layer and the CLI prints the same envelope. |
| `--repo <SELECTOR>` | no | Restrict classes to one repository (see [Repository scope](query.md#repository-scope---repo-issue-67)). Unknown or ambiguous selectors exit `1` with the standard machine-readable stderr diagnostic. |
| `--min-size N` | no | Minimum clone-class size: only classes with at least `N` members are reported. **Default `2`.** Values below `2` are rejected with an `invalid_min_size` diagnostic on stderr and exit `1`. |
| `--limit N` | no | Maximum clone classes returned. **Default `50`, maximum `500`.** Values outside `1..=500` are rejected with an `invalid_limit` diagnostic on stderr and exit `1`. |
| `--format` | no | `json` (default) or `text`. |

## What counts as a clone class

- A clone class is the set of live Rust `Symbol` records sharing one
  normalized-body content hash (`blake3:` over `normalize_code` of the body —
  the same framing the extractor uses for `content_signature`, issue #206).
  Whitespace-only and comment-only differences do **not** split a class.
- Only `Symbol` nodes with `language == "rust"` and a recoverable normalized
  body participate (Rust only, consistent with the current extraction scope).
  Symbols whose summary carries no normalized body are skipped, never guessed.
- A class is reported only when it holds **at least `--min-size` distinct
  symbol records**; singletons never surface.
- Tombstoned symbols are excluded via latest-write-wins liveness, and a
  symbol re-scanned with an edited body contributes only its newest write —
  matching the embedded current-state read on both transports.
- In an unscoped multi-repository store, members from different repositories
  can share one class (identical bodies are identical bodies). Members emit
  only the citable handle (`record_id`, `repo_relative_path`, `span`,
  `qualified_name`); the repository boundary is visible via the
  repo-relative path, consistent with the unscoped list-query contract in
  [`query.md`](query.md).

## Ordering (deterministic)

Classes are sorted by:

1. `size` descending;
2. `content_hash` ascending — the documented stable tie-break;
3. members within a class by `record_id` ascending.

The full report is byte-identical across repeated runs on an unchanged store.

## Output

`--format json` (default) prints **one JSON envelope on one line**, keeping
the one-JSON-object-per-line contract of [`query.md`](query.md):

```json
{"ok":true,"result":{
  "min_size":2,
  "limit":50,
  "total_class_count":1,
  "returned_class_count":1,
  "truncated":false,
  "classes":[{
    "content_hash":"blake3:9f2c…",
    "size":3,
    "members":[
      {"record_id":"codegraph:v1:…","qualified_name":"a::compute","repo_relative_path":"src/a.rs","span":{"start_byte":0,"end_byte":42,"start_line":1,"end_line":4}}
    ]
  }]
}}
```

### Result fields

| Field | Type | Always present | Description |
|-------|------|----------------|-------------|
| `min_size` | number | yes | Minimum class size applied. |
| `limit` | number | yes | Class-list limit applied. |
| `total_class_count` | number | yes | Clone classes before truncation. |
| `returned_class_count` | number | yes | Clone classes returned after truncation. |
| `truncated` | boolean | yes | Completeness signal: whether `--limit` cut the class list (issue #121). Never silent. |
| `empty_reason` | string | when empty | `"no_clone_classes"` when the scope holds no clone class (issue #196). |
| `classes[]` | array | yes | Clone classes, largest first. |
| `classes[].content_hash` | string | yes | `blake3:` hash of the shared normalized body — the class identity. |
| `classes[].size` | number | yes | Class size (`== members.len()`). |
| `classes[].members[]` | array | yes | Citable member handles, `record_id` ascending. |

`--format text` prints a human-readable class listing (one class header plus
one line per member handle) and, when applicable, an explicit `truncated:
showing K of M classes` line. The text format is not stable and must not be
parsed by scripts.

## Empty vs not-found honesty (issue #196)

| Condition | Exit | Output |
|-----------|------|--------|
| Clone classes returned | `0` | Report JSON on stdout, `ok:true` |
| Scope exists but contains **zero** clone classes | `0` | `ok:true`, empty `classes`, `"empty_reason": "no_clone_classes"` |
| Unknown / ambiguous `--repo` selector | `1` | standard selector diagnostic on stderr |
| Invalid `--limit` / `--min-size` | `1` | `invalid_limit` / `invalid_min_size` diagnostic on stderr |

"Scope contains zero clone classes" and "scope not found" are distinct
machine-readable answers — the lane never conflates them.

## Exit codes

| Code | Meaning |
|------|---------|
| `0` | Report returned (including the well-formed empty answer). |
| `1` | Load error, invalid `--limit`/`--min-size`, or unknown/ambiguous `--repo` selector. |

## Compared to the boring alternative

`rg` / `git grep` can locate a snippet you already suspect, but cannot
*discover* unknown duplicate bodies: they are defeated by
whitespace/comment/formatting differences and return raw line hits with no
symbol identity or grouping. `eg query clones` groups by the normalized body
the extractor already computed, and every member is a dereferenceable graph
handle an agent can pivot on with `eg query symbol`, `eg query context`, or
`eg query change-impact` — something line-hit grep cannot offer.

## Out of scope (per issue #216)

- Near-duplicate / token-similarity / threshold-based ("Type-3") clone
  detection — exact normalized-body equivalence only; fuzzy similarity stays
  with the embedding path (#154).
- Cross-language clone detection (Rust only in this slice).
- Automatic refactoring, helper extraction, or any write/edit action —
  read-only discovery only.
- Ranking clones by "refactor value" or complexity (#162).
- Sub-symbol granularity; the unit is a whole symbol body.
