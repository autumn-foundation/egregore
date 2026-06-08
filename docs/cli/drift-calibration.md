# Semantic Drift Calibration — CLI Reference

**Status:** Active at v1. Enforces quality metrics for semantic drift detection over codebase history.

## Overview

The `eval-drift` command executes a local-first calibration workflow to measure and enforce the quality of semantic drift detection. By running semantic drift queries across a curated set of code evolution scenarios, it validates that the system can accurately distinguish meaningful behavior changes from structural adjustments, text edits, or comment-only changes without relying on remote or hosted indexing.

## Usage

Run the calibration against a local scenarios corpus:

```powershell
cargo run -- eval-drift --corpus corpus/drift_calibration_corpus.json --threshold 0.0005
```

### Options

* `--corpus <PATH>`: Path to the JSON file containing the drift calibration corpus. Defaults to `corpus/drift_calibration_corpus.json`.
* `--threshold <FLOAT>`: The cosine similarity threshold (range `0.0` - `1.0`) used to classify a change as semantic drift. Defaults to `0.20`.

## Labeled Corpus Scenarios

The system is evaluated using a labeled dataset of 30 scenarios representing four primary classes of code evolution:

1. **Meaning Changed (`meaning_changed`)** (15 scenarios): Real changes to execution logic, behavior, or boundary conditions. Examples include modifying arithmetic operators, switching comparison operators, changing string constants, or altering filter criteria.
2. **Structure Changed Only (`structure_changed_only`)** (5 scenarios): Pure refactorings where the logic is structurally reorganized, but runtime semantics remain identical. Examples include translating a loop to an iterator-based stream (`.iter().sum()`), renaming local variables, or introducing intermediate variables.
3. **Text Changed Only (`text_changed_only`)** (5 scenarios): Changes that do not affect syntax or execution, such as comments, formatting, indentation, or import sorting.
4. **Unchanged (`unchanged`)** (5 scenarios): Commits where no changes were made (empty commits or no-ops).

## Quality Gates and Tradeoffs

To pass calibration, the semantic drift detection must satisfy the following gates on the 30-scenario corpus:

* **Precision:** $\ge 0.75$ for `meaning_changed` scenarios.
* **Recall:** $\ge 0.70$ for `meaning_changed` scenarios.
* **Unchanged False Positives:** Exactly $0$ unchanged scenarios can have drift detected above the threshold.
* **Determinism:** Results must be 100% byte-equivalent and ordered identically across multiple consecutive runs.

### Threshold Selection and Tradeoffs

The default release threshold is set to **`0.20`**.

Due to the nature of dense vector embeddings, there are distinct trade-offs between precision and recall depending on the threshold:

| Threshold | Target Behavior | Expected Recall | Expected Precision | Trade-off Description |
|-----------|-----------------|-----------------|--------------------|-----------------------|
| `0.20` (Default) | High-Precision | ~0.70 | ~0.90 | Minimizes noise from structural refactoring and code organization updates at the cost of missing extremely subtle single-character semantic shifts. |
| `0.0005` | High-Recall | 1.00 | ~0.75 | Catches every possible semantic change, but triggers false positives on structural changes like loop-to-iterator refactorings. |

> [!NOTE]
> Since abstract syntax tree (AST) structure and syntactic shape influence token layouts, pure structural changes (`structure_changed_only`) yield non-zero cosine distances (typically between `0.05` and `0.10`). At the default threshold of `0.20`, these false positives are successfully filtered out.

## Comparison Against Boring Baselines

AletheiaDB's semantic drift detection provides qualitative advantages over standard developer tools:

| Method | Detects Meaning | Ignores Format/Comments | Spans Resolved | Multi-Commit Context |
|--------|:---------------:|:-----------------------:|:--------------:|:--------------------:|
| **Semantic Drift** | **Yes** | **Yes** | **Yes** | **Yes** |
| `git diff` | No | No | No | No |
| `git log -S` | Partially | No | No | No |
| `grep`/`ripgrep` | No | No | No | No |

### Baseline Details

1. **`git diff`**: Fails to distinguish formatting, comments, or imports from true logical updates. It cannot evaluate meaning or compute semantic significance.
2. **`git log -S<string>`**: Looks for occurrences of specific string patterns. It misses semantic context (e.g. changing `>` to `>=`) and cannot trace drift to a specific AST node or symbol.
3. **`ripgrep`**: Static search tool that lacks temporal awareness, commit history, and AST-level mapping.

## CLI Output Explanation

When the `eval-drift` command finishes, it prints a structured report.

```text
Semantic Drift Calibration Report
=================================
Evaluation threshold: 0.20
Total scenarios: 30
Metrics:
  Precision: 0.9333 (pass: true)
  Recall:    0.9333 (pass: true)
  Unchanged false positives: 0 (pass: true)
  Status:    PASS

Scenario Details:
-----------------
  [meaning_01] class=meaning_changed        detected=true  max_score=0.3412 | Baselines: diff=true  log_s=true  rg=true
         - model=all-MiniLM-L6-v2 score=0.3412 thresh=0.20 path=src/lib.rs 9:10 status="drift is a lead, not proof"
  [structure_01] class=structure_changed_only detected=false max_score=0.0821 | Baselines: diff=true  log_s=true  rg=true
  [text_01] class=text_changed_only      detected=false max_score=0.0000 | Baselines: diff=true  log_s=false rg=false
  [unchanged_01] class=unchanged           detected=false max_score=0.0000 | Baselines: diff=false log_s=false rg=false
```

For each detected drift, the output displays:
* Before/After commit handles (retrieved from repository history).
* File path and line span of the affected symbol.
* Cosine drift score and the threshold used.
* The model name used for embedding generation.
* The diagnostic caveat: `status="drift is a lead, not proof"`.
