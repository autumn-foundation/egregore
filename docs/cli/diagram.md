# `eg query diagram` — local call neighborhood as a diagram

Renders one symbol's local call neighborhood as a diagram in a standard text
format (Mermaid by default, or Graphviz DOT). This is the local-orientation
lane for the transitive family (`transitive-callers`, `transitive-callees`):
instead of a flattened reachability list, you get a citable picture of the
symbol's immediate surroundings — direct callers, direct callees, and the
containing file/module — suitable for pasting into a PR description, an ADR,
or a review comment.

```sh
eg query diagram login --graph graph.jsonl
eg query diagram login --graph graph.jsonl --format dot | dot -Tsvg -o neighborhood.svg
eg query diagram login --graph graph.jsonl --format json > neighborhood.json
eg query diagram login --graph graph.jsonl --depth 2 --max-nodes 40
```

## What the diagram shows

The walk is a level-synchronized BFS from the resolved symbol anchor over a
**closed edge vocabulary** — `CALLS`, `DEFINES`, `CONTAINS`, `IMPORTS`. No
edge appears that is not in the graph; no labels are synthesized.

Expansion rule, per hop:

- from a **symbol**: inbound `CALLS` (callers), outbound `CALLS` (callees),
  inbound `DEFINES`/`CONTAINS` (containing file/module);
- from a **file/module**: outbound `DEFINES`/`CONTAINS` (member symbols),
  outbound `IMPORTS` (imported files).

So the default `--depth 1` neighborhood of `login` is: who calls `login`,
who `login` calls, and the file/module that defines it. At `--depth 2` the
containing file additionally contributes its other member symbols and its
imports. Repository-level expansion is excluded: the walk never climbs to the
repository root, so depth 2 stays local.

After the walk, the rendered edge set is the **induced subgraph** over the
admitted nodes restricted to the walk vocabulary: every stored edge of those
four kinds whose endpoints are both rendered appears, and no other edge
appears.

## Determinism and ordering

Output is byte-identical across runs:

- node keys (`n0`, `n1`, …) are assigned in **ascending record-ID order**;
- edges are emitted ordered by `(source ID, target ID, label, edge ID)`;
- every node carries its shortest hop distance from the anchor;
- the anchor node is styled as the focus (`classDef anchor` in Mermaid,
  `shape=box, style=bold` in DOT).

Every rendered node is citable: the diagram header carries a legend mapping
each node key to its **stable record ID** plus repo-relative file/span
(`src/auth.rs:10-20`), with the node kind and name. The JSON envelope
additionally carries the structured nodes/edges, hop distances, the
truncation diagnostics, and both rendered diagram texts.

## Truncation is explicit, never silent

- **Depth bound** (`--depth`, default 1): reachable nodes beyond the bound are
  counted per depth in a `max_depth_truncated` diagnostic
  (`dropped_frontier: [{depth, count}]`, `dropped_total`).
- **Node budget** (`--max-nodes`, default 100, anchor included): discovered
  nodes are admitted in `(hop, record ID)` order; the remainder are counted in
  a `node_budget_truncated` diagnostic with `dropped_nodes` and
  `dropped_edges`.

Both notices are embedded in the Mermaid/DOT comments, the JSON
`truncation` object, and the text format.

## Formats

- `mermaid` (default): a `flowchart LR` block. Paste into any Markdown
  renderer or compile with `mmdc`.
- `dot`: a `digraph`. Pipe into `dot -Tsvg`.
- `json`: machine-readable envelope — `ok`, `lane`, `handle`, `target`
  (record ID, name, kind, path, span), `depth`, `max_nodes`, `at_commit`,
  `as_of`, `node_count`, `edge_count`, `ordering`, `nodes[]` (key, record ID,
  name, kind, path, span, hop), `edges[]` (edge record ID, label, source/target
  keys and record IDs), `truncation`, `diagnostics`, plus the embedded
  `mermaid` and `dot` texts and a `disclaimer`.
- `text`: human-readable node/edge listing with the same content and ordering.

Node labels use the symbol/file name (falling back to the record ID).
Mermaid labels escape `"` as `#quot;` (the Mermaid-documented entity escape);
DOT labels use `\"`/`\\`. The anchor is always marked.

## Temporal selectors

`--at <sha|prefix>` and `--as-of <RFC3339>` reuse the bitemporal selector
from the transitive lanes (issues #8/#66): the diagram renders the graph state
at one commit's snapshot. A symbol absent at the pinned revision is a stable
`no_match` diagnostic (exit 2), not an empty diagram.

## Diagnostics and exit codes

| Exit | Meaning |
|------|---------|
| 0 | Diagram rendered. |
| 1 | Malformed/ambiguous/unsupported input: `--depth 0`, `--max-nodes 0`, an ambiguous symbol name (candidates listed), or a non-symbol handle. Machine-readable JSON on stderr. |
| 2 | Unknown handle, stale (tombstoned) handle, or an unresolvable `--at`/`--as-of` selector. JSON envelope on stdout. |
| 3 | `empty_neighborhood`: the symbol resolved but has no callers, callees, or container in this view. JSON envelope on stdout — never a lone-node diagram masquerading as "no dependencies". |

Handles follow the shared query conventions: a symbol record ID
(`codegraph:vN:<hex>`) or an exact symbol name; `--repo` scopes resolution to
one repository. The lane is strictly read-only: `--data-dir` reads from a
throwaway copy, never the live store (issue #424).

## Trust and use

The diagram is a **projection of stored graph edges, not proof of runtime
behavior**. Every node cites its stable record ID; verify citations against
the store before acting. It answers "what does the graph say surrounds this
symbol" — for reachability leads see `transitive-callers`/`transitive-callees`,
for orientation see `orientation`.

Use it when: onboarding onto unfamiliar code, reviewing a PR's blast radius,
or attaching a citable picture to a design discussion. Do not use it as a
control-flow or data-flow diagram — the lines are graph edges.
