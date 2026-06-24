# Issue 95: Orient agents in an unfamiliar repo with a deterministic code map Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement `eg query orient` CLI command and API to return a repository orientation map (entry points, module/file tree with symbol counts, and top-N referenced symbols ranked by inbound graph degree).

**Architecture:**
1. Implement data types and logic for the orientation map in `src/query.rs`.
2. Add the `Orient` subcommand to `QuerySubcommand` in `src/cli.rs`.
3. Handle error conditions (empty graph exit code 3, no entry points exit code 4) and formatting options (`--format json` and `--format text`).
4. Write comprehensive unit and integration tests to verify correctness, determinism, and error cases.

**Tech Stack:** Rust (standard library and serde serialization)

---

## User Review Required

> [!IMPORTANT]
> - **Exit Codes**: The command will exit with `3` on an empty graph/zero code-graph nodes, and with `4` if no entry-point files are found.
> - **Absent Span Citation**: Tree directories and symbols without a span will use the documented `AbsentHandleRule::NoSpanModuleLevel` reason.
> - **Path separators**: All paths in the module tree are normalized to forward slash (`/`) for consistency.

## Open Questions

None.

## Proposed Changes

### Core Query Logic

#### [MODIFY] [query.rs](file:///c:/Users/markm/egregore/src/query.rs)

1. **Add dependencies**:
   ```rust
   use crate::citation_audit::AbsentHandleRule;
   ```
2. **Define target structs**:
   ```rust
   #[derive(Serialize, Clone, Eq, PartialEq, Debug)]
   pub struct EntryPoint {
       pub record_id: String,
       pub repo_relative_path: String,
   }

   #[derive(Serialize, Clone, Eq, PartialEq, Debug)]
   pub struct ModuleTreeNode {
       pub name: String,
       pub path: String,
       pub kind: String, // "directory" or "file"
       pub symbol_count: usize,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub record_id: Option<String>,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub absent_handle_reason: Option<AbsentHandleRule>,
       pub children: Vec<ModuleTreeNode>,
   }

   #[derive(Serialize, Clone, Eq, PartialEq, Debug)]
   pub struct ReferencedSymbol {
       pub record_id: String,
       pub name: String,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub repo_relative_path: Option<String>,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub span: Option<SourceSpan>,
       #[serde(skip_serializing_if = "Option::is_none")]
       pub absent_handle_reason: Option<AbsentHandleRule>,
       pub inbound_degree: usize,
   }

   #[derive(Serialize, Clone, Eq, PartialEq, Debug)]
   pub struct OrientationMap {
       pub entry_points: Vec<EntryPoint>,
       pub module_tree: Vec<ModuleTreeNode>,
       pub top_referenced_symbols: Vec<ReferencedSymbol>,
   }

   #[derive(Debug, Clone, Eq, PartialEq)]
   pub enum OrientationError {
       EmptyGraph,
       NoEntryPoints,
   }
   ```
3. **Implement path classifier helper**:
   ```rust
   pub fn is_entry_point(path: &str) -> bool {
       let normalized = path.replace('\\', "/");
       normalized == "src/lib.rs"
           || normalized == "src/main.rs"
           || normalized.ends_with("/src/lib.rs")
           || normalized.ends_with("/src/main.rs")
           || (normalized.starts_with("src/bin/") && normalized.ends_with(".rs"))
           || (normalized.contains("/src/bin/") && normalized.ends_with(".rs"))
   }
   ```
4. **Implement core `orientation_map` query function**:
   ```rust
   struct TrieNode {
       name: String,
       path: String,
       is_file: bool,
       symbol_count: usize,
       record_id: Option<String>,
       children: BTreeMap<String, TrieNode>,
   }

   impl TrieNode {
       fn compute_transitive_counts(&mut self) -> usize {
           let children_sum: usize = self.children.values_mut()
               .map(|child| child.compute_transitive_counts())
               .sum();
           if !self.is_file {
               self.symbol_count = children_sum;
           }
           self.symbol_count
       }
   }

   fn convert_trie_node(node: TrieNode) -> ModuleTreeNode {
       let mut children: Vec<ModuleTreeNode> = node.children
           .into_values()
           .map(convert_trie_node)
           .collect();
       ModuleTreeNode {
           name: node.name,
           path: node.path,
           kind: if node.is_file { "file".to_string() } else { "directory".to_string() },
           symbol_count: node.symbol_count,
           record_id: node.record_id,
           absent_handle_reason: if node.is_file { None } else { Some(AbsentHandleRule::NoSpanModuleLevel) },
           children,
       }
   }

   pub fn orientation_map(
       records: &[GraphRecord],
       repo_id: Option<&str>,
       limit: usize,
   ) -> Result<OrientationMap, OrientationError> {
       let tombstoned_ids: BTreeSet<&str> = records
           .iter()
           .filter_map(|r| {
               if let GraphRecord::Tombstone { deleted_id, .. } = r {
                   Some(deleted_id.as_str())
               } else {
                   None
               }
           })
           .collect();

       let index = RepositoryIndex::build(records);

       let is_owned = |id: &str| -> bool {
           if let Some(r_id) = repo_id {
               index.owner_of(id) == Some(r_id)
           } else {
               true
           }
       };

       let is_code_edge = |label: EdgeLabel| -> bool {
           matches!(
               label,
               EdgeLabel::Contains
                   | EdgeLabel::Defines
                   | EdgeLabel::Imports
                   | EdgeLabel::References
                   | EdgeLabel::Calls
                   | EdgeLabel::Implements
                   | EdgeLabel::Mentions
           )
       };

       // 1. Zero code-graph nodes check
       let code_nodes_count = records.iter().filter(|r| {
           if let GraphRecord::Node { id, kind, .. } = r {
               if tombstoned_ids.contains(id.as_str()) {
                   return false;
               }
               if !is_owned(id.as_str()) {
                   return false;
               }
               matches!(
                   kind,
                   NodeKind::Repository
                       | NodeKind::File
                       | NodeKind::Module
                       | NodeKind::Symbol
                       | NodeKind::Import
                       | NodeKind::Diagnostic
               )
           } else {
               false
           }
       }).count();

       if code_nodes_count == 0 {
           return Err(OrientationError::EmptyGraph);
       }

       // 2. Entry points extraction
       let mut entry_points = Vec::new();
       for r in records {
           if let GraphRecord::Node { id, kind: NodeKind::File, repo_relative_path: Some(path), .. } = r {
               if tombstoned_ids.contains(id.as_str()) {
                   continue;
               }
               if !is_owned(id.as_str()) {
                   continue;
               }
               if is_entry_point(path) {
                   entry_points.push(EntryPoint {
                       record_id: id.clone(),
                       repo_relative_path: path.clone(),
                   });
               }
           }
       }
       if entry_points.is_empty() {
           return Err(OrientationError::NoEntryPoints);
       }
       entry_points.sort_by(|a, b| a.repo_relative_path.cmp(&b.repo_relative_path));

       // 3. Module/file tree
       let mut files = Vec::new();
       for r in records {
           if let GraphRecord::Node { id, kind: NodeKind::File, repo_relative_path: Some(path), .. } = r {
               if tombstoned_ids.contains(id.as_str()) {
                   continue;
               }
               if !is_owned(id.as_str()) {
                   continue;
               }
               files.push((id.clone(), path.clone()));
           }
       }

       let mut file_symbol_counts: BTreeMap<String, usize> = BTreeMap::new();
       for r in records {
           if let GraphRecord::Node { id, kind: NodeKind::Symbol, repo_relative_path: Some(path), .. } = r {
               if tombstoned_ids.contains(id.as_str()) {
                   continue;
               }
               if !is_owned(id.as_str()) {
                   continue;
               }
               *file_symbol_counts.entry(path.clone()).or_default() += 1;
           }
       }

       let mut trie_roots: BTreeMap<String, TrieNode> = BTreeMap::new();
       for (file_id, file_path) in &files {
           let normalized = file_path.replace('\\', "/");
           let segments: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
           if segments.is_empty() {
               continue;
           }
           let first_seg = segments[0].to_string();
           let count = file_symbol_counts.get(file_path).copied().unwrap_or(0);

           let mut curr_node = trie_roots.entry(first_seg.clone()).or_insert_with(|| TrieNode {
               name: first_seg.clone(),
               path: first_seg.clone(),
               is_file: segments.len() == 1,
               symbol_count: if segments.len() == 1 { count } else { 0 },
               record_id: if segments.len() == 1 { Some(file_id.clone()) } else { None },
               children: BTreeMap::new(),
           });

           for (i, seg) in segments.iter().enumerate().skip(1) {
               let subpath = segments[0..=i].join("/");
               let is_last = i == segments.len() - 1;
               curr_node = curr_node.children.entry(seg.to_string()).or_insert_with(|| TrieNode {
                   name: seg.to_string(),
                   path: subpath,
                   is_file: is_last,
                   symbol_count: if is_last { count } else { 0 },
                   record_id: if is_last { Some(file_id.clone()) } else { None },
                   children: BTreeMap::new(),
               });
           }
       }

       let mut roots: Vec<TrieNode> = trie_roots.into_values().collect();
       for root in &mut roots {
           root.compute_transitive_counts();
       }
       // Sort roots alphabetically
       roots.sort_by(|a, b| a.name.cmp(&b.name));
       let module_tree: Vec<ModuleTreeNode> = roots.into_iter().map(convert_trie_node).collect();

       // 4. Top-referenced symbols
       let mut inbound_degrees: BTreeMap<String, usize> = BTreeMap::new();
       let mut active_symbols = BTreeMap::new();
       for r in records {
           if let GraphRecord::Node { id, kind: NodeKind::Symbol, name, repo_relative_path, span, .. } = r {
               if tombstoned_ids.contains(id.as_str()) {
                   continue;
               }
               if !is_owned(id.as_str()) {
                   continue;
               }
               active_symbols.insert(
                   id.clone(),
                   (
                       name.clone().unwrap_or_default(),
                       repo_relative_path.clone(),
                       *span,
                   ),
               );
           }
       }

       for r in records {
           if let GraphRecord::Edge { id, label, target, source, .. } = r {
               if tombstoned_ids.contains(id.as_str()) {
                   continue;
               }
               if !is_code_edge(*label) {
                   continue;
               }
               if !is_owned(id.as_str()) {
                   continue;
               }
               if active_symbols.contains_key(target) {
                   *inbound_degrees.entry(target.clone()).or_default() += 1;
               }
           }
       }

       let mut ranked_symbols: Vec<ReferencedSymbol> = active_symbols
           .into_iter()
           .map(|(id, (name, repo_relative_path, span))| {
               let inbound_degree = inbound_degrees.get(&id).copied().unwrap_or(0);
               ReferencedSymbol {
                   record_id: id,
                   name,
                   repo_relative_path,
                   span,
                   absent_handle_reason: if span.is_none() { Some(AbsentHandleRule::NoSpanModuleLevel) } else { None },
                   inbound_degree,
               }
           })
           .collect();

       ranked_symbols.sort_by(|a, b| {
           b.inbound_degree
               .cmp(&a.inbound_degree)
               .then_with(|| a.record_id.cmp(&b.record_id))
       });
       ranked_symbols.truncate(limit);

       Ok(OrientationMap {
           entry_points,
           module_tree,
           top_referenced_symbols: ranked_symbols,
       })
   }
   ```

### CLI Wiring

#### [MODIFY] [cli.rs](file:///c:/Users/markm/egregore/src/cli.rs)

1. **Add `Orient` variant to `QuerySubcommand` enum**:
   ```rust
   /// Return a repository orientation map for cold-starting in an unfamiliar repository.
   Orient {
       /// Graph JSONL path (mutually exclusive with --data-dir).
       #[arg(long)]
       graph: Option<PathBuf>,
       /// Embedded `AletheiaDB` data directory (mutually exclusive with --graph).
       #[arg(long)]
       data_dir: Option<PathBuf>,
       /// Restrict symbol/file resolution to one repository.
       #[arg(long)]
       repo: Option<String>,
       /// Limit the number of top most-referenced symbols returned (default: 20).
       #[arg(long, default_value_t = 20)]
       limit: usize,
       /// Output format.
       #[arg(long, default_value = "json")]
       format: OutputFormat,
   },
   ```

2. **Handle `QuerySubcommand::Orient` in `query_cmd`**:
   ```rust
   QuerySubcommand::Orient {
       graph,
       data_dir,
       repo,
       limit,
       format,
   } => {
       let records = load_query_records(graph.as_deref(), data_dir.as_deref())?;
       let index = query::RepositoryIndex::build(&records);
       let selected = resolve_repo_scope(&index, repo.as_deref());
       query_orient_cmd(&records, selected.as_deref(), limit, format)
   }
   ```

3. **Implement `query_orient_cmd`**:
   ```rust
   fn query_orient_cmd(
       records: &[GraphRecord],
       repo_id: Option<&str>,
       limit: usize,
       format: OutputFormat,
   ) -> Result<()> {
       match query::orientation_map(records, repo_id, limit) {
           Ok(map) => {
               match format {
                   OutputFormat::Json => {
                       let envelope = serde_json::json!({
                           "ok": true,
                           "result": map,
                       });
                       println!("{}", serde_json::to_string_pretty(&envelope)?);
                   }
                   OutputFormat::Text => {
                       println!("Entry Points:");
                       for ep in &map.entry_points {
                           println!("- {} ({})", ep.repo_relative_path, ep.record_id);
                       }
                       println!("\nModule/File Tree:");
                       for node in &map.module_tree {
                           print_tree_node_text(node, 0);
                       }
                       println!("\nTop Referenced Symbols:");
                       for (i, sym) in map.top_referenced_symbols.iter().enumerate() {
                           let citation = if let Some(span) = sym.span {
                               let path = sym.repo_relative_path.as_deref().unwrap_or("");
                               format!(" @ {}:{}", path, span.start_line)
                           } else {
                               " [no_span_module_level]".to_string()
                           };
                           println!("{}. {} degree={}{} ({})", i + 1, sym.name, sym.inbound_degree, citation, sym.record_id);
                       }
                   }
               }
               Ok(())
           }
           Err(query::OrientationError::EmptyGraph) => {
               let code = "empty_graph";
               let msg = "graph has zero code-graph nodes";
               match format {
                   OutputFormat::Json => {
                       let envelope = serde_json::json!({
                           "ok": false,
                           "error": {
                               "code": code,
                               "message": msg
                           }
                       });
                       println!("{}", serde_json::to_string(&envelope)?);
                   }
                   OutputFormat::Text => {
                       eprintln!("Error: {}", msg);
                   }
               }
               std::process::exit(3);
           }
           Err(query::OrientationError::NoEntryPoints) => {
               let code = "no_entry_points";
               let msg = "no entry-point files found in the graph";
               match format {
                   OutputFormat::Json => {
                       let envelope = serde_json::json!({
                           "ok": false,
                           "error": {
                               "code": code,
                               "message": msg
                           }
                       });
                       println!("{}", serde_json::to_string(&envelope)?);
                   }
                   OutputFormat::Text => {
                       eprintln!("Error: {}", msg);
                   }
               }
               std::process::exit(4);
           }
       }
   }

   fn print_tree_node_text(node: &query::ModuleTreeNode, indent: usize) {
       let indent_str = "  ".repeat(indent);
       let suffix = if node.kind == "directory" { "/" } else { "" };
       let citation = if node.kind == "directory" {
           " [no_span_module_level]".to_string()
       } else if let Some(ref id) = node.record_id {
           let path = &node.path;
           format!(" ({}) @ {}", id, path)
       } else {
           format!(" @ {}", node.path)
       };
       println!("{}- {}{} {} ({} symbols)", indent_str, node.name, suffix, citation, node.symbol_count);
       for child in &node.children {
           print_tree_node_text(child, indent + 1);
       }
   }
   ```

---

## Verification Plan

### Automated Tests

We will write unit and integration tests in the following files:

#### [NEW] [tests/query_orient.rs](file:///c:/Users/markm/egregore/tests/query_orient.rs)
Integration tests for the new `eg query orient` CLI command covering:
- Happy path: correctly identifying entry points (`src/lib.rs`, `src/main.rs`, `src/bin/foo.rs`), displaying tree hierarchy, counting symbols, and ranking most-referenced symbols.
- Scoped repo: `--repo` restriction.
- `--format text` vs `--format json` output comparisons.
- Error path: empty graph returns exit code 3 and diagnostic `empty_graph`.
- Error path: graph with no entry points returns exit code 4 and diagnostic `no_entry_points`.
- Stable/deterministic ordering check over 5 consecutive runs.

To run:
`cargo test --test query_orient`

#### Unit Tests in `src/query.rs`

We will add a new test `test_orientation_map_happy_and_errors` to `src/query.rs`'s unit tests.
