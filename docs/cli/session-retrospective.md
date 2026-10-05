# `eg query session` — one agent session's footprint, claims, and verification

Pivots the graph on a single `AgentSession` and returns a trust-separated
retrospective of that run: what it touched, what it claimed, what it produced,
what failed, and whether the work was verified. Built for the handoff moment —
an operator (or the next agent) asking "what did session S actually do, and
was it verified?" without re-reading the raw transcript.

```powershell
eg query session <ID-OR-HANDLE> --graph <PATH> [--format json|text]
eg query session <ID-OR-HANDLE> --data-dir <PATH>
```

The session resolves by canonical `AgentSession` record ID or by imported
session handle (the node's `source_handle`, i.e. `<artifact path>:<artifact
hash>` as set at transcript import). An unknown id prints
`{"ok":false,"error":{"code":"no_match",...}}` to stdout and exits 2
(consistent with `query context`); an ambiguous handle exits 1.

## Sections

The answer is trust-separated into distinct sections:

| Section | Contents | Trust |
|---|---|---|
| `touched` | Deterministic code-graph footprint: `File` / `Symbol` records named by `TOUCHED_FILE` / `MENTIONS_SYMBOL` edges from the session's members | `source_derived` — code-graph facts only; no agent-authored record can appear here |
| `observations` | Agent-authored `Observation` members | `agent_unverified` / `agent_verified` / `agent_contradicted` |
| `failures` | `FAILED_ON` edges (with their failed-on code targets) plus agent `Failure` claims | agent-authored |
| `artifacts` | Targets of `PRODUCED_PATCH` / `PRODUCED_EVIDENCE` edges | `artifact` |
| `verification_evidence` | Verification-domain records linked to the session (edge-derived members, `session_id`-stamped records, `VALIDATED_BY` targets) | `verification_evidence` |

Every returned row carries a `record_id` and at least one citation handle
(repo-relative path/span or record handle). The top-level
`verification_status` is `verified` | `unverified` | `failed`, derived **only**
from verification-domain evidence linked to the session — a failing check
beats a passing one; with no linked checks the status is `unverified`. Agent
claims never promote it.

Session membership is edge-derived only (`AUTHORED_BY` / `SESSION_OF`, at most
3 hops from the session node), mirroring `eg query sessions`: a matching
`session_id` string is not membership by itself.

## Standing disclaimer

"No footprint found is not evidence the run did nothing." A run that never
wrote a cited edge, an import that dropped its transcript, or a store that was
only partially ingested all look the same as an idle session.
