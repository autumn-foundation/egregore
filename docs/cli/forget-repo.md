# eg forget-repo

`eg forget-repo <selector>` logically evicts EVERY record belonging to ONE
repository from a shared multi-repo embedded store, across every domain — code
facts, semantic drift, agent memory, project/task, artifact, verification, and
log — leaving co-resident repositories byte-identical (issue #248).

This is the **sanctioned bulk exception** to issue #231's rule that deterministic
code facts are never tombstoned. `eg forget` corrects a single wrong or sensitive
fact and REFUSES code-graph, semantic, and temporal records; `eg forget-repo`
offboards a whole repository as a unit. Eviction is **not** record-level
tombstoning of individual facts: it is a repository-scoped operation whose
audit trail is one event, not one decision per fact.

Eviction is **logical**, exactly like `eg forget`: the bytes stay in the store
for bi-temporal history views, while every current-state read/serving lane drops
the repository. There is no physical delete (the `aletheiadb` API exposes none).

## Synopsis

```powershell
# Dry-run is the DEFAULT: read-only, prints the eviction plan, mutates nothing.
eg forget-repo acme/widget --data-dir .egregore --reason "offboarded customer"

# Perform the eviction.
eg forget-repo acme/widget --data-dir .egregore --reason "offboarded customer" --confirm

# Re-running is an idempotent no-op success (action: already_evicted).
eg forget-repo acme/widget --data-dir .egregore --reason "anything" --confirm

# Any identity handle resolves the repository.
eg forget-repo https://example.com/acme/widget --data-dir .egregore --reason "x" --confirm
eg forget-repo codegraph:v6:<hex> --data-dir .egregore --reason "x" --confirm

# Unknown / ambiguous selectors exit 2.
eg forget-repo no-such-repo --data-dir .egregore --reason "x"   # unknown_repository_selector
eg forget-repo widget --data-dir .egregore --reason "x"         # ambiguous_repository_selector
```

| Option | Meaning |
|--------|---------|
| `<selector>` | Repository selector: record ID, `owner/name`, basename, remote URL, root commit SHA, or canonical path (reuses `RepositoryIndex::resolve_selector`). |
| `--data-dir` | Embedded `AletheiaDB` data directory (default `.egregore`). |
| `--reason` | Required eviction reason, recorded on the event (redaction policy v1 applies). |
| `--evicted-by` | Operator handle recorded as the actor (default `operator`). |
| `--transaction-time` | Fixed RFC 3339 instant for deterministic output; defaults to now. |
| `--confirm` | Perform the eviction. **Dry-run is the default** — without this flag the command is strictly read-only, takes no write lease, and mutates nothing. |

`--confirm` needs exclusive store access (the ordinary embedded write lease); a
running daemon holding the lease is refused with `store_contended`. Dry-run reads
a throwaway snapshot copy and never takes the lease.

## Exit codes

| Code | Condition |
|------|-----------|
| 0 | Dry-run plan; successful eviction; idempotent no-op (`already_evicted`). |
| 1 | Malformed request field: empty `--reason` (`missing_reason`), empty `--evicted-by` (`missing_evicted_by`), invalid `--transaction-time` (`invalid_transaction_time`). |
| 2 | `unknown_repository_selector` (no repository matches) / `ambiguous_repository_selector` (more than one matches; candidates listed). |

The error envelope is printed to stderr as the last non-empty line:
`{"ok":false,"error":{"code":"...","detail":{...}}}`. For an ambiguous selector,
`detail.candidates` lists every matching repository record ID (sorted).

## Success envelope (stdout, one JSON line)

```json
{"ok":true,"action":"dry_run",
 "repository":{"id":"codegraph:v6:...","display":"acme/widget"},
 "evicted_by":"op-1","reason":"offboarded customer","transaction_time":"...",
 "planned":{"total":15,
   "by_domain":{"agent_memory":2,"artifact":2,"codegraph":4,"log":1,
     "project":2,"semantic":2,"verification":2},
   "representative_ids":["..."]},
 "unattributable":{"total":0,"by_domain":{},"representative_ids":[]},
 "shared_cross_repo":{"total":0,"representative_ids":[]},
 "cross_repo_citations":[]}
```

`--confirm` sets `"action":"evicted"` and adds
`"eviction":{"event_id":"agent_memory:v1:...","tombstone_count":N}`. A re-run
sets `"action":"already_evicted"`. Every list is sorted and the whole envelope is
byte-identical across runs under a pinned `--transaction-time`.

## Cross-domain attribution

1. **Seed** the owned set from `RepositoryIndex` (read-only reuse): code-graph
   containment (`owner_of` over `CONTAINS`/`DEFINES`/`IMPORTS`), `SemanticDrift`
   (via `DRIFTS_FROM`), and log records (the `repository_id` payload field,
   issue #362).
2. **Extend** by walking the cross-domain **evidence** subgraph undirected from
   the seed. The traversed label set is an exhaustive partition of every
   `EdgeLabel` variant (a `match` with no wildcard arm — the #247 completeness
   invariant), mirroring the `evidence-path` evidence set: evidence links, log
   topology, and the project registry are TRAVERSED; code-graph topology and
   intra-agent-memory / semantic scaffolding are EXCLUDED. A non-seed record
   reached from exactly ONE repository is attributed to it; the walk never
   expands through another repository's seed node, so ownership never bleeds
   across repositories.
3. Each attributed content record is tombstoned; the containment edges from the
   repository to its evicted content are tombstoned too.

The **repository identity node is emptied, not tombstoned**: its content is
evicted but the `Repository` record survives, so a scoped query
(`eg query symbol <name> --repo <selector>`) resolves the now-empty repository
and returns a clean no-match (exit 2) rather than an "unknown selector" error.

## The eviction event

Exactly one auditable event is written per `--confirm` run: a reused
`NodeKind::Retraction` node (agent-memory domain, **no schema bump**) whose prior
handle (`source_handle`) is the repository identity record ID, `agent_id` is the
redacted `--evicted-by`, `text` is the redacted `--reason`, and
`transaction_time` is the pinned/`now` instant. Its record ID is deterministic in
a distinct namespace from #231 retractions
(`agent_memory_stable_id(["node","repo_eviction",<repo_id>])`), so the two event
kinds never collide. Each evicted record additionally gets its own domain-scoped
tombstone whose ID is a pure function of the target handle. The event is written
before the tombstones so the deletions are the latest writes and stay active.

## Honest-gap contract

Attribution is never guessed. A record with **no derivable attribution** — a
legacy `log:v2:` `ErrorSignature` with an empty `repository_id`, an orphan
artifact with no evidence edge — is REPORTED under `unattributable` and NEVER
evicted. A record reachable from TWO OR MORE repositories is SHARED, reported
under `shared_cross_repo`, and NEVER evicted. A **surviving** record that merely
cites an evicted handle over an evidence edge is KEPT; the now-dangling link is
reported under `cross_repo_citations`, never silently dropped or cascade-evicted
(consistent with `eg forget`'s stale-evidence discipline).

## Temporal-store residual (scan-history)

On a **non-temporal** (`eg scan`) store, eviction is clean and complete: a base-ID
tombstone excludes each evicted node (and its edges) from every current-state
read. On a **`scan-history`** store, code nodes are commit-anchored (temporal):
the embedded current-state read re-emits every per-commit snapshot regardless of
tombstones (the same mechanical wall that makes `eg forget` REFUSE temporal
records). A base-ID tombstone therefore does not suppress an evicted repository's
temporal code snapshots from the current-state serving lanes, and no per-commit
version ID exists to tombstone at the read layer. This slice evicts the
non-temporal records and reports the residual; a physical purge of temporal
snapshots would require an `aletheiadb` capability that does not exist. Bi-temporal
honesty actually WANTS the historical `--at <commit>` snapshots preserved — the
eviction is recorded as a transaction-time fact by the audit event.

Read-only in dry-run, redaction-safe (record IDs, domains, kinds, handles,
counts, and the redacted reason/actor only), deterministic and byte-identical
across runs.
