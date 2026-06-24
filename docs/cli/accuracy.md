# eg audit accuracy

Measure the **precision and recall of code-graph extraction** against a checked-in, human-reviewed labeled Rust corpus — answering the maintainer question *"are the extracted deterministic facts correct, or is the extractor silently dropping symbols, attributing wrong spans, or inventing spurious edges?"* (issue #93).

This is a **measurement gate** to verify the correctness of the facts produced by the Rust extractor. It does not expand the extractor's vocabulary or change query behavior.

> [!NOTE]
> **Extraction correctness is distinct from other quality gates:**
> * **Determinism (`deterministic_scan`):** Ensures that repeated scans of the same codebase yield byte-identical graph outputs, but does not measure whether those facts are correct.
> * **Self-reported incompleteness (#87 / Diagnostic nodes):** Captures only the limitations the extractor *admitted* (e.g., macro invocation boundaries), but cannot detect silent misses (e.g., failing to parse a function) or false positive edges.
> * **Token cost (#84):** Measures the token size/savings ratio of query answers compared to a raw text search baseline.
> * **Semantic relevance (#58):** Calibrates the embedding-derived search layer.
> * **Latency (#57):** Gathers speed/performance metrics.

---

## Synopsis

```text
eg audit accuracy [--corpus-dir <PATH>] [--labels <PATH>] [--span-line-tolerance <INT>] [--min-precision <F>] [--min-recall <F>] [--format json|text]
```

### Options
* `--corpus-dir <PATH>` — Directory containing the Rust source files to scan. Default: `corpus/accuracy`.
* `--labels <PATH>` — JSON file containing the expected ground-truth nodes and edges. Default: `corpus/accuracy_labels.json`.
* `--span-line-tolerance <INT>` — Maximum allowed line difference when comparing actual extract spans with expected spans. Default: `0` (exact line match).
* `--min-precision <F>` — Override the minimum precision threshold for all classes (0.0 to 1.0). If omitted, falls back to the thresholds defined in the labels file (or `0.98` for Symbols and DEFINES).
* `--min-recall <F>` — Override the minimum recall threshold for all classes. If omitted, falls back to labels thresholds (or `0.95` for Symbols and DEFINES).
* `--format <json|text>` — Output format. Default: `json` (newline-delimited JSON report).

---

## Exit Codes

* `0` — Gate passed (`ok: true`; all metrics meet or exceed target thresholds).
* `1` — Gate failed (`ok: false`; one or more metrics fell below thresholds; full JSON report is printed).
* `2` — Loader or usage error (e.g. missing files, unparseable JSON/Rust, scanning error).

---

## How It Works

1. **Deterministic Scan:** Runs the Rust extractor over the `--corpus-dir` with a stable repository ID override (`accuracy-corpus`) to generate the in-memory graph of actual records.
2. **Node Matching:** Performs greedy 1-to-1 matching of expected nodes to scanned actual nodes. A match requires identical kind, normalized repo-relative path, name, and symbol sub-kind.
3. **Span Verification:** Checked under `--span-line-tolerance`. If a scanned node has the correct name but its span differs by more than the tolerance, it is reported as a **span miss** (counted as a False Negative for the expected span, and a False Positive for the scanned actual span).
4. **Edge Matching:** Resolves the source/target node IDs of scanned edges using the node match mapping, and compares the resulting relations against expected edges.
5. **Precision & Recall Calculation:**
   * $\text{Precision} = \frac{\text{True Positives}}{\text{True Positives} + \text{False Positives}}$
   * $\text{Recall} = \frac{\text{True Positives}}{\text{True Positives} + \text{False Negatives}}$
6. **Threshold Enforcement:** Validates computed metrics against thresholds. If any class falls below its target, diagnostics are printed to `stderr` and the process exits with `1`.

---

## Scoping & Rust Constructs

The labeled accuracy corpus covers common constructs to verify the extractor's capabilities:

### In Scope (In-Tree Checked)
* **Free functions:** Extracted as `Symbol` nodes (kind `"function"`).
* **Generics:** Validates that generic parameters on structs/impls are parsed without malforming symbol names.
* **Structs & Impls:** Checks struct definitions, implementation blocks, and methods (`Symbol` nodes of kinds `"struct"`, `"impl"`, `"method"`).
* **Traits & Trait Impls:** Checks trait declarations and their implementation blocks (`IMPLEMENTS` and `DEFINES` edges).
* **Nested Modules:** Verifies modules and module scoping (`Module` nodes, `CONTAINS` edges).
* **Re-exports & Aliases:** Verifies `pub use ... as ...` imports (`Import` nodes).

### Out of Scope / Known Limitations (Flagged via Diagnostics)
* **Macro Code Generation:** The extractor is a syntactic Tree-sitter parser and does not expand macros. Definitions hidden inside macro invocations are out-of-scope for extraction recall, but the macro call site itself must be extracted as a `Diagnostic` node (honestly flagged gap) rather than counted as a silent miss.
* **Decoy Calls:** Decoy function calls written inside comments or string literals must NOT generate any `CALLS` or `MENTIONS` edges. The evaluator verifies that comment/string-literal decoy calls yield exactly `0` spurious relations.
