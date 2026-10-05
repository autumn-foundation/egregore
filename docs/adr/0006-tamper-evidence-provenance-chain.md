# ADR 0006: Adopt AletheiaDB's Provenance Hash Chain for Store Tamper-Evidence

## Status

Accepted (evaluation decision for issue #491; implementation is follow-up work,
not part of this decision)

## Context

Issue #488 upgraded Egregore to AletheiaDB 0.2.0. That release added four
upstream capabilities that overlap Egregore's existing evidence machinery:

| Upstream capability (0.2.0) | Egregore's existing equivalent |
|---|---|
| Derivation lineage closures — version-pinned upstream/downstream fact-to-fact closures (`create_*_with_lineage`, `upstream_lineage` / `downstream_lineage`; upstream #3371) | `eg query evidence-path` (#247) — undirected shortest witness path over an explicitly partitioned evidence/provenance edge subgraph |
| Trust propagation over derivation lineage — computed confidence as a tree with per-label policy and a `trust_breakdown` surface (upstream #3382) | The trust-class system (`source_fact` / `agent_observation` / `runtime_observation` / `verification_evidence` / `project_state`) |
| Belief-revision audit — when and why the database changed its mind (upstream #3362) | Supersession + `--supersession exclude\|include-but-flag`, contradiction handling in agent memory |
| Tamper-evident provenance hash chain — hash chain over provenance with `verify_chain` / `export_chain_head` (upstream #3351) | *nothing equivalent* |

Issue #491 scoped the evaluation to **tamper-evidence only**, asking for a
decision on whether `verify_chain` can back a credible "this evidence store has
not been altered" claim in the #338 evidence-pack manifest, with the
retraction-interaction questions (Q3, Q4) answered concretely. This ADR is that
decision.

## Investigation

All findings below were verified against the pinned `aletheiadb` 0.2.0 sources
(`Cargo.lock`: `aletheiadb 0.2.0`, checksum
`e905efaa187e9d1048b82b373928e4d078607d0f8852349ad184e0edf3810fba`) and
Egregore trunk at `d092e05`.

### Q1: What does the chain cover? Are Egregore's `create_node` / `create_edge` writes chained?

The chain is an **opt-in sidecar**, disabled by default (`ChainConfig::enabled`
defaults to `false`; the log lives at `<data_dir>/chain`). When enabled, the
capture hook sits in all three write-commit paths — `write`,
`write_with_timestamp`, `write_with_options` (`db/transaction.rs`) — and there
is **no public commit path that bypasses it** (`WriteTransaction` exposes no
`pub fn commit`; `commit_with_timestamp` is crate-internal). The capture covers
`CreateNode` / `UpdateNode` / `CreateEdge` / `UpdateEdge` directly, and
`DeleteNode` / `RetractNode` / `DeleteEdge` / `RetractEdge` via their closing
versions resolved post-commit (`db/chain.rs`: `precapture_chain`,
`finalize_chain_capture`).

Egregore's adapter writes exclusively through these paths: `AletheiaDB::create_node`
and `create_edge` both route through `self.write(|tx| …)` (`db/ops.rs`), and the
adapter's own updates use `self.db.write(|tx| tx.update_node(…))`
(`src/adapters/aletheiadb.rs`). So **when enabled, 100% of adapter writes are
chained** — including the tombstone writes below.

Two caveats, both honest:

1. **Egregore does not enable the chain today.** `open_inner`
   (`src/adapters/aletheiadb.rs`) builds the config from
   `durable_config_for_data_dir` and never sets `.chain(…)`. Enabling is a
   deliberate one-way operator decision (follow-up implementation work).
2. **The chain attests from its genesis, not from the store's birth.**
   Enabling on an *existing* store backfills: `rebuild_chain_tail` reseals every
   historical transaction from replayed history beyond the (genesis) head. But
   seal and verify read the *same* `VersionSource`, so a backfilled chain proves
   "history as currently stored is internally consistent and nothing changed
   since enablement" — it cannot detect alterations made *before* the chain
   existed. The manifest claim must therefore be scoped as "unaltered since
   <chain-genesis timestamp>", never "never altered".

### Q2: What does `verify_chain` cost? Audit lane or read path?

`verify_full` is **O(history)**: it recomputes every version's leaf from stored
history (full property reconstruction per version, stable across anchor/delta
re-encoding via `DbVersionSource::normalize_immutable`) and re-folds the chain
from genesis, holding the historical read lock once for the whole pass
(`VersionSource::scoped`). It is unambiguously an **`eg audit` lane**, not
something on any read or pack-assembly path.

Cheaper surfaces exist for the manifest/verify flow:

- `verify_entity_chain(kind, id)` — O(entity versions); proven scan-free by
  upstream's fetch-counting test.
- `verify_chain_against(anchor)` — O(transactions), **no version re-fetch**;
  re-folds the sealed leaves from genesis and checks the current head extends
  the exported anchor. Detects rollback (truncation) and fork (divergence).
- `export_chain_head()` — O(1) head export for anchoring.

The credible manifest design this enables: pack assembly records the chain
genesis digest, the head digest at assemble time, and optionally a full-verify
pass result (pass/fail, `head_seq`, timestamp). `verify_pack` later re-exports
the head and runs `verify_chain_against` for cheap freshness — without an
O(history) pass per verification.

### Q3: Does a legitimate retraction read as tampering? (Answered: no.)

This is the load-bearing question, and the answer is **no — legitimate
retractions verify cleanly, by construction and by regression test.**

Mechanism: the leaf binds a version's *immutable* logical content —
identity, label, endpoints, reconstructed properties, creation coordinates
(`valid_from`/`transaction_from`), the tombstone flag, and the born-closed
terminal `valid_to` (`provenance_chain/canonical.rs`, `db/chain_source.rs`).
It deliberately does **not** bind facets mutated by later writes — open
`valid_to`/`tx_to`, `is_current` — which are normalized out, so supersession
closes cannot break the chain. A retraction is simply a new transaction writing
a tombstone/retraction version; it is captured and sealed like any other
transaction, and verification recomputes the identical leaf from the same
stored bytes.

Upstream proves the dual with in-crate regression tests (`db/chain.rs`,
`tamper_tests`): a legit `retract_node` / `delete_node_with_valid_time` seals
as an ordinary transaction and verifies; only *tampering with* a retraction —
extending its `valid_to`, re-opening a tombstone interval — is flagged and
localized to the earliest broken sequence.

Egregore's retraction shape is even friendlier than the bi-temporal-close case
those tests cover: `eg forget` (#231) and `eg forget-repo` (#248) **never touch
stored versions**. They write *new* tombstone nodes plus retraction/eviction
event nodes through ordinary `db.write` / `create_node` transactions
(`src/adapters/aletheiadb.rs::write_tombstone`), leaving every target byte in
place for `--at` history. Redaction operates on pre-write `GraphRecord`s, not
on stored history. The per-entity timeline-consistency check
(non-decreasing transaction starts, well-formed intervals) is compatible with
tombstone-node appends. A chain that flagged our own retractions would require
in-place history mutation — which Egregore never does.

### Q4: Does the chain survive `eg forget`'s tombstone writes and the eviction path? What about erasure stability? (Answered: yes.)

- **Tombstone writes**: ordinary committed transactions → captured, sealed,
  verified like everything else. Survive trivially.
- **`eg forget-repo` (#248)**: explicitly *logical* eviction — tombstone nodes
  plus one auditable eviction event; the evicted records "disappear from the
  catalog surface", not from storage. There is **no physical version deletion
  anywhere in Egregore's adapter** (no `delete_node` / `retract_node` /
  `.historical` access in `src/adapters/aletheiadb.rs`; `repo_evict.rs` is pure
  tombstone-node logic). The chain therefore stays verifiable across both
  forget paths.
- **Erasure stability**: upstream's crypto-shred AC4 tests (`db/chain.rs`,
  `crypto_shred_ac4_tests`) prove the chain stays verifiable after
  `erase_subject` because the leaf binds the stored ciphertext envelope while
  erasure destroys only the key. Egregore does not use designation/crypto-shred
  (no `designate_subject` / `erase_subject` / `EncryptionConfig` usage in
  `src/`), so this is a principle rather than a dependency: erasure that
  preserves bytes preserves verifiability; Egregore's logical retraction always
  preserves bytes.

The sharp edge to record: `verify_full` **fails on a missing version**
("version … not found in source" → broken). Any *future* physical-deletion
feature (true hard delete, retention compaction) would break verification —
which is the correct behavior, since physical deletion *is* an alteration, but
it must then be a designed, disclosed operation: verify before, export anchor
after, record both in the manifest.

## Decision

**Yes.** `verify_chain` can back a credible "this evidence store has not been
altered" claim in the #338 evidence-pack manifest, under these conditions:

1. **Enablement is a separate, deliberate step.** The chain is off today; a
   follow-up issue should enable it in the adapter (`open_inner`), record the
   chain genesis digest + timestamp in store metadata at enable time, and treat
   enabling an existing store as a documented one-way operator action (the
   backfill binds surviving history only).
2. **Scope the claim honestly.** The manifest claim is "no alteration of the
   store's committed history since <chain-genesis timestamp>" — covering the
   whole store (all transactions), not just the pack's window. Never "never
   altered".
3. **Full verify is an audit lane.** `eg audit` (O(history)); pack assembly
   records genesis + head digests and optionally a full-pass result;
   `verify_pack` uses cheap `verify_chain_against` for freshness.
4. **Our retractions are safe.** Q3/Q4 answered above: legitimate retractions
   and logical evictions verify cleanly; only in-place history mutation —
   which Egregore never performs — would read as tampering.

## Explicitly not revisited

Nothing in this evaluation overturns the issue's scoping decision:

- **Derivation lineage (#3371)** stays out. `eg query evidence-path` (#247) is
  an undirected shortest witness path over an explicitly partitioned
  evidence/provenance edge subgraph; upstream lineage is version-pinned
  fact-to-fact closures. Different constructs; adopting both means two parallel
  models.
- **Trust propagation (#3382)** stays out. The trust-class system is an
  epistemic category with hard rules ("an agent-authored claim is never
  evidence for itself"), not a numeric `computed_confidence` tree. Replacing
  it would lose semantics a score cannot express.
- **Belief-revision audit (#3362)** stays out. Supersession +
  `--supersession exclude|include-but-flag` and the agent-memory contradiction
  handling are load-bearing and bi-temporally honest; a second revision model
  would duplicate them.

All three exist in 0.2.0 (CHANGELOG: "Bi-temporal, provenance & lineage"
section) — this is a deliberate non-adoption, not an oversight.

## Follow-up work (not this ADR)

- Implementation issue: enable the provenance chain in the embedded adapter,
  persist chain genesis metadata, add the `eg audit` verify lane.
- Implementation issue: extend `PackManifest` (#338) with the chain anchor
  fields (genesis digest, head digest at assemble, optional full-verify
  result) and teach `verify_pack` the `verify_chain_against` freshness check.
- Document the physical-deletion rule: any future true-delete feature must
  verify-before / export-anchor-after, or it will (correctly) break the chain.
