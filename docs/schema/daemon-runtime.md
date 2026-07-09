# `egregored` Runtime Directory Contract

**Schema version:** 1 (`DAEMON_RUNTIME_SCHEMA_VERSION`)
**Status:** Frozen for v1. Breaking changes require `schema_version: 2`.

This document is the source of truth for how local clients find `egregored`
before they use the HTTP wire contract in [`daemon-api.md`](daemon-api.md).
The wire spec owns request and response envelopes. This file owns the runtime
sidecar directory, `egregored.json`, file permissions, stale-file checks, and
working-directory discovery.

## 1. Runtime Directory Path

For a data directory `D`, the runtime directory is:

```text
parent(D) / (basename(D) + ".egregore-runtime")
```

The runtime directory is adjacent to the data directory. It is not nested
inside the data directory.

Examples:

| Data dir | Runtime dir |
|---|---|
| `.egregore` | `.egregore.egregore-runtime` |
| `/abs/path/store` | `/abs/path/store.egregore-runtime` |
| `./relative/dir` | `./relative/dir.egregore-runtime` |

Rationale: AletheiaDB owns the data directory contents. Egregore daemon control
files MUST NOT collide with database files.

## 2. Runtime File Set

The v1 runtime directory file set is closed:

| File | Writer | Reader | Format | Lifecycle | Permissions |
|---|---|---|---|---|---|
| `egregored.lock` | Daemon startup | Clients never read content | Advisory lock file | Held for daemon lifetime; released by OS on process death; may linger | POSIX `0600`; Windows: current-user + SYSTEM full control, no inheritance, no broad-group access (enforced via Windows ACL) |
| `egregored.json` | Daemon startup and graceful shutdown | Clients on connect | JSON schema in section 3 | Rewritten on every start; rewritten with `state: "stopped"` on graceful shutdown; left in place | POSIX `0600`; Windows: current-user + SYSTEM full control, no inheritance, no broad-group access (enforced via Windows ACL) |
| `idempotency.json` | Daemon only | Daemon only | Internal JSON journal | Created on startup; may change without a schema bump because it is not client contract | POSIX `0600`; Windows: current-user + SYSTEM full control, no inheritance, no broad-group access (enforced via Windows ACL) |

Any future file, such as structured logs, is an additive contract update in
this document.

Additive note (issue #200): the embedded adapter acquires the same
`egregored.lock` exclusive advisory lock for the lifetime of every embedded
write, so a data directory has exactly one live writer — daemon or embedded —
at any moment. A writer that loses the race is refused with the structured
`store_contended` error documented in
[`docs/cli/embedded-concurrency.md`](../cli/embedded-concurrency.md).

## 3. `egregored.json` Schema

Required fields:

| Field | Type | Notes |
|---|---|---|
| `schema_version` | integer | Must be `1` for this contract |
| `pid` | u32 | Daemon process ID |
| `address` | string | HTTP `host:port` for v1 |
| `token` | string | Opaque bearer token; clients must not parse it |
| `data_dir` | string | Canonical absolute data directory path |
| `version` | string | Daemon binary version |
| `started_at_unix_ms` | u128 number or string | Unix epoch milliseconds |
| `state` | enum | `"running"`, `"stopped"`, or `"crashed"` |

Reserved nullable fields:

| Field | Type | v1 value | Future use |
|---|---|---|---|
| `api_version` | string | `null` | Wire API value such as `v1` or `v2` |
| `transports` | array | `null` | `{ "kind": "http"|"unix"|"pipe", "address": string }` |
| `token_expires_at_unix_ms` | u128 number or string | `null` | Token rotation expiry |
| `daemons_index_url` | string | `null` | Multi-daemon registry pointer |

Adding an optional field is additive within `schema_version: 1`. Renaming,
removing, or changing the semantics of an existing field requires
`schema_version: 2`.

## 4. Stale-File Detection

A client that reads `egregored.json` MUST verify liveness before trusting
`address` or `token`:

1. Open `egregored.lock` and attempt a shared advisory lock.
2. If the shared lock succeeds, no daemon holds the exclusive lease. The
   metadata is stale. The client SHOULD treat the daemon as down and MAY rewrite
   `state` to `"crashed"`.
3. If the shared lock fails because another process holds it, the daemon is
   alive. The client MAY then probe `GET /v1/health`.

On macOS and Linux this lock check is authoritative because the kernel releases
the lock on process death. On Windows it is the strong liveness signal.

## 5. Discovery Walk

Clients with a working directory but no data directory use these precedences:

1. `EGREGORE_DATA_DIR`, if set, names the data directory directly. Derive the
   runtime directory using section 1 and stop.
2. Otherwise walk upward from the working directory to the filesystem root,
   without crossing filesystem mount points. At each directory, try:
   - that directory as a data directory, using the section 1 runtime path
   - that directory's `.egregore` child as a data directory

The first runtime directory that exists wins. If none exists, the client MUST
report `no daemon for this directory` and include the paths it tried. It MUST
NOT silently fall back to embedded mutation.

## 6. Permissions and Startup

### POSIX

- runtime directory: `0700`
- `egregored.lock`: `0600`
- `egregored.json`: `0600`
- `idempotency.json`: `0600`

### Windows ACL contract

Every runtime file and the runtime directory must grant full control only to
the **current user** (identified by SID, not username) and **SYSTEM**
(`S-1-5-18`).  Inheritance is disabled.  No broad-group SIDs may hold any
Allow entry:

| Blocked SID | Group |
|---|---|
| `S-1-1-0` | Everyone |
| `S-1-5-32-545` | BUILTIN\\Users |
| `S-1-5-11` | NT AUTHORITY\\Authenticated Users |
| `S-1-5-32-546` | BUILTIN\\Guests |

The daemon MUST refuse startup with `runtime_permissions_unsafe` if it cannot
enforce these ACLs.  Before reading `egregored.json`, the daemon client also
verifies the ACL is safe; if a broad-group SID is found, the client returns
`runtime_permissions_unsafe` without reading or forwarding the bearer token.

#### Diagnosing unsafe permissions on Windows

```
icacls <runtime-dir>
icacls <runtime-dir>\egregored.json
icacls <runtime-dir>\egregored.lock
```

A safe ACL shows only the current user and SYSTEM.  If broad groups appear,
delete the runtime directory and let the daemon recreate it, or use
`icacls <path> /reset` followed by a fresh `eg daemon start`.

Startup order:

1. acquire the exclusive lock
2. create or repair runtime directory permissions
3. bind the socket enough to know the final v1 address
4. write `egregored.json` with restricted permissions
5. create the daemon-private idempotency journal
6. begin accepting requests

Failing any step before serving rolls back partial metadata when possible.

## 7. Shutdown Lifecycle

On `eg daemon stop` or `POST /v1/admin/shutdown`, the daemon:

1. stops accepting new work
2. drains accepted writes
3. checkpoints/persists store state through the existing sink path
4. rewrites `egregored.json` with `state: "stopped"`
5. releases the lock
6. removes nothing

On crash, `egregored.json` remains as last written, usually
`state: "running"`. Clients use the stale-file rule to distinguish a live
running daemon from a crashed one. A future `eg daemon stop --purge` may remove
the runtime files.

## 8. Reserved Hooks

Token rotation is reserved but not implemented. When
`token_expires_at_unix_ms` is populated, clients MUST re-read
`egregored.json` after that instant. The reserved wire error code is
`token_rotated`.

The multi-daemon registry is reserved but not implemented. `daemons_index_url`
may later point to a per-user daemon index such as
`$XDG_RUNTIME_DIR/egregore/daemons.json` on POSIX or
`%LOCALAPPDATA%\egregore\daemons.json` on Windows. V1 clients that target
multiple repositories must receive explicit data directories.

## 9. Client Pseudocode

```text
fn connect(start_dir):
  runtime_dir = discover_runtime_dir(start_dir)
  metadata_path = runtime_dir / "egregored.json"
  lock_path = runtime_dir / "egregored.lock"

  metadata = json_read(metadata_path)
  assert metadata.schema_version == 1

  lock = open(lock_path)
  if try_shared_lock(lock):
    unlock(lock)
    error("daemon metadata is stale")

  address = metadata.address
  token = metadata.token
  response = http_get("http://" + address + "/v1/health")
  assert response.status == 200
  return Client(address, token)
```

This example uses only fields documented in this file and the unauthenticated
health route documented in [`daemon-api.md`](daemon-api.md).
