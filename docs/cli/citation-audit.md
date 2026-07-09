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
eg audit citations --graph <PATH>    [--min-code-citation <F>] [--format json]
eg audit citations --data-dir <DIR>  [--min-code-citation <F>] [--format json]
```

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
  "gate": {
    "code_citation_completeness": 1.0,
    "code_gate_pass": true,
    "non_code_handle_gate_pass": true,
    "unclassified_missing_rows": 0
  }
}
```

| Field | Meaning |
|-------|---------|
| `code_gate_pass` | **Fails** when fewer than `min_code_citation` (default 95%) of code-answer rows carry a stable record ID plus a repo-relative file/span handle or a documented absent-span reason (AC4). |
| `non_code_handle_gate_pass` | **Fails** when any agent-memory, project, artifact, verification, redaction, protected-artifact, or user-context row lacks at least one source / verification / task / policy-audit / protected-payload handle (AC5). |
| `unclassified_missing_rows` | Missing-handle rows that lack a classifying diagnostic. The success metric requires this to be `0`. |
| `ok` | `true` only when both gates pass and `unclassified_missing_rows == 0`. |

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
`artifact` / `user_context` vocabulary), a `status`
(`cited` / `absent_handle_documented` / `missing_required_handle` /
`excluded_unverified` / `excluded_protected`), and a `primary_handle` when
present. Deterministic code facts are kept separate from agent-authored memory,
project intent, artifacts, verification evidence, and user-context policy; an
agent-authored claim is never counted as evidence for itself (AC6).

### Covered workflows

`symbol`, `file`, `drift`, `semantic`, and `manifest-deps` (code-oriented) plus
the cross-domain lanes `context`, `subsystem`, `task`, `memory`, `failures`,
`change-impact`, `policy`, `candidates`, `changes`, and `evidence-freshness`.
The `manifest-deps` lane gates every returned `DependencyDeclaration` row on
its stable record ID plus the repo-relative `Cargo.toml` handle (a spanless
path-cited source fact). A workflow with
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
project, verification, user-context, daemon, and schema-version contracts. It
introduces no new graph domain, node kind, edge label, trust class,
schema-version rule, redaction taxonomy, protected-artifact retrieval behavior,
hosted service, LLM-generated answer, language expansion, or project-management
UI.
