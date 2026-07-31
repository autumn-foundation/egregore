# ADR 0006: AletheiaDB 0.2.0 Namespaces for Per-Repository Scoping

## Status

Accepted — evaluation complete, **not adopting** namespaces as the per-repository
scoping boundary in this slice. Issue
[#485](https://github.com/autumn-foundation/egregore/issues/485). Follows the
`AletheiaDB` 0.1.1 → 0.2.0 upgrade (#488), which linked the namespace surface
without enabling it.

Executable evidence: `tests/integration/namespace_evaluation.rs`.

Citations of the form `src/db/…`, `src/core/…`, `src/api/…`, `src/tenant/…`,
`src/query/…` are into the **vendored 0.2.0 crate**
(`aletheiadb-0.2.0`, `Cargo.lock` checksum
`e905efaa187e9d1048b82b373928e4d078607d0f8852349ad184e0edf3810fba`). Citations
naming an Egregore file (`src/adapters/…`, `src/query/repo.rs`,
`src/repo_evict.rs`, `src/embeddings.rs`, `src/languages/…`, `src/cli/…`) are
into this repository. **Honest limit:** the ADR's doc-lock test asserts that key
identifiers and paths appear in this file; it does **not** machine-verify line
numbers. Upstream line drift on a version bump will not fail CI — re-check these
citations when the `aletheiadb` pin moves (see `docs/cli/store-upgrade.md`).

## Context

Egregore supports shared multi-repository embedded stores, and `--repo <selector>`
scoping is re-derived per lane from `RepositoryIndex` attribution
(`src/query/repo.rs`). 0.2.0 ships namespaces (upstream #3349): an
ownership/visibility axis orthogonal to the type label, with namespaced writes,
scoped reads, a membership index, a traversal boundary, and per-namespace counts.

Issue #485 asks whether a namespace per repository should push that boundary into
the engine, so scoped reads are enforced once rather than by each lane's filter.
It names four open questions and requires a written answer before any
implementation.

## What 0.2.0 actually provides

| Capability | Where | Evaluated? |
| --- | --- | --- |
| Registry: `create_namespace` / `list_namespaces` / `get_namespace` / `describe_namespace` / `delete_namespace` | `src/db/namespace.rs:439,452,463,474,485` | Yes |
| Namespaced writes: `create_node_in_namespace` / `create_edge_in_namespace`, plus `WriteRequestOptions::with_namespace` inside a transaction | `src/db/namespace.rs:522,552`; `src/api/transaction/mod.rs:137` | Yes |
| Scoped current-state reads: `get_node_scoped`, `get_edge_scoped`, `list_nodes_scoped`, `find_nodes_by_property_scoped` | `src/db/namespace_query.rs:140,157,178,221` | Yes |
| Scoped bi-temporal reads: `get_node_at_time_scoped`, `get_edge_at_time_scoped`, `find_nodes_at_time_scoped`, `find_nodes_by_property_at_scoped` | `src/db/namespace_query.rs:488,513,539,564` | Yes (Q1) |
| Traversal boundary: `traverse_scoped[_directed]`, `traverse_scoped_as_of[_directed]` | `src/db/namespace_query.rs:251,292,609,645` | Yes (Q1, Q3) |
| Scoped vector search: `find_similar_scoped`, `find_similar_by_embedding_scoped` | `src/db/namespace_query.rs:422,454` | Yes — see "the one real capability gain" |
| Per-namespace counts: `namespace_counts()` | `src/db/namespace_query.rs:726` | Yes |
| AQL / Cypher `USE` / `IN NAMESPACE` read scope: `execute_aql_scoped`, `execute_cypher_scoped`, `QueryBuilder::in_namespace(s)`, `ScopeFilterIterator` | `src/db/query.rs:69,604,733`; `src/query/builder.rs:901,918,932`; `src/query/executor/iterators.rs:1955-2048` | **Not evaluated** — Egregore issues no AQL or Cypher; it drives the typed Rust API only. Noted because #485 enumerated it. |
| Changefeed namespace filter: `ChangeFilter.namespace`, `ChangeRecord.namespace` | `src/core/changefeed.rs:101-109`; `src/core/changefeed_subscription.rs:241` | **Not evaluated** — Egregore consumes no changefeed. Noted because #485 enumerated it. |
| Tenant surface: `create_node_in_namespace` per tenant, per-tenant `backup`/`restore_tenant`, and `delete_tenant` (deregister **plus** `remove_dir_all` of the tenant directory) | `src/tenant/mod.rs:397,442,519,739-784,796` | Surveyed — see Alternatives; **this**, not `delete_namespace`, is 0.2.0's physical-purge primitive |

Model properties the answers turn on:

- A namespace is **exactly one per entity, fixed at creation, immutable for the
  entity's life** (`src/core/namespace.rs:6-7`).
- It rides an engine-reserved property key (`__aletheia_ns`,
  `src/core/namespace.rs:49`). A user write carrying a reserved key is rejected by
  `reject_reserved_keys` (`src/core/namespace.rs:716-721`) at the write seam
  (`src/api/transaction/write/mod.rs:1674` and the `replace_*`/CAS entry points),
  so a namespace cannot be forged or rewritten through the public API.
- The `default` namespace is never stamped, so existing data stays byte-identical
  and adoption is inert until opted into (`src/core/namespace.rs:20-23,751`).
- Name charset is `[A-Za-z0-9._:/-]`, max 128 bytes
  (`src/core/namespace.rs:80,163`) — an Egregore repository record ID
  (`codegraph:v8:<hex>`, `SCHEMA_VERSION` = 8 in `src/ir.rs:12`) and an
  `owner/name` handle both fit.

## The four open questions

### Q1 — Does the boundary compose with bi-temporal reads and `--at` / `--as-of`?

**Yes, including the history-replay read paths — with one bounded caveat that is
about sampling, not about correctness.**

Scoped point-in-time reads reconstruct at `(valid_time, transaction_time)`
**first**, then filter by the reconstructed entity's namespace; because the
namespace is immutable across an entity's whole history that filter is stable
under later writes (`src/db/namespace_query.rs:474-502,539-587`). The scoped
as-of traversal applies the same rule per hop
(`src/db/namespace_query.rs:609-717`). Premise locked by
`upstream::namespace_scope_composes_with_bitemporal_point_in_time_reads` and
`upstream::scoped_as_of_traversal_does_not_cross_the_namespace_boundary`.

Two refinements matter, and an earlier draft of this ADR got both wrong.

**Scoped historical enumeration exists, but samples.**
`find_nodes_at_time_scoped` / `find_nodes_by_property_at_scoped` do enumerate
entities that are gone from current state — "nodes since deleted from current
state are still found when both dimensions anchor before the deletion"
(`src/db/ops.rs:1334-1345`). But the candidate set is capped at
`max_schema_as_of_entities`, **default 50,000**, and when the cap bites the call
keeps the lowest `cap` node ids and sets `NodesAtTime::sampled`
(`src/db/ops.rs:1347-1358`). For a `scan-history` store — the case with the most
entities — a scoped historical read would be a **disclosed sample**, not a
complete answer. Egregore's lanes require completeness, so this API could not
replace the adapter's enumeration; it post-filters anyway
(`src/db/namespace_query.rs:547-554,574-587`), so it also saves nothing.

**The current-state membership caveat does not bite Egregore.** Upstream's cheap
enumeration path (`list_nodes_scoped` via the membership index) is current-state,
so a *deleted* entity is not listable by namespace — locked by
`upstream::namespace_membership_enumeration_is_current_state_only`. Egregore
never creates that condition: it issues **zero** engine deletes (no `delete_node`
/ `delete_edge` call exists in `src/`), deletion is entirely logical via
tombstone records that are themselves live nodes, and each commit snapshot is a
**distinct live node** keyed in the adapter's own by-commit index
(`src/adapters/aletheiadb.rs:129-130,163-167`). So the population
`list_nodes_scoped` would enumerate is exactly the population
`get_all_node_ids()` walks today (`src/adapters/aletheiadb.rs:574-671,2130-2132`),
history included. Locked by
`egregore::tombstoned_records_remain_live_engine_nodes`.

That makes scoped enumeration *more* applicable to `scan-history` stores than the
first draft claimed, not less. **Honest limit:** the equivalence holds only while
Egregore issues no engine delete. If a future physical-eviction mechanism (the
#472 direction) ever calls one, scoped enumeration would silently stop seeing
those versions, and this ADR's cost analysis would need redoing.

### Q2 — Where do repo-agnostic records live, and can a scoped read still see them?

**They would sit in `default`, and a repository-scoped read cannot see them
unless every scope explicitly unions `default` in — which re-admits every other
repository's `default` residue at the same time.**

An out-of-scope entity is deliberately **indistinguishable from a missing one**
(`src/db/namespace_query.rs:127-135`): `get_node_scoped` returns `NodeNotFound`
rather than a permission error, so a scoped caller cannot learn that an
out-of-scope entity exists. That is the right security default and the wrong
default for our repo-agnostic records. Locked by
`upstream::scoped_read_hides_default_namespace_records_unless_unioned`.

**Which records are repo-agnostic?** Bounded survey, by construction rather than
by sampling a store — the classes are those whose stable ID does not take a
repository as an input, plus those attribution deliberately refuses to resolve:

1. The `EmbeddingModel` vector-index identity (#104) — describes the **store's**
   single vector index, minted at a fixed repo-agnostic ID
   (`src/embeddings.rs:540-556`). Locked by
   `egregore::repo_agnostic_embedding_index_identity_has_no_repository_owner`.
2. `unattributable` records — no derivable owner at all: a legacy `log:v2:`
   signature with an empty `repository_id`, an orphan artifact
   (`src/repo_evict.rs:662-690`).
3. `shared_cross_repo` records — reachable from **two or more** repositories, so
   `default` would merge "belongs to everyone" with "belongs to nobody"
   (`src/repo_evict.rs:662-690`). Locked by
   `egregore::a_record_reached_from_two_repositories_is_shared_not_owned`.

Class 1 is load-bearing: if a repository-scoped read failed to union `default`,
the #104 identity gate would stop finding the identity and refuse every semantic
query with `embedding_identity_unrecorded` (exit 7) on a perfectly healthy store.
The mitigation ("always union `default`") is a rule every future lane must
remember — exactly the class of per-lane discipline this issue hoped to
eliminate.

### Q3 — Does the traversal boundary break `eg query evidence-path`?

**The engine's traversal boundary is not the risk, because Egregore never uses
engine traversal. The entity filter on the corpus load is the risk, and it is
real.**

`eg query evidence-path` is deliberately repo-agnostic: a witness chain may
legitimately cross repositories (`src/query/evidence_path.rs`), and the lane is an
in-process walk over records already loaded into memory. No `traverse_scoped*`
call — indeed no `NamespaceScope` reference of any kind — exists anywhere in
`src/`. So adopting the engine boundary would not truncate that walk directly.

It would truncate the walk's **input**. Every embedded query lane loads its corpus
through the adapter's bulk reads (`read_all_records*`,
`src/adapters/aletheiadb.rs:574,840,1213,1291`); if that read were
namespace-scoped, the far endpoint would not be in the record set, and — per the
upstream semantics cited in Q2 — would be indistinguishable from absent. The lane
would answer `endpoint_not_found` (exit 2) for a witness chain that genuinely
exists.

`evidence_path_regression::evidence_witness_chain_spans_two_repositories_and_a_pruned_corpus_loses_it`
**illustrates the shape** of that failure end-to-end: a two-hop cross-repository
witness over the full corpus, `endpoint_not_found` over a corpus pruned to one
repository's attributed records. It is deliberately *not* presented as proof
about namespaces — it prunes a JSONL corpus by hand and involves no
`NamespaceScope`. Treating it as more would be exactly the lead-as-proof error
this project refuses everywhere else. The conditional claim — that a
namespace-scoped bulk read would produce this corpus — follows from the adapter
structure cited above, and is reasoned, not measured.

A wrong `endpoint_not_found` presented as a verdict is the failure mode this
project treats as worst: an honest-looking answer that is false. Any adoption
would therefore have to exempt the repo-agnostic lanes from scoping by hand —
again, per-lane discipline.

### Q4 — Migration: can an existing shared store's `default` records be re-stamped?

**No re-stamp exists. In practice this means re-ingest into a fresh
`--data-dir`, but for a sharper reason than "the API refuses".**

There is no public re-stamp. `restamp_namespace` is `pub(crate)` and exists to
**preserve** an entity's existing namespace across PATCH / `replace_*` / CAS /
lease-claim updates — never to change it (`src/core/namespace.rs:761-773`), and
the immutability is wired at every update entry point. Forging the ride-along key
is rejected (`src/core/namespace.rs:716-721`). Locked by
`upstream::existing_entities_cannot_be_restamped_into_a_namespace`.

The naive inference — "so an existing store can never be namespaced" — is too
strong, and worth stating precisely because it is the kind of overreach that
makes an ADR untrustworthy. Egregore's embedded ingest is physically
**append-only**: a re-ingest appends a *new* physical node per record version via
`create_node` (`src/adapters/aletheiadb.rs:2651`) and reads collapse by
`codegraph_id` to the latest write (`src/adapters/aletheiadb.rs:574-671`).
Namespace immutability binds a *physical entity*, not an Egregore logical record,
so newly written versions **could** be stamped into an existing data dir.

What actually forces a fresh `--data-dir` is that every pre-existing physical
version — including all of a `scan-history` store's commit snapshots, which are
the bulk of it — stays in `default` permanently. The store is then irreversibly
mixed: any scoped read that touches history sees `default`-namespaced versions it
must either union in (defeating the boundary) or silently drop (a false answer).

Two further migration costs the issue did not ask about but an adopter would hit:

- **`eg export` round-trips lose namespaces.** Canonical JSONL has no namespace
  field, so `eg export` → re-ingest silently drops every stamp. The documented
  export/ingest parity contract would still hold for record counts but no longer
  for scoped reads.
- **`delete_namespace` deletes the registration, not the entities**
  (`src/db/namespace.rs:287-291,478`).
  Namespaces are **not a route to physical eviction** and do not close #472. Locked by
  `upstream::delete_namespace_leaves_its_entities_intact`. (0.2.0's actual
  physical purge is `TenantManager::delete_tenant`, `src/tenant/mod.rs:739-784`
  — a different axis; see Alternatives.)

## Decision

**Not adopting** namespaces as the per-repository scoping boundary in this slice.
The rejection is specifically of *namespace-as-the-enforcing-boundary*; the
narrower accelerator shape is deferred, not rejected (see Alternatives).

### 1. The hard half of attribution has no write-time answer

Repository attribution is **derived at read time** by `RepositoryIndex` from graph
closure — `CONTAINS`/`DEFINES`/`IMPORTS` containment, the `DRIFTS_FROM` target for
`SemanticDrift`, the persisted `repository_id` for log records
(`src/query/repo.rs:161-262`).

Two important qualifications, both of which an earlier draft got wrong. For
**codegraph** records the repository is known at mint time — `repository_id` is an
input to the stable ID itself (`src/languages/rust.rs:1611-1619`, and the same
parameter threaded through `python.rs` / `go.rs` / `cross_file.rs`), so the writer
must know it to compute the ID at all. For **log** records it has been a persisted
field since #362. Those halves could be stamped honestly.

The cross-domain records could not. An agent `Observation`, an artifact, a
verification record — these acquire a repository only through evidence-edge
closure, which can land in a **later** ingest (`eg refresh` / `eg watch` /
`eg link-logs` / `eg resolve-frames`), can resolve to **two** repositories
(`shared_cross_repo`), or to **none** (`unattributable`). Premise locked by
`egregore::repository_index_requires_the_containment_edge_to_attribute`.

So a namespace boundary would cover the half that was never the problem and leave
the half that is — the cross-domain records — in `default`, where every scope must
union them back in.

### 2. A namespace holds one value; two of our attribution classes are not one-valued

`eg forget-repo` (#248) partitions records into owned, `shared_cross_repo`
(reachable from ≥2 repositories — never evicted), and `unattributable` (no
derivable owner — never evicted, always *reported*)
(`src/repo_evict.rs:662-690`). Both non-owned classes are inexpressible as a
namespace, and collapsing them into `default` would destroy a distinction the
privacy story depends on.

### 3. Only `--data-dir` has namespaces, so the boundary can never decide an answer

Egregore answers the same question over a JSONL graph and over an embedded store.
A JSONL graph has no namespaces, so the `RepositoryIndex` filter must exist and
run anyway for `--graph`; an engine boundary can only ever be a second, redundant
mechanism.

This is a **standing synchronization obligation**, not by itself a correctness
regression — the accelerator shape below discharges it with an equality
obligation, and the repo already tolerates disclosed divergences elsewhere (the
`embedded_log_retention_caveat` on `log-deltas` / `error-context` is a documented
`--graph` vs `--data-dir` difference). The argument is therefore a cost argument:
every lane that opts in must be kept in sync forever, by test, and a *scoping*
divergence is worse than a *retention* divergence because it changes which
records exist in the answer rather than how many observations were coalesced.

### 4. Migration is a one-way door on every existing store

Per Q4: pre-existing physical versions stay in `default` permanently, and
`eg export` round-trips drop stamps. Adoption means re-ingesting every existing
shared store, and there is no way back.

### The benefit is real, and unmeasured

Being fair to the other side:

- **Scoped enumeration would work**, including on `scan-history` stores (Q1). On a
  shared store with N repositories, a `--repo`-scoped bulk read could skip the
  other repositories' nodes. The embedded CLI even knows `--repo` *before* it
  opens the store — each query lane opens per invocation
  (`src/cli/records.rs:117,144,226,312,370`, `src/cli/inspect.rs:184`,
  `src/cli/export.rs:60`), so the open-time index rebuild could be scoped too. Only
  the long-lived daemon, which shares one handle across lanes, could not.
- **`find_similar_scoped` is the one real capability gain, not just a speed-up.**
  Post-filtering a top-K ANN result is *lossy*: `eg query semantic --repo X
  --limit N` today over-fetches and escalates the candidate pool
  (`src/adapters/aletheiadb.rs:460-472,4835-4859`) because top-K-then-filter can
  return fewer than N in-repo hits — and that escalation is **bounded**, so on a
  large shared store the lane can still under-return. `find_similar_scoped` is
  filter-complete by construction: it over-fetches until it has K genuinely
  in-scope results (`src/db/namespace_query.rs:400-441`) — "never 'take `k`, then
  drop the out-of-scope ones leaving fewer than `k`'". This is the engine-level
  form of the same requirement `--under` (#198) already imposes: scope the pool
  *before* the top-N cap. It is the strongest argument for adoption in this
  document.
- **`namespace_counts()`** would make `eg inspect --data-dir` per-namespace totals
  an O(1) read.

**No benchmark was run for this evaluation.** No store size, record count, or
timing appears anywhere in this document, and the phrase "the cost win is modest"
does not appear because it would be unearned. The decision rests on the four
structural costs above, not on a claim that the benefit is small.

## What we do instead

Nothing changes in this slice. `--repo` scoping stays derived from
`RepositoryIndex`, which already works identically over `--graph` and
`--data-dir`, already models the shared/unattributable classes honestly, and
already tolerates attribution arriving late.

The cross-repo-bleed risk #485 names is real and this ADR does not close it. It is
a *coverage* problem rather than a *mechanism* problem — every scoped lane should
route through the shared `RepositoryIndex` helpers, and a test should assert that
each one does. **That work is not done here and has no owner**; it needs its own
issue. Recording it as an open follow-up rather than implying it is handled:

> **Open follow-up (unfiled):** a lane-coverage test asserting every `--repo`-
> accepting lane applies `RepositoryIndex` scoping, so a new lane cannot forget.

The honest counter-argument to "coverage, not mechanism" — that mechanisms exist
precisely because coverage problems recur, and that this codebase is fail-closed
about privacy elsewhere (`eg forget-repo` refuses to evict `shared_cross_repo`) —
is real. It is outweighed here only because the mechanism on offer covers the easy
half of attribution and not the hard half (Decision §1), not because
defence-in-depth is unwanted.

## Revisit triggers

Each trigger names how it would actually be noticed. Upstream additions are **not**
caught by CI — the evidence tests lock current behavior against regression, not
against new APIs — so triggers 1 and 2 are checklist items for the next
`aletheiadb` version bump (`docs/cli/store-upgrade.md`), not automatic.

1. **Upstream ships namespace re-stamping, or a namespace-axis physical purge.**
   Re-stamping removes the one-way-door migration cost (Q4, Decision §4). A
   namespace-axis purge would additionally make namespaces relevant to #472, which
   today they explicitly are not. *Detector:* upgrade checklist — diff
   `src/db/namespace*.rs` for a new public API.
2. **Upstream removes the `max_schema_as_of_entities` sampling cap on scoped
   historical reads, or adds a membership-indexed historical enumeration.**
   *Detector:* upgrade checklist — re-read `src/db/ops.rs:1334-1358`.
3. **Egregore gives cross-domain records a retrievable `repository_id` at mint
   time.** Note what is already true: the codegraph domain has the repository at
   mint time (it is an ID input) and log records have carried a retrievable
   `repository_id` since #362. What is missing is a *retrievable* field on
   agent-memory / artifact / verification records, plus a decision on what to
   write for the genuinely shared and genuinely unattributable cases. This is the
   single change that would flip Decision §1. *Detector:* whoever implements it
   reopens this ADR.
4. **Measurement.** A benchmark on a shared multi-repo store showing the
   open-time `get_all_node_ids()` rebuild plus the bulk read consuming **more than
   half** the wall time of a `--repo`-scoped lane, on a store of at least 1M
   records. *Detector:* none today — this needs a benchmark harness that does not
   exist; adding one is a prerequisite for firing this trigger, not a side effect.
5. **A demonstrated under-return from `eg query semantic --repo`.** A case where
   the bounded over-fetch ladder returns fewer than `--limit` in-repo hits that a
   filter-complete scoped search would have found. That is a correctness gap the
   `RepositoryIndex` filter cannot close, and would justify adopting
   `find_similar_scoped` on its own, independent of the rest of this decision.

If revisited, the only shape compatible with the `--graph` contract is **namespace
as a pure accelerator**: the scoped read may only *prune candidates the
`RepositoryIndex` filter would have dropped anyway*, never decide the answer, with
two obligations enforced by test on every lane that opts in —

- **Equality:** `scoped == unscoped-then-filtered`, byte for byte.
- **Determinism:** the scoped path must preserve the lane's documented ordering.
  `list_nodes_scoped` ends in `sort_unstable` by node id
  (`src/db/namespace_query.rs:209`), which is deterministic but is *not* the
  adapter's current emit order — so a scoped bulk read would have to re-establish
  ordering explicitly rather than inherit it.

`find_similar_scoped` (trigger 5) is the one exception worth considering
separately: there, scoping is not an accelerator but a completeness fix, and the
`--graph` transport has no vector index at all, so the agreement contract does not
bind it the same way.

## Alternatives considered

- **Namespace per repository, enforcing (full adoption).** Rejected — Decision
  §1-§4.
- **Namespace only the records whose owner is known at write time** (codegraph +
  log), leaving cross-domain records in `default`. This survives §2 (the
  non-one-valued classes are never stamped) and, combined with the accelerator
  constraint, §3. Rejected for this slice because it still pays §4's one-way-door
  migration in full while delivering a boundary that, by construction, does not
  cover the records whose mis-scoping would actually leak — and because every
  scope must still union `default`, which re-admits them anyway.
- **Namespace as a pure accelerator on `--data-dir` only.** Deferred, not
  rejected. Preserves the agreement contract by construction; costs a permanent
  synchronization obligation. Trigger 4 (or 5, for the vector lane) is what would
  make it worth the trade.
- **Tenant per repository.** 0.2.0's tenant surface is the only thing in the
  release that physically purges (`delete_tenant` deregisters and
  `remove_dir_all`s the tenant directory, `src/tenant/mod.rs:739-784`). Rejected
  as a scoping answer because each tenant is a **separate database in a separate
  directory** — which dissolves the shared multi-repo store this issue is about,
  and with it every cross-repository lane (`evidence-path` most obviously). Worth a
  separate look purely as a #472 (physical eviction) mechanism; that is not this
  issue.
- **Namespace on a different axis — domain, or trust class.** Genuinely deferred
  and the most promising future use. Unlike repository attribution, a record's
  `Domain` (`codegraph` / `agent_memory` / `project` / `artifact` / `verification`
  / `semantic` / `user_context` / `log` — all eight in `src/ir.rs:2939-2956`) and
  its trust class are **declared at write time, exactly one per record, and never
  revised** — precisely the model `NamespaceScope` assumes. Out of scope for #485,
  which asks specifically about per-repository scoping, and it inherits the same
  `--graph` agreement obligation, so it needs its own evaluation.
- **Wait for upstream.** What triggers 1 and 2 encode.

## Consequences

- 0.2.0's namespace surface stays **inert**: no `create_*_in_namespace` call, no
  `NamespaceScope` in any read path, no `namespaces.json` in any Egregore store.
  Existing stores keep their entities in the implicit `default` namespace with no
  ride-along key, byte-identical to pre-0.2.0 data (`src/core/namespace.rs:20-23`).
- No schema-version bump, no record-ID change, no output change. Determinism, the
  `--graph` vs `--data-dir` agreement, and byte-identical output are untouched
  because nothing is touched. (Determinism becomes a live obligation only under
  the accelerator shape; the requirement is stated in Revisit triggers.)
- The premises above are locked as executable tests in
  `tests/integration/namespace_evaluation.rs`. Those tests catch **regressions** in
  current upstream and Egregore behavior; they do not catch upstream **additions**,
  and they do not verify this ADR's line numbers.
- #472 (`eg forget-repo` leaving temporal code snapshots on scan-history stores)
  remains open and unaffected.

## A registry footnote specific to our build

Upstream persists the namespace registry as
`{persistence.data_dir}/namespaces.json` (`src/db/namespace.rs:417-424`) and
quarantines a corrupt one, restarting **empty** (`src/db/namespace.rs:126-133`),
while scope validation consults only the registry
(`src/db/namespace_query.rs:100-107`). That reads like a single point of failure,
and is not: every durable load reconciles the registry against the entities
actually present (`reconcile_namespace_registry`, `src/db/config.rs:397-417`).

In **our** build the sidecar does not exist at all. `Cargo.toml` links
`aletheiadb` with `default-features = false` and never enables `serde`, and both
halves of the sidecar — the load in `NamespaceRegistry::open` and the write in
`save_locked` — are `#[cfg(feature = "serde")]`
(`src/db/namespace.rs:139,333-341`). Registrations would be reconstructed from
entity membership on every open, and an explicitly-created **empty** namespace
would be per-process. Not disqualifying for a repository boundary — an empty
namespace there means a repository with no records — but it means
`create_namespace` could not serve as a durable declaration that a repository
exists. Locked by
`upstream::namespace_registry_does_not_persist_under_egregores_feature_set`.
