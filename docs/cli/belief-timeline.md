# Belief Timeline — `eg query belief-timeline` (issue #235)

Trace the **chronological timeline of agent-authored belief** about one code
target: every agent-authored observation-class record (`Observation`,
`Decision`, `Failure`) whose evidence links cite the target, ordered
oldest→newest, carrying the supersession/contradiction relationships their
authors wrote.

> **The arc is for reading prior reasoning, not for deciding truth.** The lane
> computes no new judgment: statuses come solely from author-written
> `superseded_by` / `SUPERSEDES` / `CONTRADICTS` data already in the store, and
> a `superseded` / `contradicted` status is a temporal-authorship lead, never a
> claim the older note is false. Mutually-contradicting notes both appear; the
> lane never asserts which side is true.

## Shortest workflow

```sh
# Against a JSONL graph:
eg query belief-timeline 'src/auth.rs#login' --graph graph.jsonl

# Against an embedded store:
eg query belief-timeline 'src/auth.rs#login' --data-dir .egregore

# Human-readable form:
eg query belief-timeline 'src/auth.rs#login' --graph graph.jsonl --format text
```

The lane is strictly read-only: it never creates, modifies, or deletes
records or indexes, and re-running the identical query against an unchanged
store yields byte-identical output.

## Code-target handles

The `<handle>` positional accepts, in this precedence order:

1. **Canonical code record ID** — `codegraph:v1:<64 hex chars>` naming a live
   `Symbol` or `File` node.
2. **`path:line`** — repo-relative file path plus a 1-based line number. The
   path must name a known code path (a live `File` or `Symbol` node carrying
   that repo-relative path); the target is the smallest enclosing `Symbol` at
   that line (narrowest span wins: line width, then byte width, then record
   ID — the same rule as `eg query file-at-point`). A line outside every
   symbol span does not resolve.
3. **Repo-relative file path** — names a live `File` node.
4. **Exact symbol name** — the existing symbol identity rule shared with
   `eg query lifeline`: one live `Symbol` node with that exact name resolves;
   several matches are ambiguous.

`--repo <selector>` restricts resolution to one repository, as on the other
code lanes. Diagnostics (exit 2 unless noted):

| Code | Meaning |
|------|---------|
| `no_match` | Well-formed handle matched nothing live. |
| `stale_handle` | Handle matched only tombstoned (retracted) records. |
| `ambiguous` | Name/path matched targets in several repositories; `candidates` lists the sorted record IDs. |
| `no_symbol_at_line` | `path:line` resolved the file but no symbol encloses the line. |
| `unsupported_handle` | Empty handle (exit 1). |

A target that resolves but has **zero** citing observations returns an
explicit empty-but-present result (`"ok": true`, `"entries": []`) — distinct
from the unresolved-target diagnostics above.

## Timeline membership

An entry appears iff it is a **live, agent-authored observation-class
record** citing the resolved target:

- kind is `Observation`, `Decision`, or `Failure` (the observation-class node
  kinds present in the agent-memory v1 schema — `Hypothesis` and `Lesson`
  have no `NodeKind` variants yet, so there is nothing of those kinds to
  return), and
- it carries `agent_id` (the schema's agent-authored marker), and
- it is not tombstoned (latest-write-wins liveness, shared with the other
  lanes), and
- it cites the target: a denormalized `evidence_links` entry with
  `target_record_id` equal to the target's record ID, **or** a graph edge from
  the record to the target (the two representations the schema requires to
  agree at write time).

Superseded and contradicted records are **included**, never filtered: the
timeline shows the whole arc, not just the chain head.

## Status model

Each entry carries a stable machine-readable `status`:

| Status | Meaning |
|--------|---------|
| `current` | No live author-written supersession or contradiction touches it. |
| `superseded` | An author-written `superseded_by` field, `SUPERSEDES` evidence link, or `SUPERSEDES` edge names a live agent-authored observation-class record that overrides it. Carries the **immediate** overrider as `forward_record_id` + `forward_handle`. |
| `contradicted` | A live `CONTRADICTS` evidence link or edge connects it to another live agent-authored observation-class record. Carries the disputing record as `forward_record_id` + `forward_handle`. Both sides of a mutual contradiction appear as `contradicted`. |

The vocabulary is closed: there is no fourth status. When the
supersession chain reachable from a record loops, each member still
carries a live supersession claim against it, so it reads `superseded`
with its **immediate** successor as `forward_record_id` + `forward_handle`,
and a stable `supersession_cycle` diagnostic (below) names the member
record IDs (sorted). The lane never loops, hangs, or truncates silently.

Rules, all deterministic:

- **Transitive chains**: given A←B←C (A superseded by B, B by C) all citing
  the target, all three appear: A and B are `superseded` (forward: B and C
  respectively — each points at the record that directly overrode it, toward
  head C) and C is `current`.
- **Precedence**: a record that is both superseded and contradicted reports
  `superseded` — the same precedence as `TemporalResolver::resolve_status`,
  shared with the recall-time supersession flag (#92) and the trust classes
  (#114).
- **Only agent beliefs dispute beliefs**: a `CONTRADICTS` link whose other
  end is not a live agent-authored observation-class record (e.g. an
  observation disputing a code symbol) does not mark the observation
  `contradicted`; likewise an overrider that is not a live agent-authored
  observation-class record never confers `superseded`. The author-written
  claim is not dropped silently — it surfaces as a
  `dangling_superseded_by` diagnostic — but the entry reads `current`,
  because no live agent belief overrides it.
- **No new judgment**: statuses are read off author-written data. Nothing is
  rewritten, hidden, or re-ranked.

## Ordering

Entries are ordered oldest→newest by a documented, deterministic temporal
key: **`observed_at`, falling back to `ingested_at`, with the record ID as
the stable tiebreaker**. Unparseable timestamps are treated as absent. The
output states the rule in a top-level `ordering` field, and every entry
carries its own `ordering_basis` (`"observed_at"`, `"ingested_at"`, or
`"record_id"`), so the basis is never implicit.

## Entry fields

Every entry exposes provenance for citation; nothing else:

| Field | Notes |
|-------|-------|
| `record_id` | Stable record ID. |
| `handle` | Citable handle: `agent_id:session_id` (or `agent_id`), the `TemporalReference` convention. |
| `kind` | `Observation`, `Decision`, or `Failure`. |
| `status` | `current` / `superseded` / `contradicted` (closed vocabulary). |
| `relation` | `SUPERSEDES` / `CONTRADICTS` when a relationship is present, else null. |
| `forward_record_id`, `forward_handle` | The overriding/disputing record, when `status` is not `current`. |
| `agent_id`, `agent_kind`, `session_id` | Authoring provenance. |
| `observed_at`, `ingested_at` | Temporal provenance. |
| `ordering_basis` | Which timestamp ordered this entry. |
| `confidence` | The record's confidence string, or null when the record omits it. |
| `evidence_handles` | Citable handle(s) of the timeline target as cited by this entry (deduped). |

**Redaction boundary**: output never includes raw transcript text, command
output, patch hunks, issue/PR bodies, env values, tokens, or protected
raw-artifact payloads. Record bodies (`text`, `summary`, `decision_text`,
`rationale_summary`, …) are never emitted — only the metadata above.

## Diagnostics

Top-level `diagnostics` (each `{code, record_ids?/record_id?, message?}`):

| Code | Meaning |
|------|---------|
| `supersession_cycle` | A supersession cycle was found; `record_ids` lists the sorted member IDs. No head exists; members read `superseded` with their immediate successor. |
| `dangling_superseded_by` | An author-written `superseded_by` names a record that is not a live agent-authored record; `record_id` is the holder, `forward_record_id` the dangling name. |

## How this differs from the sibling memory surfaces

| Surface | Command | Reads | Decides truth? |
|---------|---------|-------|----------------|
| **Plain recall** (#91) | `eg query semantic-memory <QUERY>` | Vector search over all observations by meaning | No — retrieves by relevance |
| **Recall-time supersession flag** (#92) | `--supersession <exclude\|include-but-flag>` on bulk lanes | Bulk recall results, hiding or flagging displaced records | No — filters/flags at recall time |
| **Belief timeline** (#235, this lane) | `eg query belief-timeline <handle>` | **All** observation-class records citing **one code target**, in chronological order, **including** superseded/contradicted ones | No — lays out the arc so the reader sees how beliefs changed |
| **Logical retirement** (#156) | `eg forget` | Operator-initiated retraction: tombstone + `Retraction` event | The operator's explicit act, auditable |
| **Symbol-code timeline** (#215) | `eg query lifeline <symbol>` | The **code** record's own lifecycle across commits (introduced/modified/removed) | No — code history, not agent beliefs |
| **Single-record audit** | `eg query memory <ID>` | Depth-1 graph audit of **one known** memory claim | No — inspects one claim's evidence |

The belief timeline is the only surface that answers *"what did agents
believe about this code, in order, including the beliefs they later replaced
or disputed?"* Use it to read prior reasoning before acting — never as a
verdict on which note is true.

## Output formats

`--format json` (default) emits one pretty-printed JSON document; `--format
text` prints a human-readable timeline with the same entries, ordering rule,
and diagnostics. Both are byte-identical across runs against an unchanged
store.
