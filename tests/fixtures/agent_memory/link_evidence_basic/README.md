# link_evidence_basic fixture

**Format:** `mini-swe-agent-1.2`
**Schema version:** `{"major": 1, "minor": 9}`
**Source:** Hand-crafted for Egregore link-evidence testing (issue #43).
**Task:** Rename the `answer` function in `src/lib.rs` to `solution` and run tests.

## What This Fixture Exercises

| Turn | Command | File | Expected result |
|------|---------|------|----------------|
| 0 | `tee src/lib.rs` | `src/lib.rs` | FileEdit → TOUCHED_FILE (file exists in rust_basic code graph) |
| 1 | `tee src/missing_helper.rs` | `src/missing_helper.rs` | FileEdit → diagnostic (file absent from code graph) |
| 2 | `patch -p1 src/lib.rs < changes.patch` | — | PatchArtifact, Failure (patch_invalid) |
| 3 | `cargo test` | — | Verification (no file link; trust boundary) |

## Expected Egregore Records (after import-traj)

| Kind | Count | Notes |
|------|-------|-------|
| `AgentSession` | 1 | Covers the full trajectory |
| `AgentRun` | 1 | One bounded task attempt |
| `AgentTurn` | 4 | One per assistant→user pair |
| `ToolCall` | 4 | One bash invocation per turn |
| `CommandRun` | 4 | One shell command per turn |
| `FileEdit` | 2 | Turn 0 (`src/lib.rs`), Turn 1 (`src/missing_helper.rs`) |
| `PatchArtifact` | 1 | Turn 2 patch attempt (invalid) |
| `Failure` | 1 | Turn 2 patch_invalid failure |
| `Verification` | 1 | Turn 3 cargo test (passed) |

## Expected link-evidence Output

When paired with the `rust_basic` code graph (which contains `src/lib.rs`):

| Source kind | File | Result |
|-------------|------|--------|
| FileEdit | `src/lib.rs` | TOUCHED_FILE edge → File record |
| FileEdit | `src/missing_helper.rs` | diagnostic: `missing_file` |

## Determinism

Re-running `link-evidence` 5 times on the same inputs produces byte-for-byte identical output.
