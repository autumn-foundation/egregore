# eg query diagnostics

List the persisted **extractor-coverage-gap** `Diagnostic` graph nodes — the
extractor's self-declared blind spots — as a citable triage lane, answering
the operator-visible question *"where did the extractor flag something it
could not parse or resolve?"*. Local-first; no scan, no recompute, no
network access, no embeddings.

> **"No extraction-gap diagnostics in scope" is not proof the code is fully
> understood for any other purpose** — only that the extractor flagged nothing
> it could not parse. A clean scope means the extractor raised no gap markers;
> it says nothing about correctness, soundness, test coverage, or whether a
> construct was understood deeply enough for some other analysis.

## The closed gap set

This lane reports exactly the `NodeKind::Diagnostic` nodes that carry **both**
a `repo_relative_path` and a `span` — every row is resolvable to a file + span
that lands on the flagged source region. In practice these are the gap markers
the scanner mints:

| Marker | Example `summary` | Meaning |
|--------|-------------------|---------|
| unsupported macro invocation | `unsupported macro invocation println!` | a `macro_invocation` naming no uniquely resolvable repo-defined macro: external macros (e.g. `println!`), or a repo name defined zero or ≥2 times |
| unresolved call | `unresolved call external_crate::missing_fn` | a call with no in-repo resolution target |
| unresolved dispatch | `unresolved trait-dispatch target Trait::method (no in-crate implementor method)` | a trait call with no matching implementor |

A `macro_invocation` that names exactly one repo-defined `macro_rules!`
macro does not surface here at all: it resolves repo-wide to a `CALLS`
edge from the invoking scope to the macro symbol, stamped `resolved` and
carrying the call-site spans (see `docs/cli/query.md` — the callers lane).
Only external, missing, or ambiguously-defined macro names keep the
`unsupported macro invocation` diagnostic.

### What this lane deliberately does NOT report

- **Source-authored markers** — TODO/FIXME `DebtMarker` nodes and
  `.unwrap()`/`.expect()` `PanicRiskSite` nodes are different node kinds and
  can never match this filter. They belong to `eg query debt-markers` and
  `eg query unwrap-expect`; this lane does not reclassify them.
- **Span-less `Diagnostic` nodes** — skipped-manifest dependency-coverage
  holes (e.g. an unparseable `Cargo.toml`) carry a path but no span; they
  surface through the manifest-deps lane's `skipped_manifest` diagnostics,
  which is their home surface.
- **Importer `Diagnostic` nodes** — GitHub-import review diagnostics carry no
  repo-relative path; they are importer artifacts, not extractor gaps.

## Synopsis

```text
eg query diagnostics --graph <PATH>   [--repo <SELECTOR>] [--file <PATH>] [--at-head | --all-history] [--format json|text]
eg query diagnostics --data-dir <DIR> [--repo <SELECTOR>] [--file <PATH>] [--at-head | --all-history] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). Strictly read-only: querying creates or modifies no records or
indexes, and re-running the identical query against an unchanged store yields
byte-identical output.

## Response shape

Default `--format json` is **newline-delimited JSON**: one compact object per
line, one per gap, in deterministic order
(`repo_relative_path`, `span.start_byte`, `git_commit`, `record_id`):

```json
{"record_id":"codegraph:v5:…","schema_version":5,"kind":"Diagnostic","name":"println!","summary":"unsupported macro invocation println!","repo_relative_path":"src/main.rs","span":{"start_byte":120,"end_byte":143,"start_line":5,"end_line":5},"language":"rust","repository_id":"codegraph:v5:…","repository":"owner/name","trust":"source_fact","corpus_mode":"head_anchored","corpus_mode_source":"default","corpus_disclaimer":"…"}
```

Stable fields:

| Field | Meaning |
|-------|---------|
| `record_id` | stable code-graph record ID — the citable handle |
| `schema_version` | record schema version |
| `kind` | always `"Diagnostic"` |
| `name` | the diagnostic subject: macro invocation (`println!`), unresolved dispatch (`unresolved_dispatch: Trait::method`), or unresolved-call display |
| `summary` | human summary, e.g. `unsupported macro invocation println!` |
| `repo_relative_path` | repo-relative file handle |
| `span` | `start_byte` / `end_byte` / `start_line` / `end_line` — lands on the flagged region |
| `language` | extractor language, when recorded |
| `git_commit` | history-backed rows only |
| `repository_id` / `repository` | owning repository, when the store carries topology |
| `trust` | always `"source_fact"` — every row is a deterministic extractor fact |
| `corpus_mode` / `corpus_mode_source` / `corpus_disclaimer` | which corpus was read (see [Corpus scope](corpus-modes.md)) |

`--format text` prints one human-readable line per gap:

```text
unsupported macro invocation println! @ src/main.rs:5 [codegraph:v5:…]
```

## Scoping

- `--repo <SELECTOR>` — the standard repository selector (record ID, display
  name, basename/override, remote URL, root commit SHA, or canonical path). An
  unknown or ambiguous selector exits 1 with the standard machine-readable
  diagnostic on stderr; it never yields a silent empty result.
- `--file <PATH>` — restrict to one repo-relative file path (exact match).
  A path naming no `File` node in the selected store slice exits 2 with a
  machine-readable `unknown_file` envelope — a typo is never a silent "clean".

## Empty vs not-found honesty

| Condition | Exit | Output |
|-----------|------|--------|
| Gaps returned | `0` | NDJSON rows on stdout |
| Scope exists but contains **zero** gap diagnostics | `0` | one explicit marker line: `{"ok":true,"lane":"diagnostics","diagnostics":[],"empty_reason":"no_gaps_in_scope",…}` (`# no_gaps_in_scope: …` in text mode) |
| `--file` names nothing in the store slice | `2` | `{"ok":false,"error":{"code":"unknown_file",…}}` |
| Unknown / ambiguous `--repo` selector | `1` | standard selector diagnostic on stderr |

"Scope contains zero gap diagnostics" and "scope not found" are distinct
machine-readable answers — the lane never conflates them. A zero-gap scope is
a citable positive fact: the extractor flagged nothing it could not parse in
scope.

## Corpus scope

With **neither** a corpus flag nor a temporal selector, over a `scan-history`
store this lane defaults to the **HEAD-anchored** corpus — records current at
each repository's stamped HEAD commit — so a gap resolved before HEAD no
longer appears. Pass `--all-history` to read the union of all commit snapshots
(the same gap then appears once per commit, distinguished by `git_commit`);
pass `--at-head` to force the HEAD-anchored view explicitly. Over a
snapshot-less (plain `scan`) store the disclosure is `single_snapshot`. Every
row carries `corpus_mode` / `corpus_mode_source` / `corpus_disclaimer`, and the
empty-scope marker carries them too. `--at-head` and `--all-history` are
mutually exclusive; a conflict exits `1` with `unsupported_combination`. See
[Corpus scope for query lanes](corpus-modes.md).

## Record shape

Each gap is a deterministic `Diagnostic` code-graph node emitted by `eg scan` /
`eg scan-history` (one record per gap):

- `name` — the diagnostic subject (`println!`, `unresolved_dispatch: Trait::method`, …);
- `summary` — the human summary (`unsupported macro invocation println!`, …);
- `repo_relative_path` + `span` — the citable file/span handle;
- a `CONTAINS` edge from the owning `File` node (repository attribution);
- standard temporal metadata on history-backed records.

Trust separation holds: `Diagnostic` gap nodes live in the code-graph domain
(`source_fact` trust class), separate from agent-authored observations by
construction.
