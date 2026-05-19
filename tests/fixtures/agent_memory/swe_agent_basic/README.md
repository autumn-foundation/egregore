# swe_agent_basic fixture

**Format:** `mini-swe-agent-1.2`  
**Schema version:** `{"major": 1, "minor": 9}`  
**Source:** Hand-crafted for Egregore M2 importer testing (issue #9).  
**Task:** Fix the `add` function in `src/calc.py` to return `a + b` instead of `a - b`.

## What This Fixture Exercises

| Event kind       | Message indices | Description |
|-----------------|-----------------|-------------|
| Happy-path turn  | 2→3             | `cat src/calc.py` → exit 0 (file exploration) |
| File edit        | 4→5             | `sed -i` modifies `src/calc.py` → exit 0 |
| Failed patch     | 6→7             | `patch -p1` fails → exit 1, hunk FAILED |
| Verification     | 8→9             | `python -m pytest tests/test_calc.py` → exit 0, 2 passed |

## Expected Egregore Record Kinds

| Kind            | Count | Notes |
|-----------------|-------|-------|
| `AgentSession`  | 1     | Covers the full trajectory |
| `AgentRun`      | 1     | One bounded task attempt |
| `AgentTurn`     | 4     | One per assistant→user pair |
| `ToolCall`      | 4     | One bash invocation per turn |
| `CommandRun`    | 4     | One shell command per turn |
| `FileEdit`      | 1     | Turn 1 sed -i edit |
| `PatchArtifact` | 1     | Turn 2 patch application (invalid) |
| `Failure`       | 1     | Turn 2 exit_code=1 patch failure |
| `Verification`  | 1     | Turn 3 pytest run |

## Idempotency

The `AgentSession` ID is derived from `BLAKE3(raw .traj bytes)` + `importer_version`.  
Re-importing this file produces the same `AgentSession` ID every time.

## Determinism

Re-importing 5 times produces byte-for-byte identical JSONL (canonical sort applied by `Graph::to_jsonl()`).
