# eg query lanes

Emit the **machine-readable query-lane capability manifest** (issue #251): a
deterministic JSON catalog of every public `eg query` lane, for agents that
need to discover what the query surface can do without reading prose docs.

```text
eg query lanes [--format json|text]
```

Local and pure: no network, no ingested store. Works on a fresh clone with an
empty data dir.

## Output

Default `--format json` emits one JSON document:

```jsonc
{
  "lanes": [
    {
      "id": "at",
      "purpose": "Resolve a `file:line` location to its smallest enclosing code symbol (issue #151).",
      "required_inputs": ["<location>", "--graph | --data-dir"],
      "store_mode": "structural",
      "trust_classes": ["source_derived"],
      "citable_handles": true,
      "freshness": ["ingest_snapshot"]
    }
  ]
}
```

The output is byte-for-byte stable across repeated runs on the same build:
lanes are sorted by id, and every list field uses a canonical order.

`--format text` renders the same catalog as a human-skimmable table.

## Field reference

- **`id`** — stable lane id: the `eg query <id>` subcommand name.
- **`purpose`** — one-line purpose, taken from the first line of the lane's
  `--help` text.
- **`required_inputs`** — the lane's input contract, derived from the actual
  clap surface: required positionals as `<name>`, required flags as `--flag`,
  plus the store selector the query loader enforces (`--graph | --data-dir`;
  `--data-dir` only for the embedding lanes, which accept no `--graph`).
- **`store_mode`** — `structural` (JSONL graph or embedded store; no embedding
  model needed) or `semantic` (embedded store ingested with `--embed`; the
  lane needs a vector index and refuses a mismatched embedder identity —
  except `similar`, which queries with the anchor's stored vector and so
  needs no query-side model identity).
  Exactly the `semantic`, `semantic-context`, `semantic-memory`, and `similar`
  lanes are `semantic` (all four are only built with the `embeddings` cargo
  feature; without it the manifest lists just the structural lanes).
- **`trust_classes`** — the answer trust class(es), from the closed
  [`TrustClass`](../../src/query/trust.rs) vocabulary (`source_derived`,
  `verification_evidence`, `agent_verified`, `agent_unverified`,
  `agent_contradicted`, `project_state`, `artifact`, `runtime_observation`,
  `other`), in canonical order.
- **`citable_handles`** — whether answers carry citable stable record
  handles (record IDs / repo-relative path+span handles).
- **`freshness`** — freshness/staleness semantics, closed vocabulary:
  - `ingest_snapshot` — answers reflect the store as ingested; staleness is
    bounded by the last ingest, never checked live.
  - `valid_time` — answers are valid-time facts over recorded history
    (`--at`/`--as-of` pins, history-ranked lanes); staleness is measured
    against the pinned commit/instant, never wall-clock now.
  - `working_tree` — the lane can check answers against the live working tree
    and reports per-answer freshness codes (`--repo-path`).
  - `store_live` — answers reflect the live agent-memory/project store;
    staleness is a property of how recently records were written.

## Anti-drift contract

The manifest is derived from the actual command surface, not hand-maintained
prose:

- ids, purposes, and required inputs come from clap introspection of
  `QuerySubcommand`;
- the per-lane classification (store mode, trust classes, citation flag,
  freshness) is a static table in `src/cli/lanes.rs`;
- `tests/integration/lane_manifest.rs` plus unit tests in `src/cli/lanes.rs`
  assert a 1:1 mapping between implemented query subcommands and manifest
  entries — no orphan lanes (implemented but unlisted), no phantom entries
  (listed but unimplemented) — and that every entry carries a populated
  contract. Adding a lane without a manifest entry fails CI.
