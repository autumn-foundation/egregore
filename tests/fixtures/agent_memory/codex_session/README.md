# codex_session fixture

**Format:** `codex-cli-1.0` (session flavor)
**Codex version pinned:** OpenAI Codex CLI ≥ 0.1.2450 (January 2025 release train)
**Source:** Hand-crafted for Egregore M3 importer testing (issue #21).
**Task:** Fix the `add` function in `calc.py` to return `a + b` instead of `a - b`.

## What This Fixture Exercises

| Event kind              | Line(s) | Description |
|-------------------------|---------|-------------|
| `session` header        | 1       | Session metadata (model, id, created_at) |
| User turn               | 2       | Initial user request |
| Assistant turn (prose)  | 3       | First assistant response with usage |
| `function_call` (cat)   | 4       | shell tool call: read file |
| `function_call_output`  | 5       | exit_code=0, stdout present |
| Assistant turn          | 6       | Second response with usage |
| `function_call` (sed)   | 7       | shell tool call: file edit via sed -i |
| `function_call_output`  | 8       | exit_code=0, no output |
| Assistant turn          | 9       | Third response with usage |
| `function_call` (pytest)| 10      | shell tool call: verification |
| `function_call_output`  | 11      | exit_code=0, test passing stdout |
| Assistant turn          | 12      | Fourth response with usage |
| `function_call` (patch) | 13      | shell tool call: git apply (will fail) |
| `function_call_output`  | 14      | exit_code=1, stderr with patch error |
| Assistant turn (aborted)| 15      | Fifth response, status=incomplete |
| `interrupted` event     | 16      | User-initiated interruption |

## Expected Egregore Record Kinds

| Kind            | Count | Notes |
|-----------------|-------|-------|
| `AgentSession`  | 1     | Covers the full session |
| `AgentRun`      | 1     | One bounded task attempt |
| `AgentTurn`     | 5     | One per assistant message |
| `ToolCall`      | 4     | One per function_call |
| `CommandRun`    | 4     | One per function_call_output |
| `FileEdit`      | 1     | Turn 2 sed -i edit |
| `PatchArtifact` | 1     | Turn 4 git apply (invalid) |
| `Failure`       | 2     | Turn 4 exit_code=1; Turn 5 aborted |
| `Verification`  | 1     | Turn 3 pytest run exit_code=0 |
| `Diagnostic`    | 1+    | Interruption event |
| `CostUsage`     | 5     | One per assistant message with usage |

## Idempotency

Session ID derived from `BLAKE3(raw JSONL bytes)` + `importer_version` + `IMPORTER_ID`.
Re-importing this file produces the same `AgentSession` ID every time.

## Determinism

Re-importing 5 times produces byte-for-byte identical JSONL (canonical sort applied by `Graph::to_jsonl()`).
