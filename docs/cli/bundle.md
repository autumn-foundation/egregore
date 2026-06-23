# Evidence Bundle — CLI Reference

**Status:** Active at v1. Enforces the compilation and verification of redaction-safe evidence bundles (Issue #68) containing transitive closures of graph records derived from query root selectors.

## Overview

The `bundle` command provides utilities to export, verify, and inspect redaction-safe evidence bundles. An evidence bundle is a single canonical JSON file containing:
- A **manifest** detailing the roots, Egregore version, repository identity, and record counts.
- A **canonically ordered list of redacted records** that form the transitive closure of directly linked evidence.
- A list of **unresolved links** detected during traversal.

Importantly, bundles do **not** contain raw sensitive payloads (such as prose transcripts, inline tool inputs/outputs, or patch hunks). These fields are scrubbed (set to `None`), while their BLAKE3 hashes and byte counts are preserved for audit verification.

---

## Commands

### 1. `export`

Computes the undirected transitive closure of directly linked records starting from the root selector, validates citation coverage requirements, scrubs raw payloads, and writes the canonical JSON bundle.

```powershell
eg bundle export --root-selector <selector> --graph <path> --out <path>
```

#### Arguments & Flags

- `--root-selector <selector>`: The starting selector. Supports:
  - `id:<record_id>` (any record ID)
  - `symbol:<symbol_name>` (symbol name prefix)
  - `file:<file_path>` (file path prefix)
  - `task:<task_id>` (task ID)
  - `memory:<memory_id>` (agent memory ID)
- `--graph <path>`: Path to the input graph JSONL file (mutually exclusive with `--data-dir`).
- `--data-dir <path>`: Path to the embedded AletheiaDB store directory (mutually exclusive with `--graph`).
- `--out <path>`: Output destination path for the serialized bundle JSON.

#### Behavior & Validation gates

- **Transitive Closure**: Traverses both explicit graph edges and inline relation fields (e.g. `approval_decision_id`, `materialized_record_id`) using an undirected Breadth-First Search (BFS).
- **Citation Coverage Gate**: Validation will fail and abort the export (exiting with code `1` and a JSON error) if the exported set does not meet:
  - **Code/topology records**: At least **95%** must carry a valid citation handle or documented absent-handle rule.
  - **Non-code records**: **100%** must carry a valid source/evidence/protected handle.
- **Redaction/Scrubbing**: Clears all raw prose fields (e.g. `text`, `valid_time_source`) and inline payload buffers (e.g. `stdout_handle.inline`, `arguments_handle.inline`), leaving only the stable hash and byte count.

---

### 2. `verify`

Performs a read-only, offline validation of an exported bundle. It checks the bundle against three quality and safety rules and outputs the results.

```powershell
eg bundle verify <path> [--format <json|text>]
```

#### Arguments & Flags

- `<path>`: Path to the bundle JSON file to verify.
- `--format <json|text>`: Output representation (defaults to `json`).

#### The Three Verdicts

- **Integrity**: Parses the bundle JSON, recalculates the BLAKE3 hashes of every scrubbed record, and verifies they match the hashes in the bundle. Also checks that the records are in canonical sorting order.
- **Coverage**: Audits that the selected roots and links meet the minimum citation thresholds (95% for code, 100% for non-code).
- **Safety**: Verifies that no raw sensitive classes (e.g. secret keys or unredacted credentials) appear in the exported records or in the verifier output.

If any verdict fails, the command exits with code `1`. If all pass, it exits with code `0`.

---

### 3. `inspect`

Prints a human-readable summary of the bundle manifest and any diagnostics/unresolved links.

```powershell
eg bundle inspect <path>
```

#### Output Fields

- **Root Selector**: The starting query selector used.
- **Source Query**: The kind of root query executed.
- **Repository Identity**: The identifier of the repository.
- **Egregore Version**: The version of Egregore used to export.
- **Omitted Records**: Counts of records excluded during traversal.
- **Included Records by Trust Class**: Counts categorized by trust class (e.g. `agent_authored`, `code_symbol`, `evidence`).
- **Root Record IDs Selected**: List of record IDs that matched the root selector.
- **Diagnostics (Unresolved Links)**: List of any broken outbound edges or referenced handles missing from the bundle.

---

## Workflow Example

1. **Scan the codebase**:
   ```powershell
   eg scan . --out graph.jsonl
   ```

2. **Ingest the graph (Dry-run or Embedded)**:
   ```powershell
   eg ingest graph.jsonl --adapter dry-run
   ```

3. **Export the Evidence Bundle**:
   ```powershell
   eg bundle export --root-selector symbol:my_function --graph graph.jsonl --out evidence_bundle.json
   ```

4. **Inspect the Manifest**:
   ```powershell
   eg bundle inspect evidence_bundle.json
   ```

5. **Verify the Bundle**:
   ```powershell
   eg bundle verify evidence_bundle.json --format text
   ```
