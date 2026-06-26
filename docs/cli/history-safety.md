# Git History and Scan Safety Guarantees

Egregore guarantees that scanning operations (`eg scan` and `eg scan-history`) are strictly read-only and safe to run on active, dirty working trees. They will never mutate the repository's Git state or modify files in the working directory.

## Guarantees

- **No checkout mutations**: History replay reads Git tree objects directly via plumbing commands (e.g. `git show <sha>:<path>`, `git ls-tree`, `git diff-tree`). It does not use checkout/reset/stash workflows to materialize versions, meaning it never touches your active checkout.
- **Index protection**: All git commands executed during scanning run with `GIT_OPTIONAL_LOCKS=0`, which prevents Git from writing or refreshing the index file, avoiding lock-file contention and index updates.
- **Detached HEAD compatibility**: Both commands are fully compatible with detached HEAD states, staged changes, unstaged edits, untracked files, and stashed work. 
- **Deterministic Committed History**: Running `eg scan-history` on a repository with uncommitted work yields the exact same committed history graph output as running it on a clean tree at the same commit.

## Covered Commands

- `eg scan <repo_path> --out <file>`
- `eg scan-history <repo_path> --out <file>`

Both commands are fully offline and run with no network access.

## Out of Scope

- **Ingestion and Query paths**: Commands like `eg ingest` and `eg query` are out of scope for these git repo guarantees. They write metadata, indexes, and nodes to an explicitly supplied `--data-dir` external to the tracked Git state.
- **Incremental Cache**: `eg refresh` reads working-tree edits as input and caches results under `--data-dir`, but does not mutate the git repository itself.
