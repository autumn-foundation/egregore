# eg audit memory-health

Audit the store-wide composition and provenance health of the `agent_memory` domain — answer the maintainer question *"is agent memory growing reviewable, or quietly rotting into sludge?"* — over a graph JSONL file or an embedded `AletheiaDB` store (issue #94).

Unlike `eg inspect` which counts totals, `eg audit memory-health` measures metrics like provenance coverage, verification ratios, supersession/contradiction rates, and link rot (dangling evidence links). It introduces **no** network access, hosted indexing, remote crawling, or mandatory remote embeddings.

## Synopsis

```text
eg audit memory-health [--graph <PATH> | --data-dir <PATH>]
                       [--min-provenance-coverage <F>]
                       [--max-dangling-evidence <F>]
                       [--max-unverified <F>]
                       [--max-current-guidance-contamination <F>]
                       [--format json]
```

* `--graph <PATH>` — Graph JSONL path (mutually exclusive with `--data-dir`).
* `--data-dir <PATH>` — Embedded `AletheiaDB` store directory (mutually exclusive with `--graph`).
* `--min-provenance-coverage <F>` — Minimum fraction of observation records that must have provenance coverage. Default: `1.0`.
* `--max-dangling-evidence <F>` — Maximum fraction of observation records that can have dangling evidence. Default: `0.0`.
* `--max-unverified <F>` — Optional maximum fraction of observation records that can be unverified.
* `--max-current-guidance-contamination <F>` — Optional maximum fraction of active observation records that can be contaminated.
* `--format json` — output format (JSON only; `text` aliases to JSON).

## Shortest local workflow

```sh
eg audit memory-health --graph graph.jsonl
echo "exit: $?"   # 0 = healthy, 1 = sludge line crossed, 2 = usage/load error
```

Exit codes:
* `0` — gate passed (`ok: true`).
* `1` — gate failed (`ok: false`; the full JSON report is still printed to stdout).
* `2` — usage/load error (bad path, unparseable graph, or invalid parameters).

## Computed Ratios

Every ratio in the report is returned as a JSON object containing `numerator`, `denominator`, and the computed `ratio` (or `0.0` if the denominator is `0`):

| Ratio | Description | Numerator | Denominator |
|-------|-------------|-----------|-------------|
| `provenance_coverage` | Share of observations carrying a full trail. | Carrying `agent_id` or `session_id`, `source_handle`, and `observed_at`. | Total Observations |
| `unverified` | Share not verified by independent tests. | Observation NOT linked to supporting verification/command evidence. | Total Observations |
| `superseded` | Share superseded by a newer record. | Observation marked as superseded by `superseded_by` or `SUPERSEDES` edge. | Total Observations |
| `contradicted` | Share contradicted by another record. | Observation involved in a `CONTRADICTS` relation. | Total Observations |
| `dangling_evidence` | Share carrying broken/orphaned links. | Observation having at least one evidence link or edge pointing to an absent/tombstoned node. | Total Observations |
| `current_guidance_contamination` | Share of active (non-tombstoned) records that are stale/contradicted/unverified/orphaned. | Active observations that are superseded, contradicted, unverified, or missing provenance. | Active Observations |
| `missing_provenance` | Share lacking origin metadata. | Lacking provenance coverage. | Total Observations |
| `weak_provenance` | Share with a source handle but broken target. | Carrying a source handle but has dangling evidence. | Total Observations |
| `unratified_memory` | Share unverified but fully citable. | Carrying provenance but unverified. | Total Observations |
| `stale_or_contradicted_memory` | Share historically valid but no longer current. | Superseded or contradicted. | Total Observations |

## Stable Diagnostics

When a threshold is breached, the report adds a diagnostic object to `"diagnostics"` with a stable, machine-readable code:

* `no_memory` — Seeded store contains no active observation records.
* `provenance_coverage_below_threshold` — Provenance coverage fell below `--min-provenance-coverage`.
* `dangling_evidence_above_threshold` — Dangling evidence ratio exceeded `--max-dangling-evidence`.
* `unverified_above_threshold` — Unverified ratio exceeded `--max-unverified`.
* `current_guidance_contamination_above_threshold` — Guidance contamination exceeded `--max-current-guidance-contamination`.

## Differences from Other Tools

* **`eg inspect`** — A general store-wide census. Reports total record counts per domain/kind/version. Answers "how much exists," not "is it clean."
* **`eg query memory` / `eg audit`** — Audits a single memory record's evidence trail (supporting, contradicting, etc.).
* **Recall-time supersession flagging** — Perperformed during `query semantic-memory` to filter or flag returned records in real-time.
* **`eg audit memory-health`** — The global composition health view. Evaluates whether the whole memory store is maintaining high trust or decaying into sludge. A low health score is a reason to **review** or remediate memory, not proof that any single record is wrong.
