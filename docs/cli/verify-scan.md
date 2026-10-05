# `eg verify-scan` — Verify Byte-for-Byte Scan Reproducibility

**Issue:** #239 — _verify scan reproducibility: byte-for-byte stable re-scan as
a runnable check_

---

## Overview

`eg verify-scan <repo>` re-scans a repository **twice within one invocation**
and reports whether the two outputs are byte-for-byte stable:

```text
eg verify-scan . --repo-id-override my-repo
eg verify-scan . --format text
```

A naive "run `eg scan` twice and `diff`" is not a reproducibility check: the
transaction/valid time comes from the wall clock and the repository identity
can vary by checkout directory, so two honest scans of the same tree differ by
default and the manual diff produces false mismatches. `verify-scan` pins both
producer-relative inputs for the two scans — one transaction instant captured
once and shared, and one repository identity — so the only thing under test is
the scan's own determinism. A silent nondeterminism regression (map ordering,
path normalization, clock leakage) shows up here instead of corrupting
incremental caches, drifting scores, or the deterministic-fact/observation
boundary without a signal.

## The determinism contract

**Determinism is guaranteed only relative to a fixed producer plus pinned
time.** Concretely, `verify-scan` promises: _this_ `egregore` binary, scanning
an unchanged tree with the pinned transaction time and repository identity,
emits byte-identical JSONL. It does **not** promise that a different
`egregore` version extracts identical records for the same source — a grammar
or extractor bump can change spans and edges without any source change. That
cross-version axis is the complementary check: `eg query producer-drift`
(issue #234) audits stored records against the running binary's producer
identity. Run `verify-scan` for "is my scan self-consistent" and
`producer-drift` for "did my extractor change under me".

## Synopsis

```text
eg verify-scan <repo_path> [--repo-id-override <id>] [--format json|text]
```

| Flag | Effect |
|------|--------|
| `--repo-id-override <id>` | Force `identity_source = operator_override`. Recommended when comparing checkouts in different directories (e.g. CRLF vs LF checkouts of the same commit), where path-derived identity would otherwise differ. Without it, the auto-detected identity is used — stable across checkouts for git repositories with a remote or a root commit, path-derived otherwise. |
| `--format json\|text` | Report format. JSON (default) is machine-readable; `text` is the human-readable form. |

The command runs the same pipeline as `eg scan` (including standard
redaction), writes nothing into the repository, and prints the report to
**stdout** with empty stderr.

### Exit codes

| Condition | Exit | Output |
|-----------|------|--------|
| Both pinned scans byte-identical | `0` | Report with `stable: true`, `ok: true`, and the record/node/edge counts |
| Scans differ | `1` | Report with `stable: false`, `ok: false`, the differing record handle(s), the first differing field per record, and the first differing JSONL line/byte offset |
| Scan failure (unreadable repo, …) | `1` | Error on stderr, no stability verdict |

## Report shape

JSON (default):

```json
{
  "ok": true,
  "stable": true,
  "repo_path": ".",
  "repository_id": "codegraph:v1:…",
  "identity_source": "remote",
  "transaction_time": "2026-09-17T23:50:01Z",
  "disclaimer": "Determinism is guaranteed only relative to a fixed producer …",
  "first": { "records": 123, "nodes": 100, "edges": 23 },
  "second": { "records": 123, "nodes": 100, "edges": 23 },
  "jsonl_hash": "blake3 hex of the first scan's JSONL",
  "difference_count": 0,
  "differences_truncated": false,
  "differences": []
}
```

`first_differing_line` (1-based JSONL line number) and `first_differing_byte`
(byte offset) are present only when the scans differ. They always name where
the two outputs diverged — including the byte-only case where the outputs
differ with no record-level differences at all (e.g. record reordering or
formatting), in which case `differences` is empty but the divergence location
is still reported.

When the scans differ, `differences` lists up to 25 entries
(`difference_count` is uncapped; `differences_truncated` says whether the
list was capped), in sorted handle order:

```json
{
  "handle": "codegraph:v6:9f…",
  "kind": "field_mismatch",
  "line": 17,
  "field": "span",
  "first_value": "{\"start_line\":12,…}",
  "second_value": "{\"start_line\":13,…}"
}
```

`kind` is one of `field_mismatch` (the handle exists in both scans; `field`
is the first differing field in sorted field order, with truncated
first/second values), `missing_in_second`, or `missing_in_first`. `line` is
the 1-based JSONL line number where the offending record occurrence appears —
in the first scan when the record is present there, otherwise in the second
scan. A handle that occurs more than once in a scan is qualified as
`<handle>#2`, `<handle>#3`, … so each entry still names one record.

Text form:

```text
verify-scan .: stable: true
  scans compared: 2, transaction_time: 2026-09-17T23:50:01Z, identity: remote (codegraph:v1:…)
  first scan:  123 records (100 nodes, 23 edges)
  second scan: 123 records (100 nodes, 23 edges)
  differences: 0
  Determinism is guaranteed only relative to a fixed producer …
```

An unstable text report prints `first differing line: <n> (byte offset <m>)`
after the difference count, then lists each difference as `~ <handle> (line
<n>): field '<f>' differs` (with `first:`/`second:` previews),
`- <handle> (line <n>): present in first scan, missing in second`, or
`+ <handle> (line <n>): missing in first scan, present in second`. When the
outputs diverge with no record-level differences, it says so explicitly
instead of leaving the divergence unexplained.

## CRLF/LF composition (issue #242)

Line-ending normalization (#242) makes the extractor itself line-ending
agnostic; `verify-scan` composes with it. A CRLF checkout and an LF checkout
of the same commit produce byte-identical scan output **once repository
identity is pinned** (pass the same `--repo-id-override` for both
checkouts — path-derived identity legitimately differs between two
different directories). The `jsonl_hash` field lets two checkouts compare
hashes without exchanging full output.

## Notes

- `verify-scan` is a self-consistency check, not a golden-baseline
  regression: it compares the two scans against each other, never against a
  committed snapshot.
- History-replay determinism beyond a single HEAD scan is out of scope for
  this slice.
- The command contributes zero nondeterminism itself: the transaction
  instant is captured once per invocation and the identity is fixed, so the
  false-positive rate on a deterministic repository is 0.
