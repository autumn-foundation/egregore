# `eg capture-coverage`

Capture one `cargo llvm-cov` (or any llvm-cov-compatible) coverage run as a
citable, deterministic **verification-domain** `CoverageReport` record.

This is CAPTURE-ONLY. `eg capture-coverage` **never executes a coverage
tool**. The caller runs the coverage tool, captures its machine-readable
artefact to a file, and hands that file plus the run metadata to
`eg capture-coverage`, which parses it into a redaction-safe graph record. A
captured coverage number is a recorded observation of one run: **high coverage
proves lines were executed by some test, not that behavior is correct**.

It is the coverage sibling of the test-runner capture workflow (issue #165,
`eg capture-tests`) and the Verus proof-capture workflow (issue #69) — the
third leg of the producer trifecta (pass/fail, proofs, coverage) the project's
quality gates name. It is distinct from static symbol→tests reachability
(issue #126): #126 finds which tests *could* reach a symbol via the call graph
without running anything, while `capture-coverage` records what a run
*actually* exercised.

## Shortest workflow

```bash
# 1. Run the coverage tool yourself, capturing its machine-readable artefact.
cargo llvm-cov --json --output-path cov.json
echo "exit=$?"

# 2. Capture that artefact as a citable CoverageReport record.
eg capture-coverage \
  --input cov.json \
  --out coverage.graph.jsonl \
  --session-id sess-42 \
  --commit "$(git rev-parse HEAD)" \
  --command "cargo llvm-cov --json --output-path cov.json" \
  --exit-code 0 \
  --executed-at 2026-07-19T12:00:00Z \
  --repo-root "$(pwd)"

# 3. (Optional) anchor each measured file/function to the code graph.
eg scan . --out code.graph.jsonl
eg capture-coverage --input cov.json --out coverage.graph.jsonl \
  --session-id sess-42 --commit "$(git rev-parse HEAD)" \
  --command "cargo llvm-cov --json --output-path cov.json" --exit-code 0 \
  --executed-at 2026-07-19T12:00:00Z --repo-root "$(pwd)" --graph code.graph.jsonl
```

LCOV artefacts work the same way (`cargo llvm-cov --lcov --output-path
lcov.info`, then `--format llvm-cov-lcov`); LCOV additionally carries
per-function names, so function-level anchoring is available there.

## Flags

| Flag | Required | Meaning |
| --- | --- | --- |
| `--input <path>` | yes | File holding the coverage artefact (stored, never executed). |
| `--out <path>` | yes | Output JSONL. |
| `--session-id <str>` | yes | Stable session identity; part of the record ID. |
| `--commit <str>` | yes | Commit handle / external identifier; part of the record ID. |
| `--command <str>` | yes | The exact command that produced the artefact. Stored, never run. |
| `--exit-code <i64>` | yes | The coverage command's exit status. |
| `--executed-at <rfc3339>` | yes | Caller-supplied timestamp. Validated RFC 3339. |
| `--suite <str>` | no | Suite/target name (node `name`; not part of the record ID). |
| `--tool <str>` | no | Coverage tool identity; part of the record ID. Default `cargo-llvm-cov`. |
| `--tool-version <str>` | no | Coverage tool version. |
| `--repo <str>` | no | Repository identity (reserved for scoping). |
| `--repo-root <dir>` | no | Absolute repository root used to relativize absolute artefact paths. Without it, paths are suffix-matched against the code graph when `--graph` is given. |
| `--graph <path>` | no | A code graph (from `eg scan`) used to resolve files/functions to File/Symbol. |
| `--format <str>` | no | Input format. `llvm-cov-json` (the default) or `llvm-cov-lcov`. |
| `--protected-raw-artifacts` | no | Capture raw input bytes into the protected store. Requires the two flags below. |
| `--protected-store <dir>` | no | Protected store directory. |
| `--producer <id>` | no | Authorised producer identity for the captured blob. |

## Records emitted (success)

- **One `CoverageReport` node** — `domain: "verification"`,
  `verification_kind: "coverage_report"`, stable ID
  `verification_stable_id(["coverage_report", session_id, commit, tool])`.
  Carries `status` (`pass` when `--exit-code` is 0, else `fail` — a nonzero
  coverage-tool exit means the figures themselves are suspect),
  `exit_code`, `executed_at`, `source_artifact_path` (the `--input` path),
  `source_artifact_hash` (BLAKE3 of the raw input bytes), temporal provenance
  (`git_commit`, `valid_time`/`observed_at` from `--executed-at`), and a
  bounded normalized summary in `stdout_handle` (format tag `llvm-cov-v1`):
  overall line coverage plus branch (LCOV) / region (JSON) / function figures
  when the artefact has them, and per-file `{path, resolved, lines,
  branches?, regions?, functions, function_details?}` sorted by path. The
  artefact's own reported percentages are used verbatim when present;
  otherwise `covered / total × 100` rounded to four decimals.
- **Cross-domain edges (only with `--graph`)** — for each measured file that
  resolves to a code-graph `File`: a `TOUCHED_FILE` edge (CoverageReport →
  File). For each named function (LCOV artefacts) whose final `::`-segment
  resolves to **exactly one** `Symbol` by name: a `MENTIONS_SYMBOL` edge
  (CoverageReport → Symbol). Resolution is conservative: **zero matches**
  emit **no edge** and a `coverage_file_unresolved` /
  `coverage_symbol_unresolved` `Diagnostic`; **two-plus** symbol matches emit
  a `coverage_symbol_ambiguous` `Diagnostic`. The code graph's own nodes are
  not re-emitted; union the output with the code graph to resolve edge
  endpoints (mirrors `eg resolve-frames`).
- **Distinct `unresolved` set** — files in the report that do not resolve to a
  code-graph `File` are listed in the capture envelope's `unresolved` section
  (and get a diagnostic each): never silently dropped, never a wrong edge.
  Without `--graph` the `CoverageReport` node is still emitted self-contained
  and every measured file is reported unresolved.

## Determinism

Record IDs carry no wall-clock. The producer stamped on every record uses the
caller-supplied `--executed-at` as its `producer_started_at` (never `now`), so
given identical input bytes and identical `--executed-at` the entire output
JSONL is byte-identical across runs (5 consecutive identical runs verified in
the integration suite).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Capture succeeded (a `CoverageReport` was written). |
| 1 | Usage / provenance error (bad `--executed-at`, unknown `--format`, incomplete `--protected-*` group, unreadable input). Machine-readable `{"code":..,"field":..}` diagnostic. |
| 3 | Protected-store I/O failure (no partial manifest). |
| 4 | Empty input file. A `Diagnostic` (`empty_coverage_report`) is written; no `CoverageReport`. |
| 5 | Unparseable input (not the declared format, or zero measurable files), or the coverage tool never ran: when the artefact carries the tool's own not-installed error text (`no such command`, `command not found`, …) a `Diagnostic` (`coverage_tool_missing`) is written instead of a zero/empty-but-"successful" coverage record. No `CoverageReport` either way. |

## Redaction / safety

Output JSONL carries only record IDs, handles, hashes, counts, percentages,
file paths, function names, statuses, redaction markers, and the bounded
normalized summary. Raw report text, source snippets, tokens, and environment
values never appear inline. Raw input bytes are retrievable only through the
protected store handle (reported to stdout, never written into the graph).

## Trust separation

A `CoverageReport` is deterministic runtime-derived evidence in the
`verification` domain — never an agent observation. Assistant prose such as
"coverage is fine" is never emitted as a `CoverageReport` unless linked to a
captured command whose output supports it.
