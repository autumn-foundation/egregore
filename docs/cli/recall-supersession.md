# Recall-Time Supersession and Contradiction

Manage temporal-trust of agent observations during bulk query recall. This feature filters out or flags superseded or contradicted memory records in bulk query results.

> **Temporal trust is computed dynamically at recall time.** Unlike database migrations or hard deletes, the original claims are preserved intact in the historical graph; the query-time resolver dynamically traces supersession and contradiction graphs to assess which claims are currently active.

## Summary of Recall-Time Supersession

When querying memory context or recalling prior observations, older agent-authored observations can be superseded by newer ones, or separate observations can explicitly contradict each other.

To prevent agent routines from retrieving stale, overridden, or mutually conflicting claims:
1. **Exclude mode (default)**: Superseded, cycled, or contradicted records are filtered out from the main results and surfaced under a top-level `excluded` diagnostics array.
2. **Include But Flag mode**: All records are returned, but they carry a `temporal_status` field (`current`, `superseded`, `contradicted`, or `cycle`) along with `superseded_by` or `contradicted_by` references (with target record IDs and handles).

---

## When to Use Recall-Time Supersession vs. Other Memory Surfaces

| Surface | Command / Option | Scope | Purpose |
|---|---|---|---|
| **Plain Recall** | `eg query semantic-memory <QUERY>` | Vector search over all observations | Retrieve any prior agent memory matching a natural-language query by meaning. |
| **Recall-Time Supersession** | `--supersession <exclude\|include-but-flag>` | Query filter/flag on bulk lanes | Dynamically hide or annotate superseded/contradicted records in bulk query results. |
| **Single-Record Memory Audit** | `eg query memory <ID>` | Depth-1 graph audit of one record | Deeply inspect all evidence, contradictions, and supersessions surrounding *one known* memory claim. |
| **Cited-Code Drift** | `eg query drift` / `eg query evidence-freshness` | Code file/span changes | Triage whether code cited in a memory/fact has modified since the memory was captured (detecting semantic code drift). |

---

## CLI Synopsis

The `--supersession <exclude|include-but-flag>` option is supported on the following query subcommands:
* `eg query context <NAME> [--supersession <mode>]`
* `eg query subsystem <NAME> [--supersession <mode>]`
* `eg query semantic-memory <QUERY> [--supersession <mode>]`
* `eg query semantic-context <QUERY> [--supersession <mode>]`

### Options
* `exclude` (default): Drops superseded/contradicted records from `observations` and lists them in `excluded`.
* `include-but-flag`: Returns all matching observations annotated with:
  * `"temporal_status": "current" | "superseded" | "contradicted" | "cycle"`
  * `"superseded_by": [{"record_id": "...", "handle": "..."}]` (when superseded)
  * `"contradicted_by": [{"record_id": "...", "handle": "..."}]` (when contradicted)

---

## Shortest Local Workflow

### 1. Ingest observations with supersession relationships
Create a `graph.jsonl` containing a chain of observations and a contradiction edge:

```json
{"record_type": "node", "id": "agent_memory:v1:obs-a", "kind": "Observation", "schema_version": 1, "text": "Claim A", "superseded_by": "agent_memory:v1:obs-b", "evidence_links": [{"target_record_id": "codegraph:v4:my-symbol", "target_domain": "codegraph", "relation": "MENTIONS_SYMBOL", "confidence": "0.9"}]}
{"record_type": "node", "id": "agent_memory:v1:obs-b", "kind": "Observation", "schema_version": 1, "text": "Claim B", "superseded_by": "agent_memory:v1:obs-c", "evidence_links": [{"target_record_id": "codegraph:v4:my-symbol", "target_domain": "codegraph", "relation": "MENTIONS_SYMBOL", "confidence": "0.9"}]}
{"record_type": "node", "id": "agent_memory:v1:obs-c", "kind": "Observation", "schema_version": 1, "text": "Claim C (current)", "evidence_links": [{"target_record_id": "codegraph:v4:my-symbol", "target_domain": "codegraph", "relation": "MENTIONS_SYMBOL", "confidence": "0.9"}]}
```

Ingest this graph into your local egregore database:
```sh
eg ingest graph.jsonl --adapter embedded --data-dir .egregore --embed
```

### 2. Query with Exclude Mode (Default)
Recalled observations will automatically filter out `obs-a` and `obs-b`, only returning `obs-c`:
```sh
eg query semantic-memory "Claim" --data-dir .egregore
```

Output:
```json
{
  "observations": [
    {
      "record_id": "agent_memory:v1:obs-c",
      "text": "Claim C (current)"
    }
  ],
  "excluded": [
    {
      "record_id": "agent_memory:v1:obs-a",
      "reason": "superseded",
      "superseded_by": [
        {
          "record_id": "agent_memory:v1:obs-c",
          "handle": "agent:test:session-c"
        }
      ]
    },
    {
      "record_id": "agent_memory:v1:obs-b",
      "reason": "superseded",
      "superseded_by": [
        {
          "record_id": "agent_memory:v1:obs-c",
          "handle": "agent:test:session-c"
        }
      ]
    }
  ]
}
```

### 3. Query with Include But Flag Mode
Include superseded memories to trace histories or debug context evolution:
```sh
eg query semantic-memory "Claim" --data-dir .egregore --supersession include-but-flag
```

Output:
```json
{
  "observations": [
    {
      "record_id": "agent_memory:v1:obs-a",
      "text": "Claim A",
      "temporal_status": "superseded",
      "superseded_by": [
        {
          "record_id": "agent_memory:v1:obs-c",
          "handle": "agent:test:session-c"
        }
      ]
    },
    {
      "record_id": "agent_memory:v1:obs-b",
      "text": "Claim B",
      "temporal_status": "superseded",
      "superseded_by": [
        {
          "record_id": "agent_memory:v1:obs-c",
          "handle": "agent:test:session-c"
        }
      ]
    },
    {
      "record_id": "agent_memory:v1:obs-c",
      "text": "Claim C (current)",
      "temporal_status": "current"
    }
  ],
  "excluded": []
}
```

---

## Daemon Integration

The daemon endpoint support query parameter `"supersession": "exclude" | "include-but-flag"` for the `observations_for_symbol` verb.

Example JSON request body to daemon:
```json
{
  "request_id": "req-1",
  "agent_id": "my-agent",
  "verb": "observations_for_symbol",
  "params": {
    "name": "my_function",
    "supersession": "include-but-flag"
  }
}
```
