# codex_rollout fixture

**Format:** `codex-cli-1.0` (rollout flavor)
**Codex version pinned:** OpenAI Codex CLI ≥ 0.1.2450 (January 2025 release train)
**Source:** Hand-crafted for Egregore M3 importer testing (issue #21).
**Task:** Add a `multiply` function to `calc.py`.

## Session vs Rollout Flavor

The rollout flavor begins with `{"type":"rollout",...}` instead of `{"type":"session",...}`.
A rollout carries `run_id` and `session_id` as top-level fields rather than deriving them.
The event body structure (message, function_call, function_call_output, interrupted) is identical.

## What This Fixture Exercises

| Event kind               | Line(s) | Description |
|--------------------------|---------|-------------|
| `rollout` header         | 1       | Run metadata (model, run_id, session_id, started_at) |
| User turn                | 2       | Initial user request |
| Assistant turn (prose)   | 3       | First assistant response with usage |
| `function_call` (cat)    | 4       | shell tool call: read file |
| `function_call_output`   | 5       | exit_code=0, stdout present |
| Assistant turn           | 6       | Second response with usage |
| `function_call` (tee)    | 7       | shell tool call: file edit via tee -a |
| `function_call_output`   | 8       | exit_code=0, stdout showing appended content |
| Assistant turn           | 9       | Third response with usage |
| `function_call` (cargo)  | 10      | shell tool call: cargo test (verification) |
| `function_call_output`   | 11      | exit_code=0, test passing stdout |
| Assistant turn (aborted) | 12      | Fourth response, status=incomplete |
| `interrupted` event      | 13      | wallclock_timeout interruption |

## Expected Egregore Record Kinds

| Kind            | Count | Notes |
|-----------------|-------|-------|
| `AgentSession`  | 1     | Covers the full rollout |
| `AgentRun`      | 1     | One bounded task attempt |
| `AgentTurn`     | 4     | One per assistant message |
| `ToolCall`      | 3     | One per function_call |
| `CommandRun`    | 3     | One per function_call_output |
| `FileEdit`      | 1     | Turn 2 tee -a edit |
| `Failure`       | 1     | Turn 4 aborted/incomplete |
| `Verification`  | 1     | Turn 3 cargo test exit_code=0 |
| `Diagnostic`    | 1+    | Interruption event |
| `CostUsage`     | 4     | One per assistant message with usage |

## Idempotency

Session ID derived from `BLAKE3(raw JSONL bytes)` + `importer_version` + `IMPORTER_ID`.
Re-importing this file produces the same `AgentSession` ID every time.

## Determinism

Re-importing 5 times produces byte-for-byte identical JSONL (canonical sort applied by `Graph::to_jsonl()`).
