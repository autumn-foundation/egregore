# `eg link-evidence`

Links imported agent evidence to deterministic code-graph facts.

## Synopsis

```
eg link-evidence --code-graph <path> --evidence <path> --out <path>
```

## Description

Reads a code-graph JSONL (produced by `scan`) and an agent-evidence JSONL
(produced by `import-traj` or `import-codex`) and resolves every unambiguous
repo-relative file or symbol handle in the evidence to a stable code-graph record ID.

Resolved handles emit cross-domain edge records in the output JSONL:

| Evidence kind | Field resolved | Edge emitted |
|--------------|---------------|--------------|
| `FileEdit` | `repo_relative_path` | `TOUCHED_FILE` → `File` node |
| `PatchArtifact` | `repo_relative_path` or `target_files` | `TOUCHED_FILE` → `File` node |
| `Failure` | `repo_relative_path` (when set) | `FAILED_ON` → `File` node |
| `Observation`, `Decision` | `name` (unambiguous only) | `MENTIONS_SYMBOL` → `Symbol` node |

**Unresolved handles** are written to stderr as machine-readable JSON diagnostics
(one object per line). The output JSONL is not written to stderr and contains
only resolvable edges.

## Shortest local workflow

```bash
# 1. Build a code graph from the repository
eg scan . --out code_graph.jsonl

# 2. Import an agent trajectory
eg import-traj session.traj --out agent_session.jsonl

# 3. Resolve evidence links (resolvable → linked.jsonl, unresolved → stderr)
eg link-evidence \
  --code-graph code_graph.jsonl \
  --evidence agent_session.jsonl \
  --out linked.jsonl

# 4. Inspect the resolved links
eg inspect linked.jsonl

# 5. Ingest into a local store for later querying
cat code_graph.jsonl agent_session.jsonl linked.jsonl > combined.jsonl
eg ingest combined.jsonl --adapter embedded --data-dir .egregore
```

## Inspecting resolved vs. unresolved evidence links

Resolved links are edges in `linked.jsonl`:

```bash
# Count resolved edges by label
grep '"label"' linked.jsonl | sort | uniq -c

# View TOUCHED_FILE links
grep '"TOUCHED_FILE"' linked.jsonl
```

Unresolved handles appear as JSON diagnostics on stderr:

```bash
# Capture diagnostics
eg link-evidence --code-graph code_graph.jsonl \
  --evidence agent_session.jsonl \
  --out linked.jsonl 2>diagnostics.ndjson

# Inspect unresolved by reason
grep '"missing_file"' diagnostics.ndjson
grep '"ambiguous_symbol"' diagnostics.ndjson
```

Each diagnostic object has:

| Field | Description |
|-------|-------------|
| `source_record_id` | ID of the evidence node with the unresolved handle |
| `source_handle` | Artifact path/hash of the source evidence, when set |
| `repo_relative_path` | File path that was attempted (for file diagnostics) |
| `symbol_name` | Symbol name that was attempted (for symbol diagnostics) |
| `reason` | Why it failed: `missing_file`, `missing_symbol`, `ambiguous_symbol`, `wrong_repo`, `stale_span` |
| `attempted_relation` | The edge label that would have been emitted |

## Trust model

**Unresolved subjective memory is not source truth.**

Agent evidence always carries provenance (`domain: "agent_memory"`) and is
kept strictly separate from deterministic code-graph facts (`domain: "codegraph"`).
The linker enforces this separation:

- `Verification` nodes are **never** linked via `TOUCHED_FILE`. Verification
  truth stays in the verification domain.
- A subjective `Observation` is **never** promoted to verified status by
  `link-evidence`. Only a `Verification` evidence record connected via
  `VALIDATED_BY` represents a verified claim.
- Symbol-name matches are only emitted as `MENTIONS_SYMBOL` edges when
  **exactly one** symbol in the code graph has that name. Ambiguous name-only
  matches produce diagnostics instead of guessed links.

## Output format

The output JSONL contains only `GraphRecord::Edge` records with:

- `record_type: "edge"`
- `schema_version: 1` (agent-memory schema version)
- IDs with the `agent_memory:v1:` prefix
- Labels from the existing cross-domain edge registry only

The output can be ingested into a temporary embedded store with `eg ingest`.

## Diagnostic reasons

| Reason | Meaning |
|--------|---------|
| `missing_file` | `repo_relative_path` does not match any `File` node in the code graph |
| `missing_symbol` | Symbol `name` does not match any `Symbol` node |
| `ambiguous_symbol` | Symbol `name` matches more than one symbol; unambiguous handle required |
| `stale_span` | Span-based target attempted but no span index available |
| `wrong_repo` | Evidence repository identity does not match the code graph |

## Edge labels used

All edges use labels from the existing cross-domain registry
(`docs/schema/agent-memory.md`). `link-evidence` does not introduce
new edge labels or a new trust model.

| Label | Wire value | Used for |
|-------|-----------|---------|
| `EdgeLabel::TouchedFile` | `TOUCHED_FILE` | FileEdit, PatchArtifact → File |
| `EdgeLabel::FailedOn` | `FAILED_ON` | Failure → File |
| `EdgeLabel::MentionsSymbol` | `MENTIONS_SYMBOL` | Observation/Decision → Symbol |

## Performance

On the seeded Rust/agent-memory fixture, the link-and-inspect workflow completes
in well under 2 seconds on any modern laptop. The resolver is purely in-memory
(no network, no embeddings) and scales linearly with the size of the JSONL files.
