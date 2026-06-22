# Issue 87: Flag Incomplete Extraction in Code Query Answers Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Surface a deterministic extraction-completeness signal (`complete` or `partial`) in symbol and file query results, enumerating the in-scope `Diagnostic` markers for file queries.

**Architecture:** 
Extend `SymbolResult` and `TxSymbolRow` in `src/cli.rs` to include `extraction_completeness` (`complete` / `partial`) and `diagnostics` (an optional list of in-scope diagnostics). Scan the graph records for matching active `Diagnostic` nodes in the queried file/scope.

**Tech Stack:** Rust (standard library and serde serialization)

---

## User Review Required

> [!IMPORTANT]
> The completeness signal is advisory: `partial` denotes that macro-hidden or unparsed regions exist in the file scope, but does not claim that a queried symbol is definitively missing or present.

## Open Questions

None. The issue requirements are very clear.

## Proposed Changes

### CLI & Query Results

#### [MODIFY] [cli.rs](file:///c:/Users/markm/egregore/src/cli.rs)

1. **Define `DiagnosticRef`** to represent the serialized diagnostic metadata:
   ```rust
   #[derive(Serialize, Clone, Eq, PartialEq, Debug)]
   struct DiagnosticRef<'a> {
       record_id: &'a str,
       repo_relative_path: &'a str,
       span: SourceSpan,
   }
   ```
2. **Add completeness fields** to `SymbolResult<'a>` and `TxSymbolRow<'a>`:
   ```rust
   extraction_completeness: &'static str,
   #[serde(skip_serializing_if = "Option::is_none")]
   diagnostics: Option<Vec<DiagnosticRef<'a>>>,
   ```
3. **Implement helper function `get_file_diagnostics`**:
   ```rust
   fn get_file_diagnostics<'a>(
       records: &'a [GraphRecord],
       file_path: &str,
       deleted: &std::collections::BTreeSet<&str>,
   ) -> (&'static str, Option<Vec<DiagnosticRef<'a>>>) {
       let mut diagnostics = Vec::new();
       for r in records {
           if let GraphRecord::Node {
               id,
               kind: NodeKind::Diagnostic,
               repo_relative_path: Some(path),
               span: Some(span),
               ..
           } = r
           {
               if path == file_path && !deleted.contains(id.as_str()) {
                   diagnostics.push(DiagnosticRef {
                       record_id: id.as_str(),
                       repo_relative_path: path.as_str(),
                       span: *span,
                   });
               }
           }
       }
       if diagnostics.is_empty() {
           ("complete", None)
       } else {
           diagnostics.sort_by_key(|d| (d.span.start_line, d.record_id));
           ("partial", Some(diagnostics))
       }
   }
   ```
4. **Update `symbol_result` and `tx_symbol_row`** to populate these fields.
5. **Update `query_file`** to populate these fields.
6. **Update `PrintText` for `SymbolResult`** to append the signal in text mode:
   ```rust
   let completeness = format!(" (extraction: {})", self.extraction_completeness);
   // Append `completeness` to format string
   ```

---

## Verification Plan

### Automated Tests
1. **Add new integration tests in [query_cli.rs](file:///c:/Users/markm/egregore/tests/query_cli.rs)**:
   - Seed a fixture containing one file with a `Diagnostic` record (e.g., from a macro invocation) and one file with zero diagnostics.
   - Run `query file` on the clean file and verify it returns `complete` and no diagnostics in `--format json`.
   - Run `query file` on the dirty file and verify it returns `partial` and enumerates the diagnostic record ID and span.
   - Run `query symbol` on a symbol in the dirty file and verify it returns `partial`.
   - Verify that running the query 5 consecutive times yields byte-identical output.
   
2. **Execute tests**:
   - `cargo test --test query_cli`
   - `cargo test --all-targets --all-features`

### Manual Verification
None required since we have 100% automated test coverage of the CLI query output.
