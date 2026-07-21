# Corpus scope for query lanes (`corpus_mode`)

Query lanes read from a store that may hold a single scanned snapshot (`eg scan`)
or the full commit history of a repository (`eg scan-history`). Over a history
store there are two honest answers to "what does the graph contain right now?":
the state at the repository's current HEAD, or the *union* of every commit
snapshot. Issue #427 unified how lanes choose between them and made every lane
**disclose** the choice in its output envelope.

This page documents the contract once. Individual lane pages link here and note
only their own default.

## Envelope disclosure fields

Every Category-A (current-state code) and Category-B (history-analysis) lane
emits three fields in its summary envelope:

| Field | Type | Meaning |
| --- | --- | --- |
| `corpus_mode` | string | Which corpus the answer was computed over — one of the four values below. |
| `corpus_mode_source` | string | How that mode was chosen — `default`, `explicit_flag`, or `selector`. |
| `corpus_disclaimer` | string | A one-line human description of the corpus, safe to surface verbatim. |

### `corpus_mode` values

| Value | Meaning |
| --- | --- |
| `head_anchored` | Records current at each repository's stamped HEAD commit (`source_snapshot`, issue #82). An edge or target removed at HEAD is **excluded**. This is the default for current-state code lanes over a snapshot-bearing history store. |
| `union` | The union of all commit snapshots — no head anchoring. An edge or target removed at a later commit **still appears**. This is the by-design default for history-analysis lanes, and the opt-in (`--all-history`) view for the flipped code lanes. |
| `commit_pinned` | A single commit's snapshot, selected by `--at <sha>` or `--as-of <instant>`. |
| `single_snapshot` | A snapshot-less store (a plain `eg scan`, a non-git filesystem-fallback scan, or a hand-built JSONL fixture with no `Repository.source_snapshot`): a keep-last-per-id current state, where head and union are the same set. |

### `corpus_mode_source` values

| Value | Meaning |
| --- | --- |
| `default` | No corpus flag or temporal selector supplied — the lane's default corpus for the store. |
| `explicit_flag` | An explicit `--at-head` or `--all-history` flag chose the mode. |
| `selector` | An `--at`/`--as-of` temporal selector chose the mode (always `commit_pinned`). |

> Note: a plain `eg scan` over a **git** working tree stamps a `source_snapshot`,
> so its store is snapshot-bearing. Lanes that read the union over a
> single-commit git scan therefore disclose `corpus_mode: "union"` (union of one
> commit == that one commit), not `single_snapshot`. `single_snapshot` appears
> only for a truly snapshot-less store.

## The `--at-head` / `--all-history` flag pair

Flipped current-state code lanes accept two plain boolean flags:

- `--all-history` — read the **union** of all commit snapshots (the pre-#427
  default). Discloses `corpus_mode: "union"`, `corpus_mode_source: "explicit_flag"`.
- `--at-head` — force the **HEAD-anchored** view explicitly. Same result as the
  default over a snapshot-bearing store, but `corpus_mode_source: "explicit_flag"`.

Mutual exclusion (all violations exit `1` with a machine-readable
`unsupported_combination` envelope on stdout):

- `--at-head` cannot be combined with `--all-history`.
- Neither `--at-head` nor `--all-history` can be combined with `--at`/`--as-of`
  (a temporal pin already selects a single-commit corpus).

## Per-category defaults

- **Current-state code lanes** default to `head_anchored` over a snapshot-bearing
  history store (and `single_snapshot` over a snapshot-less store). They answer
  "what does the code look like now?", so a symbol/edge removed before HEAD should
  not appear by default.
- **History-analysis lanes** are `union` **by design** — churn, coupling,
  lifelines, deltas, ownership, and recency are questions *about* history and must
  see every commit. They carry the disclosure fields for transparency but expose
  no `--at-head`/`--all-history` flags.

## Which lanes are in which category

| Category | Lanes | Corpus behavior |
| --- | --- | --- |
| **Flipped + flagged** | `deps`, `transitive-callers`, `transitive-callees`, `path`, `who-imports`, `change-impact`, `cycles`, `evidence-path`, `context`, `subsystem`, `symbol`, `failures`, `unsafe-sites`, `unwrap-expect`, `debt-markers` | New default `head_anchored`; `--all-history` opts into `union`; `--at-head` forces head. |
| **Head-anchored (already)** | `public-api`, `unreferenced`, `at`, `locate`, `file`, `who` (who-changed), `undocumented`, `verification-coverage` | HEAD-anchor by default; disclose `head_anchored` (or `single_snapshot`); `--at`/`--as-of` pin a commit. |
| **History-analysis (union by design)** | `churn`, `coupling`, `lifeline`, `deltas`, `public-api-deltas`, `ownership`, `recency`, `changes` | Union by design; disclose `union`. No corpus flags. |

With issue #456 the last latent Category-A lanes (`change-impact`, `cycles`,
`evidence-path`, `context`, `subsystem`, `symbol`, `failures`, `unsafe-sites`,
`unwrap-expect`, `debt-markers`) are flipped to the `head_anchored` default and
gain the `--at-head`/`--all-history` flag pair under this published contract.
`symbol` discloses the corpus per NDJSON row rather than on an envelope; the
inventory lanes that already offered `--at` keep it as `commit_pinned` and reject
`--at` combined with either corpus flag.
