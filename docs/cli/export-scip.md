# `eg export scip` — Export a Scan as a SCIP Code-Intelligence Index

**Issue:** #233 — _Export an Egregore scan as a SCIP index for existing code-nav tools_

---

## This is a DEFINITIONS-ONLY index

`eg export scip` emits a **definitions-only** SCIP index. It powers
**go-to-definition** and **workspace/document symbol search** in any SCIP
consumer (Sourcegraph, an LSP/SCIP editor index, code-review annotation
tooling), but it does **NOT** power **find-references**, call hierarchies, or
implementation navigation. Every emitted `Occurrence` is a
`SymbolRole::Definition`; the index contains **zero reference occurrences**.

The reason is structural, not a bug: Egregore's cross-symbol graph edges
(`CALLS` / `IMPORTS` / `REFERENCES` / `IMPLEMENTS`) are **positionless** — they
name a source and a target record but carry no source coordinate, and a SCIP
reference occurrence requires a position. Rather than fabricate ranges, this
slice omits them (see [Fabrication guards](#fabrication-guards-ac7)). Both
follow-ups below have landed at the data level; emitting reference occurrences
remains future exporter work:

* **#462** — _Retain `CallSiteFact.span` on edges to enable SCIP reference
  occurrences._ **Landed:** a resolved `CALLS` edge now carries
  `call_site_spans` — the deduplicated per-call-site spans (interpreted in the
  caller symbol's file), attached by both the cross-file and same-file
  resolution passes. Ambiguous/unresolved edges carry none, per the
  fabrication-guard discipline. Emitting reference occurrences from this data
  is still future SCIP-exporter work.
* **#463** — _Column-precision SCIP ranges._ **Implemented**: the Tree-sitter
  extractors record zero-based byte-offset columns on `SourceSpan`
  (`start_column` / `end_column`, additive and never identity inputs), so
  definition ranges are column-precise with no source re-read; column-less
  legacy spans degrade to the whole-line fallback.

---

## Overview

`eg export scip` reads a code graph — from a `scan` / `scan-history` JSONL
(`--graph`) or an embedded `AletheiaDB` store (`--data-dir`) — and writes a
standard [SCIP](https://github.com/sourcegraph/scip) index protobuf. It is a
**one-way interop bridge**: Egregore's deterministic, byte-stable symbol and
definition facts light up navigation in tools a maintainer already runs, without
adopting the `eg query` CLI.

```
eg export scip --graph <graph.jsonl> --out <index.scip>
eg export scip --data-dir <store>    --out <index.scip>
```

Exactly one of `--graph` or `--data-dir` is required (they are mutually
exclusive); `--out` names the `.scip` file to create or overwrite. The command
is **fully local**: no network access, no remote symbol resolution, no hosted
upload. The `--data-dir` read is strictly read-only (it reads a throwaway copy of
the store, never the live store).

It is built with Sourcegraph's official `scip` crate (pure Rust, no `protoc`, no
C toolchain), so the emitted protobuf is the reference binding for the reference
format.

## What is emitted

| Egregore graph fact | SCIP output |
|---|---|
| Each `File` node | one `Document` (`relative_path` = repo-relative path; `language` from the file extension: Rust / Python / TypeScript / Go) |
| Each span-bearing `Symbol` node (and span-bearing `Module` node) | one `SymbolInformation` + one `SymbolRole::Definition` `Occurrence` |
| `visibility` / `signature` / `doc` on a symbol | `documentation[]` (visibility + doc text) and `signature_documentation` (the declaration header) |
| `Repository` node name | the moniker package name + the `project_root` URI |

Each definition `Occurrence.range` is **column-precise** whenever the extractor
recorded columns: `[start_line - 1, start_column, end_line - 1, end_column]` —
SCIP's 0-based, half-open lines/characters derived from Egregore's 1-based
`SourceSpan` plus its zero-based byte-offset columns, with **no source re-read**
(issue #463). Spans from legacy graphs or non-tree-sitter sources carry no
columns and degrade honestly to the whole-line fallback
`[start_line - 1, 0, end_line, 0]`. Every `Document` declares
`position_encoding: UTF8CodeUnitOffsetFromLineStart` so consumers interpret the
`character` values as the UTF-8 byte offsets the Tree-sitter extractors record.
The index `Metadata` carries `tool_info` (`name: "egregore"`, the
crate version), `project_root` as a `file://` URI, and `text_document_encoding:
UTF8`.

### Symbol monikers (global scheme)

Monikers are **global** (cross-document navigable), not `local`, so
go-to-definition resolves across the whole index. Each moniker is built with the
`scip` crate's own `Symbol` formatter, so it is grammar-valid by construction:

```
scip-egregore cargo <package> 0.0.0 <descriptors>
```

* **scheme** `scip-egregore` (identifies Egregore as the indexer).
* **manager** `cargo`.
* **package** the `Repository` node's display name (or `egregore` when no
  repository is present).
* **version** the fixed placeholder `0.0.0` — the graph does not retain the
  scanned crate's own `[package] version`, and a stable sentinel keeps monikers
  byte-identical across runs. (A real version join is a future refinement.)
* **descriptors** derived deterministically from the ADR-0004 identity: the
  module-qualified name is split on `::`; every non-final segment becomes a
  `namespace/` descriptor, and the final segment takes the suffix its kind
  dictates (`Type#` for struct/enum/trait/type-alias, `method().` for
  function/method, `term.` for const/static, `namespace/` for a module). The
  ADR-0004 source-order **disambiguator** feeds the SCIP method-disambiguator
  slot for callables (and a trailing `meta:` descriptor for other kinds), so the
  moniker is unique exactly where the Egregore identity is.

Because the derivation is span-independent (ADR-0004), monikers are stable across
unrelated edits and re-scans.

## Fabrication guards (AC#7)

Symbols and spans Egregore cannot faithfully place are **dropped, never emitted
with fabricated ranges**, and the command reports how many nodes were skipped and
why. Dropped, per the definitions-only contract:

* **Positionless edges** — every `CALLS` / `IMPORTS` / `REFERENCES` /
  `IMPLEMENTS` / containment edge (no position → no occurrence). This is why the
  index has no reference occurrences.
* **`Diagnostic` stub/marker nodes** — extractor stubs for unsupported
  constructs and unresolved call targets; emitting them would invent symbols.
* **Span-less nodes** — any node with no `SourceSpan`.
* **Anonymous `impl` blocks** — `impl`-kind symbol nodes have no navigable
  display name; their contained methods still map via their own `method` nodes.

The final report is printed to stdout:

```
exported <N> definitions across <M> documents to <out>; skipped <K> nodes (<J> no-span, <I> diagnostic-stubs, <H> impl-blocks)
```

## Determinism

Output is **byte-identical across runs** and independent of graph insertion
order. Documents are sorted by `relative_path`; within a document,
`SymbolInformation` and `Occurrence`s are sorted by
`(start_line, name, record_id)` before encoding. The moniker version component is
a fixed sentinel and no wall-clock time enters the output.

## Reading it back

The emitted bytes are a standard SCIP `Index` protobuf and load into any SCIP
consumer (`scip print`, Sourcegraph `src code-intel upload`). Egregore also
validates the round-trip **in-process** — `aletheia_egregore::scip::decode_index`
parses the bytes back — so conformance is tested without an external `scip` CLI.

## Relationship to `eg export`

The bare `eg export --data-dir <dir> --out <file.jsonl>` command is unchanged: it
dumps the store as canonical Egregore JSONL (issue #155, see
[`export.md`](export.md)). `eg export scip` is an additive sibling that emits the
SCIP interchange format instead; it does not alter the JSONL dump.

## Out of scope

* **Reference / implementation occurrences** (callers, implementors) — the
  positional edge data now exists (#462: `call_site_spans` on resolved `CALLS`
  edges); emitting them from `eg export scip` is future work, also gated on
  the edge-resolution work #134/#148/#152.
* **Column-precise ranges** — #463, implemented.
* **LSIF emission**, **temporal / historical export** (SCIP is single-snapshot),
  **importing SCIP** produced by other indexers, and any **hosted indexing,
  remote crawl, or automatic upload**.
