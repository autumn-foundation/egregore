# Egregore Query Patterns

Recipes for common agent and developer tasks using egregore's structural and semantic query surfaces.

## Impact Analysis — "What breaks if I change X?"

Before refactoring a function or changing a public API:

```powershell
# 1. Find the symbol's stable record ID and location
egregore query symbol daemon::handle_query --data-dir .egregore --format json

# 2. Grep the JSONL for CALLS edges that target this symbol
#    (the graph captures direct callers)
grep '"label":"CALLS"' graph.jsonl | grep "daemon::handle_query" \
  | grep -o '"summary":"[^"]*"'

# 3. Check what the function itself calls (its dependency surface)
grep '"summary":"daemon::handle_query calls' graph.jsonl \
  | grep -o '"summary":"[^"]*"'

# 4. Check what types/constants it references
grep '"summary":"daemon::handle_query mentions' graph.jsonl \
  | grep -o '"summary":"[^"]*"'
```

Reading results:
- **Callers** = scope of breakage if the signature changes
- **Callees** = what your refactor must preserve behavior for
- **Mentions** = constants, types, and error constructors your implementation depends on

---

## Refactoring Safety Check

Before starting a refactor, establish the full contact surface:

```powershell
# Everything that touches the target module
grep '"src/daemon.rs"' graph.jsonl | grep '"label":"IMPORTS"' \
  | grep -o '"summary":"[^"]*"'

# All symbols the module defines (the public API)
egregore query file src/daemon.rs --data-dir .egregore --format text

# Semantic neighbors (related concepts that might need updating too)
egregore query semantic "daemon request handling" \
  --data-dir .egregore-semantic --format text --limit 15
```

Combine structural (exact imports/callers) with semantic (related concepts) to avoid missing implicit dependencies.

---

## Semantic Discovery — "Where is X handled?"

When exploring an unfamiliar codebase without knowing the symbol names:

```powershell
# Find rate limiting / throttling code
egregore query semantic "rate limiting request throttling" \
  --data-dir .egregore-semantic --format text

# Find error recovery / retry logic
egregore query semantic "retry backoff error recovery" \
  --data-dir .egregore-semantic --format text

# Find authentication / authorization
egregore query semantic "authentication token verification" \
  --data-dir .egregore-semantic --format text

# Find database write path
egregore query semantic "persist write commit transaction" \
  --data-dir .egregore-semantic --format text
```

Once you have candidate symbol names from semantic search, use `query symbol` for exact location and `query file` to see their siblings.

---

## Architecture Exploration — "How is this codebase structured?"

```powershell
# See all files and their top-level module structure
egregore query file src/lib.rs --data-dir .egregore --format text

# Find all modules (broad sweep)
grep '"kind":"Module"' graph.jsonl | grep -o '"name":"[^"]*"' | sort -u

# Find all public trait definitions
grep '"symbol_kind":"Trait"' graph.jsonl | grep -o '"summary":"[^"]*"' | head -20

# Find all error types
egregore query semantic "error type definition" \
  --data-dir .egregore-semantic --format text --limit 10
```

---

## Cross-Commit Comparison — "How did X change?"

Requires a history graph (`scan-history`) and a store ingested from it.

```powershell
# What did this symbol look like 5 commits ago?
egregore query symbol daemon::handle_query \
  --data-dir .egregore --at HEAD~5

# What does it look like now?
egregore query symbol daemon::handle_query \
  --data-dir .egregore --at HEAD

# Which symbols drifted most semantically across the history?
egregore query drift --data-dir .egregore --limit 20 --format text
```

Drift score interpretation:
- **> 0.4**: Major semantic change — logic or responsibility likely shifted
- **0.2–0.4**: Moderate change — worth reviewing what changed
- **< 0.2**: Minor change — likely renaming, formatting, or small additions

---

## Token-Efficient Agent Context

Rather than loading whole files, build targeted context from the graph:

```powershell
# Get the location of the function you care about
egregore query symbol my::module::target_fn --data-dir .egregore --format json
# → { "repo_relative_path": "src/module.rs", "span": { "start_line": 142, "end_line": 178 } }

# Read only those lines
# (pass start_line/end_line to Read tool with offset/limit)
```

This pattern gives an agent precise, bounded context (36 lines) instead of an entire module (400+ lines). Combine with the callers list from impact analysis to build a minimal but complete picture of a change.

---

## Keeping the Index Fresh

Egregore is snapshot-based — re-run scan + ingest when code changes significantly.

Suggested triggers:
- After a branch merge or large PR
- Before starting a non-trivial refactor
- After any change to a module's public API

```powershell
# Quick refresh (structural only, ~seconds)
egregore scan . --out graph.jsonl
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore

# Full refresh with semantic (downloads model from cache, ~minutes for large repos)
egregore ingest graph.jsonl --adapter embedded --data-dir .egregore-semantic --embed
```

For CI integration, run `scan` + `ingest` (structural only) on every PR and store the result as an artifact. Semantic ingest on merge to trunk.

---

## Gitignore

Keep the stores out of version control:

```gitignore
.egregore/
.egregore-semantic/
graph.jsonl
history.graph.jsonl
```

The JSONL files are fully reproducible from the repo; the stores are derived from the JSONL.
