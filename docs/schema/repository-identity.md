# Repository Node Identity

## Overview

A `Repository` node's stable ID is derived from its VCS remote URL, not its directory basename. This ensures that two clones of the same repository in different locations produce the same node ID in AletheiaDB, enabling cross-clone deduplication and bi-temporal merging.

## Identity Cases

Identity is determined by the first matching case, evaluated in priority order:

### 1. `remote` — VCS remote URL

**Condition**: `.git` exists and `git remote` lists at least one remote.

**Input**: The normalized URL of the lowest-name-sorted remote (alphabetically first remote name).

**ID formula**: `stable_id(["repository", "remote", <normalized_url>])`

**`identity_source`**: `"remote"`

**Payload fields set**: `remote_url`

### 2. `local_root_commit` — Root commit SHA

**Condition**: `.git` exists with at least one commit but no remotes.

**Input**: The SHA of the oldest first-parent ancestor of `HEAD` (`git rev-list --max-parents=0 HEAD`).

**ID formula**: `stable_id(["repository", "local-root-commit", <root_sha>])`

**`identity_source`**: `"local_root_commit"`

**Payload fields set**: `root_commit_sha`

### 3. `local_path` — Canonical absolute path

**Condition**: No `.git` directory, or `.git` exists but HEAD has no commits.

**Input**: `std::fs::canonicalize(repo_root)` (resolves symlinks).

**ID formula**: `stable_id(["repository", "local-path", <canonical_path>])`

**`identity_source`**: `"local_path"`

**Payload fields set**: `canonical_path`

### 4. `operator_override` — CLI escape hatch

**Condition**: `--repo-id-override <string>` passed to `scan` or `scan-history`.

**Input**: The provided override string.

**ID formula**: `stable_id(["repository", "operator-override", <override_string>])`

**`identity_source`**: `"operator_override"`

**Payload fields set**: none (all optional fields are `null`)

## URL Normalization

All remote URLs are normalized before hashing:

| Input form | Normalized form |
|---|---|
| `git@github.com:owner/repo.git` | `https://github.com/owner/repo` |
| `https://github.com/owner/repo.git` | `https://github.com/owner/repo` |
| `http://example.com/owner/repo` | `https://example.com/owner/repo` |
| `https://GITHUB.COM/owner/repo` | `https://github.com/owner/repo` |

Rules applied in order:
1. SSH `git@host:path` → `https://host/path`
2. `http://` → `https://`
3. Host portion lowercased
4. Trailing `.git` stripped

## JSONL Payload

Every `Repository` node carries a `repository_identity` object:

```json
{
  "record_type": "node",
  "kind": "Repository",
  "id": "codegraph:v2:<blake3hex>",
  "repository_identity": {
    "identity_source": "remote",
    "remote_url": "https://github.com/owner/repo",
    "root_commit_sha": null,
    "canonical_path": null,
    "basename": "repo"
  }
}
```

### `identity_source` values

| Value | Meaning |
|---|---|
| `"remote"` | Derived from VCS remote URL |
| `"local_root_commit"` | Derived from root commit SHA (no remote) |
| `"local_path"` | Derived from canonical absolute path (no git) |
| `"operator_override"` | Forced by `--repo-id-override` CLI flag |

## Schema Version

`SCHEMA_VERSION = 2`. All `stable_id` outputs use the prefix `codegraph:v2:`.

## Child Record Scope

`Commit` and `Change` node IDs are scoped to the repository ID:

- Commit: `stable_id(["node", "commit", <repository_id>, <commit_sha>])`
- Change: `stable_id(["node", "change", <repository_id>, <commit_sha>, <status>, <path>])`

## Shared-Store Considerations

The `inspect` CLI command prints the identity source alongside each repository ID:

```
repository: codegraph:v2:<hex> (remote: https://github.com/owner/repo)
```

For shared AletheiaDB stores (daemon mode), `local_path` identity is not recommended because absolute paths are machine-specific. Use `--repo-id-override` or a remote-backed clone instead.

## Related

- Issue: [#7 Derive Repository node identity from VCS remote](https://github.com/madmax983/egregore/issues/7)
- Daemon API: [daemon-api.md](daemon-api.md)
- Agent Memory: [agent-memory.md](agent-memory.md)
