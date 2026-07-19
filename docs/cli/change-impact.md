# eg query change-impact

Surface **graph-derived impact leads** for a code symbol or file — answer the
operator-visible question *"if I edit this handle, what nearby code should I
inspect first?"* — starting from a symbol name, canonical record ID, or
repo-relative file path. Local-first; no network access.

> **Rows are impact LEADS to inspect before editing, not proof of breakage.**
> Absence of a lead is not proof a change is safe. This query reports what the
> graph's recorded relationships can and cannot show; it never infers a breakage
> cause when supporting relationship evidence is absent.

## Synopsis

```text
eg query change-impact <HANDLE> --graph <PATH>    [--repo <SELECTOR>] [--depth N]
eg query change-impact <HANDLE> --data-dir <DIR>  [--repo <SELECTOR>] [--depth N]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`).

## Handle resolution

`<HANDLE>` accepts these handle types, tried in order:

1. **Canonical code record ID** — `codegraph:v<N>:<64hex>` naming a `Symbol` or
   `File`.
2. **Repo-relative file path** — e.g. `src/lib.rs`. For file handles the
   traversal automatically seeds the file's directly defined/contained symbols so
   callers of file-local symbols appear at hop 1.
3. **Exact symbol name** — e.g. `process_event`. Several symbols of the same name
   in one repository form a multi-anchor query; the same name across repositories
   is ambiguous unless `--repo` is given.

`--repo <SELECTOR>` restricts file/symbol resolution to one repository; without
it, a file or symbol that matches more than one repository is reported as
`Ambiguous` rather than resolved implicitly.

| Condition | Exit | Output |
|-----------|------|--------|
| Success (including a resolved target with **no** leads) | `0` | Change-impact JSON on stdout, `ok:true` |
| Empty handle or malformed canonical ID | `1` | `{"Unsupported":{...}}` on stderr |
| Handle matches targets in more than one repository | `1` | `{"Ambiguous":{...}}` on stderr |
| Handle resolves to no live target in the store | `2` | `{"ok":false,"error":{"code":"no_match",...}}` on stdout |
| Handle names a tombstoned (deleted) target | `2` | `{"ok":false,"error":{"code":"stale_handle",...}}` on stdout |

A resolved target that simply has no leads is a **real, successful, empty
answer** (exit 0), distinct from a handle that resolves to nothing (exit 2).

## Shortest offline workflow

```sh
# Scan the working tree into a JSONL graph file
eg scan . --out graph.jsonl

# Query change impact for a symbol name
eg query change-impact process_event --graph graph.jsonl

# Query by canonical record ID
eg query change-impact codegraph:v4:abc123... --graph graph.jsonl

# Query a file — seeds all symbols it defines
eg query change-impact src/lib.rs --graph graph.jsonl

# Widen the neighborhood (default depth is 1)
eg query change-impact process_event --graph graph.jsonl --depth 2

# Scope to one repository in a multi-repo store
eg query change-impact Widget --graph graph.jsonl --repo acme/widget
```

## Response shape

The top-level envelope always includes all five groups (even when empty) so
"checked and none found" is distinct from "class not checked":

```json
{
  "ok": true,
  "handle": "process_event",
  "target_type": "symbol",
  "target_ids": ["codegraph:v4:abc123..."],
  "depth": 1,
  "disclaimer": "Rows are impact LEADS to inspect before editing, not proof of breakage. Absence of a lead is not proof a change is safe.",
  "direct_callers": [...],
  "direct_callees": [...],
  "referencing_files": [...],
  "implementation_symbols": [...],
  "containing_context": [...],
  "truncations": [],
  "diagnostics": [],
  "page": { "cursor": null, "has_more": false, "returned": 3 }
}
```

## Groups and edge-direction semantics

| Group | Edge label | Direction | Meaning |
|-------|-----------|-----------|---------|
| `direct_callers` | `CALLS` | inbound | Code that calls the anchor symbol. |
| `direct_callees` | `CALLS` | outbound | Code the anchor symbol calls. |
| `referencing_files` | `IMPORTS` / `REFERENCES` | inbound | Files that import or reference the anchor. |
| `implementation_symbols` | `IMPLEMENTS` | either | Trait impls for the anchor, or what the anchor implements. |
| `containing_context` | `DEFINES` / `CONTAINS` | inbound | The owning file or module; always hop-1, never truncated. |

Only `Symbol` nodes expand the frontier for subsequent hops; `File` and
`Module` owners stop expanding (prevents whole-repo blow-up at depth ≥ 2).
Non-call code references (type and value uses, trait bounds) are recorded by
the extractor as `REFERENCES` edges and surface under `referencing_files`;
the agent-domain `MentionsSymbol` edge (an observation referencing a symbol)
is cross-domain and is not part of this code-topology traversal.

## Per-row fields (provenance — AC4)

Each lead row carries:

| Field | Description |
|-------|-------------|
| `record_id` | Stable BLAKE3-based `codegraph:vN:<hex>` ID. |
| `kind` | Node kind (`Symbol`, `File`, `Module`, …). |
| `schema_version` | Record schema version. |
| `name` | Symbol or file name. |
| `repo_relative_path` | Repo-relative path or `null`. |
| `span` | Source span or `null`. |
| `valid_time` | Valid-time range when temporal. |
| `symbol_kind` | Language-specific symbol kind or `null`. |
| `language` | Language tag or `null`. |
| `relation` | Edge label string (`CALLS`, `IMPORTS`, …). |
| `direction` | `"inbound"` or `"outbound"`. |
| `edge_record_id` | Stable ID of the connecting edge. |
| `resolution` | Call resolution status (`"resolved"` / `"ambiguous"` / `"unresolved"`) for `CALLS` edges labeled by the resolution passes (issues #152/#134); omitted when the edge carries none. Filter on `resolution == "resolved"` to act only on uniquely resolved call edges. |
| `edge_git_commit` | Git commit SHA for the edge record. |
| `anchor_id` | The anchor record this lead is reached from. |
| `hop` | BFS hop distance from the anchor (always ≥ 1). |
| `trust` | Always `"impact_lead"` — rows are leads, not proof. |

## `--depth` default and truncation contract

`--depth` defaults to `1` (direct neighbors only). Use `--depth 2` for a wider
neighborhood. Extremely high-fanout nodes can saturate a group; the per-group
cap is 200 leads. When a group exceeds the cap, the response includes a
`truncations[]` entry:

```json
{
  "group": "direct_callers",
  "returned": 200,
  "total": 347,
  "depth": 1
}
```

and a `neighborhood_truncated` diagnostic. No group with members is ever fully
dropped — if truncation occurs, at least the cap-count leads are returned.

## When to use vs. alternatives

| Reach for | When you want |
|-----------|---------------|
| **`eg query change-impact`** (this) | **What code to inspect before editing** — graph-derived blast-radius leads, grouped by relation. |
| `eg query context` | General evidence-backed context for a **symbol**. |
| `eg query subsystem` | Everything under a **directory prefix** — code, tasks, drift, memories. |
| `eg query task` | Whether a **task / acceptance criterion** is complete. |
| `eg query failures` | **Prior failed attempts** linked to a code or task handle. |
| `rg` / `git grep` | Fast recursive **text** search when you know the string. |
| `git log -S` | Commits where matching **text changed** (pickaxe). |
| rust-analyzer | Live **type-checked** call hierarchy within one workspace. |
| `eg query semantic` | Conceptually **similar** code when you do not know the exact name. |

`rg` and rust-analyzer are faster when you already know what to look for, but
they do not produce graph-record IDs, stay local to one workspace, and do not
join to agent memory or verification evidence. This query is handle-driven,
redaction-safe, and byte-identical across runs for reproducible agent workflows.

## Safety: no raw payloads

Output never includes raw transcript text, raw command output, patch hunks,
issue bodies, or bearer tokens. Records with protected payloads are surfaced
through a `protected_payload` diagnostic; only IDs, hashes, labels, paths,
spans, and bounded summaries appear in leads.

## Diagnostic codes

| Code | Meaning |
|------|---------|
| `unresolved_edge_target` | An in-scope edge points to a node absent from the store. |
| `unsupported_relation` | An in-scope codegraph edge has an unhandled label. |
| `neighborhood_truncated` | A group was capped at `MAX_LEADS_PER_GROUP`; see `truncations[]`. |
| `protected_payload` | A reached record carries a protected payload (withheld; hash only). |

## Scope

This slice consumes existing code-graph, query, repository-identity,
schema-versioning, redaction, and protected-artifact contracts. It introduces
no new graph domain, importer, edge vocabulary, trust model, hosted service,
LLM-generated answer, or language expansion (issue #76, AC10).
## Corpus scope

This lane is designed as a current-state lane but today reads the **union of all
commit snapshots** over a `scan-history` store. It discloses this honestly —
`corpus_mode: "union"` (or `single_snapshot` over a snapshot-less store),
`corpus_mode_source`, and `corpus_disclaimer` — and does **not** yet accept the
`--at-head`/`--all-history` flags (an `--at`/`--as-of` selector, where offered,
still pins a single commit as `commit_pinned`). Flipping it to the HEAD-anchored
default and adding that flag pair, under the contract published in issue #427, is
tracked in **issue #456**. See [Corpus scope for query lanes](corpus-modes.md).
