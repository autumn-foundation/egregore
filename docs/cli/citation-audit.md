# eg audit citations

Audit **citation completeness** across Egregore's public query workflows — answer
the maintainer question *"do our answers still cite graph records and
repo-relative file/span handles, or have they regressed into uncited local
prose?"* — over a seeded local record set. Local-first; no network access,
hosted indexing, remote repository crawling, or mandatory remote embeddings.

> **A passing gate measures citation coverage, not truth.** The audit proves
> that returned rows carry the handles their trust class requires; it never
> decides whether an answer is correct. Absence of a missing-handle row is not
> evidence an answer is right.

This is a **measurement gate** layered over existing query, evidence-link,
redaction, protected-artifact, project, verification, and user-context
contracts. It introduces no new graph domain, trust model, edge vocabulary,
hosted service, or LLM-generated answer (issue #65).

## Synopsis

```text
eg audit citations --graph <PATH>    [--min-code-citation <F>] [--min-log-citation <F>] [--format json]
eg audit citations --data-dir <DIR>  [--min-code-citation <F>] [--min-log-citation <F>] [--format json]
```

`--min-log-citation` gates the `runtime_observation` (log-domain) lane and
defaults to `1.0` — the strictest gate, because a runtime observation is the
least-trusted trust class. Both thresholds are validated to `[0.0, 1.0]`; an
out-of-range value is a usage error (exit 2, `invalid_min_code_citation` /
`invalid_min_log_citation`).

Reads a seeded record set from a JSONL graph (`--graph`) or an embedded
`AletheiaDB` store (`--data-dir`). The two sources are mutually exclusive.

## Shortest local workflow

```sh
# Scan a repo and any cross-domain records into one JSONL graph, then audit it:
eg scan . --out graph.jsonl
eg audit citations --graph graph.jsonl
echo "exit: $?"   # 0 = gate passed, 1 = gate failed, 2 = usage/load error
```

The `semantic` workflow needs a vector index, so over a `--graph` fixture it is
reported `enabled: false` with `disabled_reason: "requires_embedded_store"`. To
include it, point the audit at an embedded store built with embeddings:

```sh
eg ingest graph.jsonl --adapter embedded --data-dir .egregore --embed
eg audit citations --data-dir .egregore
```

## How to interpret pass/fail

The report ends with a `gate` block and a top-level `ok`:

```json
{
  "ok": true,
  "min_code_citation": 0.95,
  "min_log_citation": 1.0,
  "gate": {
    "code_citation_completeness": 1.0,
    "code_gate_pass": true,
    "non_code_handle_gate_pass": true,
    "log_citation_completeness": 1.0,
    "log_gate_pass": true,
    "unclassified_missing_rows": 0
  }
}
```

| Field | Meaning |
|-------|---------|
| `code_gate_pass` | **Fails** when fewer than `min_code_citation` (default 95%) of code-answer rows carry a stable record ID plus a repo-relative file/span handle or a documented absent-span reason (AC4). |
| `non_code_handle_gate_pass` | **Fails** when any agent-memory, project, artifact, verification, redaction, protected-artifact, or user-context row lacks at least one source / verification / task / policy-audit / protected-payload handle (AC5). |
| `log_gate_pass` | **Fails** when fewer than `min_log_citation` (default 100%) of `runtime_observation` (log-domain) rows carry their required citation — a well-formed `log:v1:` record ID plus `LogSource` provenance (issue #328). A below-threshold lane emits a `below_log_citation_threshold` diagnostic naming the specific failing workflow, the `runtime_observation` class, and the measured rate (issues #328, #376). |
| `unclassified_missing_rows` | Missing-handle rows that lack a classifying diagnostic. The success metric requires this to be `0`. |
| `ok` | `true` only when the code, non-code, and log gates all pass and `unclassified_missing_rows == 0`. |

Exit codes: `0` gate passed, `1` gate failed (the full JSON report is still
printed to stdout so it is consumable), `2` usage/load error (bad path,
unparseable graph).

## Report shape

Every workflow reports the same six tallies, per workflow and overall (AC3):

| Count | Meaning |
|-------|---------|
| `total_rows` | Rows the workflow returned. |
| `rows_with_record_id` | Rows carrying a stable record ID. |
| `rows_with_primary_handle` | Rows carrying the primary citable handle their trust class requires. |
| `rows_using_absent_handle_rule` | Rows that legitimately have no span and use a documented absent-handle rule. |
| `rows_missing_required_handle` | Rows missing a required handle (these fail the gate). |
| `rows_excluded_unverified_or_protected` | Rows excluded because they are unverified or carry a protected payload — reported, never hidden. |

Each row carries a `record_id`, a `trust_class` (reusing the existing
`source_fact` / `agent_authored` / `verification_evidence` / `project_state` /
`artifact` / `user_context` / `runtime_observation` vocabulary), a `status`
(`cited` / `absent_handle_documented` / `missing_required_handle` /
`excluded_unverified` / `excluded_protected`), and a `primary_handle` when
present. Deterministic code facts are kept separate from agent-authored memory,
project intent, artifacts, verification evidence, user-context policy, and
runtime log observations. The standing invariant: **an agent-authored claim is
never counted as evidence for itself, and a runtime observation is never counted
as verification** (AC6; issue #328).

### The `runtime_observation` citation requirement (issue #328)

Runtime log observations (`LogSource`, `ErrorSignature`, `LogEvent`,
`LogOccurrenceBucket`) are the least-trusted trust class — a program's own claim
about its execution, deterministically parsed but never verified. Every returned
`runtime_observation` row must carry:

- a stable, well-formed `log:v1:` record ID, **and**
- its `LogSource` provenance: the source path **and** the `source_artifact_hash`.
  A `LogSource` is cited from its own payload. An `ErrorSignature` / `LogEvent` /
  `LogOccurrenceBucket` gets provenance by resolving its `CAPTURED_FROM` /
  `AGGREGATES` edge(s) to an **at-least-one** present `LogSource` carrying a
  hash — log node IDs exclude the source, so a signature may carry several
  `CAPTURED_FROM` edges to distinct sources. A row with no resolvable source is a
  `missing_required_handle` failure, never credited by its own ID.

The template hash is **not** a standalone field: the `ErrorSignature` template is
hashed into the content-addressed `log:v1:` record ID (identity = repository,
algorithm, normalized template, severity), so a well-formed `log:v1:` ID *is* the
citation of the template-hash requirement. This is a disclosed schema shape (part
of the #361–#364 log-graph known-limitation cluster), not a new field.

A signature's resolved backtrace-frame targets (`FRAME_RESOLVES_TO`, #322) and
its overlapping symbol deltas are **code rows**, audited under the existing
code-handle rule (record ID + repo-relative file/span, or a documented
absent-span reason); an `unresolved`-targeting frame passes via its `Diagnostic`
handle, and a dangling target is never counted as cited.

### Covered workflows

`symbol`, `file`, `drift`, `semantic`, and `manifest-deps` (code-oriented) plus
the cross-domain lanes `context`, `subsystem`, `task`, `memory`, `failures`,
`change-impact`, `policy`, `candidates`, `changes`, `evidence-freshness`,
`log-deltas`, `error-context`, and `log_signatures`. The `manifest-deps` lane
gates every returned `DependencyDeclaration` row on its stable record ID plus the
repo-relative `Cargo.toml` handle (a spanless path-cited source fact). The audit
drives **three** log query workflows, each returning `runtime_observation` rows
gated by `--min-log-citation` (their resolved-frame and overlapping-symbol-delta
rows are gated as code rows):

- `log-deltas` (`eg query log-deltas`, #326) — runtime error-signature deltas
  across a commit range.
- `error-context` (`eg query error-context`, #324) — one signature's full
  cross-domain context bundle, driven once per `ErrorSignature` in the set.
- `log_signatures` (the subsystem `log_signatures` section, `eg query
  subsystem`, #325) — signatures whose frames resolve under a subsystem prefix.

Because the `runtime_observation` citation requirement is class-wide, ANY lane
that surfaces a log row (e.g. `memory` reaching an `ErrorSignature` as supporting
evidence) applies the same rule, and a below-threshold lane names ITSELF in the
`below_log_citation_threshold` diagnostic (issue #376) — never a hard-coded
`log-deltas`. A workflow with
nothing to return in the fixture reports zero rows rather than disappearing; one
that needs inputs the fixture lacks (e.g. `semantic` without an embedded vector
index, or `changes` without a commit range) is reported `enabled: false` with a
stable `disabled_reason`, never silently dropped.

Ambiguous code handles in a multi-repository store are not skipped: the audit
records an `ambiguous_code_handle` diagnostic and drives each candidate record ID
so its rows are still measured. Scan-history graphs that carry multiple temporal
versions under one stable record ID keep each version as a distinct row, so a
later uncited version cannot hide behind an earlier cited one.

### Diagnostic codes (AC7)

| Code | Meaning |
|------|---------|
| `missing_span` | A code row carries neither a file/span handle nor a documented absent-span reason. |
| `missing_required_handle` | A non-code row lacks the handle its trust class requires. |
| `unresolved_evidence_link` | An evidence-link target is absent from the record set. |
| `stale_evidence_target` | An evidence target exists only as a tombstone. |
| `redacted_field` | A row carries a redaction marker or policy version. |
| `protected_payload` | A row references a protected raw payload, withheld by hash/handle only. |
| `unsupported_workflow` | A workflow could not run (e.g. `semantic` without an embedded store). |
| `below_log_citation_threshold` | A `runtime_observation` (log-domain) lane fell below `min_log_citation`; names the **specific** failing workflow (`log-deltas`, `error-context`, `log_signatures`, or any other lane that surfaced a log row), the `runtime_observation` class, and the measured rate (issues #328, #376). |
| `missing_record_id` | A row (code or log) carries no well-formed stable record ID. |

### Safety: no raw payloads (AC8)

Output never includes raw transcript text, command output, patch hunks, issue
bodies, PR comments, environment values, bearer tokens, or protected
raw-artifact payloads. Only record IDs, handles, hashes, redaction markers,
bounded labels, and counts appear. Protected payloads are referenced by
`protected:v1:<hash>` handle only.

### Scope: the default invocation of each workflow

The gate measures each public query workflow at its **default invocation** — the
current store view, no repository scope, and default depth — which is the
behavior an operator gets by running the command with only its required
arguments. Flag-driven variants are deliberately outside the default gate
because they form an unbounded surface and are operator-initiated re-runs, not
the default answer:

- temporal/transaction-time views (`--at`, `--as-of`, `--tx-as-of`),
- repository scoping (`--repo`) in a shared multi-repo store, and
- wider neighborhoods (`eg query change-impact --depth N`, `N > 1`).

The one exception is `eg query evidence-freshness`, whose *default* invocation is
itself history-inclusive; the audit mirrors that by reading the history-inclusive
store view for that lane over `--data-dir`. To gate a specific flagged view, run
the underlying query directly and inspect its handles.

## When to use this versus other tools

| Reach for | When you want |
|-----------|---------------|
| **`eg audit citations`** (this) | Prove, across **all** public query outputs, that rows remain evidence-citable above a threshold, separated by trust class. |
| `rg` / `git grep` | Fast recursive **text** search when you know where to look. It sets the accountability bar but understands no trust classes or evidence links. |
| `jq` | Cheaply count or filter fields in **JSONL** you have already located. It cannot separate deterministic facts from agent memory. |
| [GitHub Code Search](https://docs.github.com/en/search-github/github-code-search/understanding-github-code-search-syntax) | Hosted path/symbol search. It does not audit whether local memory, task, artifact, redaction, and verification results carry evidence handles. |
| [Sourcegraph Cody](https://sourcegraph.com/docs/cody/core-concepts/context) | Polished codebase-aware retrieval. Egregore's stricter job is to *prove* every local result is citable, not just that relevant context was retrieved. |
| raw transcript review | Replaying an agent run turn-by-turn. |

`rg` + `jq` over transcripts and JSONL are honest substitutes for the underlying
search, but they do not measure citation coverage across trust classes or fail a
gate when answers regress into uncited prose.

## Scope

This slice consumes existing query, evidence-link, redaction, protected-artifact,
project, verification, user-context, log-domain, daemon, and schema-version
contracts. It introduces no new graph domain, node kind, edge label, trust class,
schema-version rule, redaction taxonomy, protected-artifact retrieval behavior,
hosted service, LLM-generated answer, language expansion, or project-management
UI. The `runtime_observation` trust class already exists in the log-graph schema
(issues #319/#320); issue #328 adds its citation **requirement** and gate, not
the class itself.
