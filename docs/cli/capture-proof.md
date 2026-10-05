# `eg capture-proof` (issue #69)

Capture a **local Verus proof run** as citable verification-domain graph records.

Unlike its capture-only siblings, `capture-proof` **executes** the verifier: it runs your local `verus` binary on a proof target and records the outcome. Egregore never installs or fetches Verus for you — the operator must have a real one.

## When to use it

Use `capture-proof` when you want the graph to say *"Verus proved this target with no errors"* and have that claim backed by the captured verifier command. Use it for:

- **Verus proof evidence** — the target is a Verus-annotated file and the evidence is the verifier's own verdict.

Reach for the siblings instead when the evidence is something else:

- `eg capture-tests` — **cargo test / libtest JSON output**. Capture-only; never executes.
- `eg capture-coverage` — **coverage reports**.
- `eg capture-bench` — **benchmark runs**.
- CI logs, terminal transcripts, or anything else → the matching capture workflow for that artifact.
- Human judgment ("I reviewed this and it looks right") → an `Observation`, not a proof record.

A captured pass proves **the verifier result**, not that every requirement is satisfied. The graph records what Verus said about the target; whether the target's specification actually captures the requirement is a separate claim, closed by a human-linked acceptance criterion.

## Usage

```bash
eg capture-proof \
  --target src/proofs/ledger.rs \
  --verus-bin ~/.cargo/bin/verus \
  --session sess-2026-09-30 \
  --commit $(git rev-parse HEAD) \
  --executed-at 2026-09-30T12:00:00Z \
  --out proof.graph.jsonl
```

Flags:

| Flag | Required | Meaning |
|---|---|---|
| `--target` | yes | Proof target file to verify. |
| `--verus-bin` | no (default `verus` on PATH) | Local Verus binary. A bare name is resolved against PATH. |
| `--verus-arg` | no, repeatable | Extra arguments passed to the verifier before the target. |
| `--out` | yes | Output JSONL path. |
| `--session` | yes | Stable session identity (part of the record ID). |
| `--commit` | yes | Commit SHA the target is evaluated at (part of the record ID, and the temporal handle). |
| `--executed-at` | yes | Caller-supplied RFC 3339 timestamp (validated). This — not a wall clock — is what makes the capture deterministic. |
| `--verifier-version` | no | Skip the `verus --version` probe and use this version string. |
| `--repo` | no | Repository identity label stored on the records. |
| `--timeout-secs` | no (default 600) | Wall-clock budget for the verifier run. |
| `--probe-timeout-secs` | no (default 30) | Budget for the `verus --version` probe. |
| `--protected-raw-artifacts` + `--protected-store` + `--producer` | no | Also store the raw (unredacted) verifier stdout/stderr in the protected store for audit. |

## What it emits

Two verification-domain records, in canonical order:

1. **`CommandRun`** — the captured verifier command: exact argv, exit code, redacted stdout/stderr handles, wall-clock `started_at`/`finished_at` (volatile metadata, excluded from canonical comparison).
2. **`ProofResult`** — the normalized proof claim: verifier name + version, `verified`/`errors` counts parsed from the `verification results::` line, exit code, target path + BLAKE3 hash, in `stdout_handle` as `egregore/proof-summary#1` JSON; linked to the `CommandRun` via an `evidence_links` `HAS_EVIDENCE` citation.

Proof statuses are `pass`, `fail`, `timeout`, and `error`. Classification is **fail-closed**:

- `timeout` — the verifier exceeded `--timeout-secs` and was killed. The proof is Failing, never Passing.
- `error` + `verus_exit_code_mismatch` — clean summary, nonzero exit: the verifier disagrees with itself.
- `error` + `malformed_verus_output` — a `verification results::` line exists but its counts don't parse.
- `error` + `unrecognized_verus_output` — no recognizable Verus output shape at all (a verifier-error `Diagnostic` is also emitted; no proof claim is recorded).

Pre-execution failures — missing/non-executable verifier binary, unsupported verifier version (no recognizable `--version` token), missing/stale/ambiguous proof target — emit a **stable diagnostic-only record** and exit 2. The verifier never ran, so there is no `ProofResult`.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Captured — **any** proof status. A failing PROOF is still a successful CAPTURE: the operator proved the claim false, and that is evidence. The status lives in the records. |
| 1 | Usage error (bad `--executed-at`, bad timeout, incomplete protected-store trio). |
| 2 | Pre-execution failure (diagnostic written to `--out`, stable code in the envelope). |
| 3 | Protected-store I/O failure. |

## Determinism

Stable IDs are `verification_stable_id(["proof_result" | "command_run", session, commit, target])` — no wall clock. Five identical runs produce byte-identical output once the documented volatile `CommandRun` wall-clock fields (`started_at`/`finished_at`) are masked.

## Trust and closing criteria

Both records classify as the existing `deterministic-but-runtime-derived` verification trust class — no new trust class, no new edge vocabulary. To close an acceptance criterion on a proof run, link the criterion to the captured **`CommandRun`** via `CLOSES_ACCEPTANCE_CRITERION` (that is the edge the task evidence gate follows): a `pass` satisfies the gate; `fail`/`timeout`/`error` block it with `FailingEvidence`.

## Redaction

Verifier stdout/stderr are redacted **per span** before persistence: each detected secret becomes an auditable `<REDACTED:class:hash_prefix>` marker; hashes cover the redacted bytes. Output above the 16 KiB inline ceiling is represented by hash/byte-count handles only, never inline. `redaction_policy_version: "v1"` is stamped on every record.
