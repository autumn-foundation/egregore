# eg protected

Capture and retrieve **protected raw artifact payloads** — transcripts, command output, patch
bytes, task narratives, and generated reports — in a local, content-addressed store that the
graph, query, and semantic surfaces never read.

By default Egregore stores only content hashes and handles for raw payloads.  This command gives
operators an opt-in workflow to also retain the original bytes so that evidence handles remain
resolvable even after source files move or are deleted.

> **Relates to:** Issue #41 (redaction) and issue #54 (encrypted stores).
> See [When to use protected mode](#when-to-use-protected-mode-vs-pathhash-only-provenance).

## Synopsis

```text
eg protected capture --manifest <PATH> --store <DIR> [--protected-raw-artifacts] [--producer <ID>] [--captured-at <RFC3339>]
eg protected get     <HANDLE> --store <DIR> --operator <ID> [--out <PATH>]
eg protected list    --store <DIR>
```

## Store layout

```text
<store>/blobs/<content_hash>   — raw bytes, content-addressed by BLAKE3 hex
<store>/manifest.jsonl         — canonical-sorted ProtectedHandle records
```

Authorization is derived from the manifest: an `--operator` may retrieve payloads iff it is the
`--producer` of at least one captured record.  There is no separate ACL file (authorization is
crash-atomic with the manifest write).

The protected store is a **separate directory** from `.egregore`.  The graph extractor, ingest,
query, inspect, and semantic-search commands never read or write the store.  This structural
separation is the primary guarantee that default query surfaces remain raw-payload-free.

## Subcommands

### `eg protected capture`

Reads a manifest JSONL (one `{"class":"…","source_path":"…"}` per line) and either previews or
captures the listed payloads.

**Supported payload classes:**

| Class | Examples |
|-------|---------|
| `transcript` | Claude Code session, Codex rollout, traj file body |
| `command_output` | `cargo test` stdout, CI log |
| `patch` | Unified diff bytes |
| `task_narrative` | Issue description, PR body, local task file |
| `report` | Scan summary, eval result, analysis document |
| `log_payload` | Post-redaction raw log bytes captured by `eg scan-logs` (issue #321) — **produced only by `eg scan-logs --protected-raw-artifacts`; rejected in a generic `protected capture` manifest** (see below) |

**Flags:**

| Flag | Required | Description |
|------|----------|-------------|
| `--manifest <PATH>` | yes | Path to the capture manifest JSONL |
| `--store <DIR>` | yes | Protected store directory |
| `--protected-raw-artifacts` | no | Enable actual capture; without this flag, preview only |
| `--producer <ID>` | when enabled | Stable operator identity recorded as authorised |
| `--captured-at <RFC3339>` | no | Override capture timestamp (for deterministic tests) |

**Exit codes:**

| Code | Meaning |
|------|---------|
| `0` | Capture or preview complete (per-entry problems are diagnostics, not failures) |
| `1` | Manifest file I/O or parse error, or `--producer` missing in enabled mode |

**Disabled mode (default):**

Without `--protected-raw-artifacts`, the command is a preview run.  It reads each source file,
computes the BLAKE3 content hash and byte length, and prints a JSON report — but writes
**nothing** to disk.  The store directory is never created.  This is safe to run on CI or in
read-only environments.

**Enabled mode:**

With `--protected-raw-artifacts`, the command stores each readable source file as a blob and
registers a `ProtectedHandle` record in `manifest.jsonl`.  The handle identity covers the
payload class, content hash, and source path (not the capture timestamp), so re-capturing the
same unchanged source yields the same handle and produces zero duplicate manifest entries.

Per-entry problems produce diagnostics and do not abort capture:

| Diagnostic code | Meaning |
|----------------|---------|
| `unsupported_payload_class` | The `class` field is not one of the recognised classes |
| `stale_source_path` | The source file is not readable (moved or deleted before capture) |
| `log_payload_requires_scan_logs` | The entry declares `class: "log_payload"`. That class is the log's **post-redaction** bytes and is produced only by `eg scan-logs --protected-raw-artifacts` (which redacts before capture). The generic manifest capture path reads `source_path` straight from disk with no redaction, so it refuses the entry — no blob and no manifest record are written for it, and any other valid entries in the same manifest still store atomically. |

**Example:**

```sh
# Preview (disabled) — hashes + handles only, nothing stored
eg protected capture --manifest evidence.jsonl --store .egregore/protected

# Enabled — store all five fixture payloads
eg protected capture \
  --manifest tests/fixtures/protected/capture.jsonl \
  --store .egregore/protected \
  --protected-raw-artifacts \
  --producer "operator-alice"
```

### `eg protected get`

Retrieves the raw bytes for a protected handle, verifying the BLAKE3 content hash before
returning any bytes.  Writes to `--out` or to stdout.

**Check order (auth before existence disclosure):**

1. Store / manifest absent → `raw_artifact_mode_disabled` (exit 1)
2. Operator not in authorised set → `unauthorized` (exit 1)
3. Handle does not start with `protected:v1:` → `malformed_handle` (exit 1)
4. Handle not in manifest → `payload_not_found` (exit 2)
5. Blob file absent → `missing_protected_payload` (exit 1)
6. BLAKE3 mismatch → `hash_mismatch` (exit 1)
7. Else → raw bytes written, exit 0

**Exit codes:**

| Code | Meaning |
|------|---------|
| `0` | Bytes written (hash verified) |
| `1` | Auth failure / malformed handle / blob missing / hash mismatch |
| `2` | Handle not found in manifest |

**Example:**

```sh
eg protected get protected:v1:<hex> \
  --store .egregore/protected \
  --operator "operator-alice" \
  --out retrieved.txt
```

### `eg protected list`

Lists all protected handles as metadata-only JSON.  Raw bytes are never included in list output.

**Exit codes:**

| Code | Meaning |
|------|---------|
| `0` | List emitted (may be empty when store is not yet initialised) |
| `1` | Manifest I/O or parse error |

**Example:**

```sh
eg protected list --store .egregore/protected
```

**Response shape:**

```json
{
  "ok": true,
  "count": 5,
  "handles": [
    {
      "handle": "protected:v1:<hex>",
      "schema_version": 1,
      "source_class": "transcript",
      "source_path": "tests/fixtures/protected/transcript.txt",
      "content_hash": "<blake3 hex>",
      "byte_len": 312,
      "captured_at": "2026-06-18T00:00:00Z",
      "producer_id": "operator-alice",
      "producer_version": "0.1.0"
    }
  ]
}
```

## Capturing raw log payloads from `eg scan-logs` (issue #321)

`eg scan-logs` can capture the scanned log's raw bytes into the protected store in the same run,
so a runtime-observation graph (issues #319 / #320) keeps a durable, retrievable copy of the log
even after the original file is rotated or deleted.  It is **disabled by default** and reuses this
store, its manifest, its authorization model, and the frozen handle identity unchanged.

```sh
# Scan a log AND capture its post-redaction bytes as a log_payload blob
eg scan-logs app.log \
  --repo-path . \
  --out log.graph.jsonl \
  --protected-raw-artifacts \
  --protected-store .egregore/protected \
  --producer "$(git config user.email)"

# Retrieve the captured log later by handle (hash verified before bytes return)
eg protected get protected:v1:<hex> \
  --store .egregore/protected \
  --operator "$(git config user.email)" \
  --out recovered.log
```

Flags (all under `eg scan-logs`; see [`docs/cli/scan-logs.md`](scan-logs.md)):

| Flag | Required | Description |
|------|----------|-------------|
| `--protected-raw-artifacts` | no | Enable capture; without it no blob and no manifest entry are written |
| `--protected-store <DIR>` | when enabled | Protected store directory |
| `--producer <ID>` | when enabled | Stable operator identity recorded as authorised for the log blob |
| `--captured-at <RFC3339>` | no | Override the blob capture timestamp (deterministic manifests); not part of handle identity |

On success, `scan-logs` prints a single JSON line reporting the capture handle, content hash,
byte count, and class — never raw bytes.  A capture I/O failure prints a `store_io_error`
envelope to stderr and exits `3`, leaving no partial manifest (single atomic manifest commit).

> **Honest limit — post-redaction bytes only.**  The stored `log_payload` blob is the log with
> the v1 redaction policy already applied line-by-line: a secret-bearing line is stored as its
> `<REDACTED:…>` marker.  Egregore **never persists the unredacted original**.  The blob's
> `content_hash` (BLAKE3 over the post-redaction bytes) is deliberately independent of the
> `LogSource.source_artifact_hash` in the graph (BLAKE3 over the unredacted, newline-normalized
> bytes); the two are never conflated and the graph never stores the protected handle.

## When to use protected mode vs path/hash-only provenance

| Use | When |
|-----|------|
| **Path + hash only (default)** | Sources are stable (checked-in files, CI artifacts with known retention), you only need to cite the evidence — not retrieve it raw. |
| **Protected mode** | Sources are ephemeral (local transcript files, temp command output, patches that may be cleaned up), you need a durable local copy that survives the source being deleted or moved. |

Protected mode is **not** a substitute for:
- **Redaction** (issue #41) — run the redaction gate *before* capturing; protected mode does not
  re-apply redaction to stored bytes.
- **Encrypted stores** (issue #54) — blobs are stored as plain bytes.  Operators who need
  encryption should layer a filesystem-level solution; issue #54 owns the Egregore encrypted-store
  workflow.

## Shortest documented operator workflow

```sh
# 1. Write a capture manifest (one entry per payload)
cat > evidence.jsonl <<EOF
{"class":"transcript","source_path":"${HOME}/.claude/projects/foo/session.jsonl"}
{"class":"command_output","source_path":"/tmp/test-run-2026-06-18.log"}
{"class":"patch","source_path":"feature.patch"}
{"class":"task_narrative","source_path":".egregore/tasks/issue-60.md"}
{"class":"report","source_path":"scan-report.md"}
EOF

# 2. Preview — verify hashes, nothing stored
eg protected capture --manifest evidence.jsonl --store .egregore/protected

# 3. Capture — store blobs
eg protected capture \
  --manifest evidence.jsonl \
  --store .egregore/protected \
  --protected-raw-artifacts \
  --producer "$(git config user.email)"

# 4. Move / delete source files (simulating evidence rot)
rm /tmp/test-run-2026-06-18.log

# 5. Retrieve by handle — hash verified
eg protected get protected:v1:<hex> \
  --store .egregore/protected \
  --operator "$(git config user.email)" \
  --out retrieved-output.log

# 6. Verify default query surfaces are still raw-payload-free
eg scan . --out graph.jsonl
# graph.jsonl contains no raw payload byte strings
eg protected list --store .egregore/protected
# list output contains only metadata (no inline bytes)
```

## Verifying default query surfaces remain raw-payload-free

1. Run `eg protected capture` with mode enabled to populate the store.
2. Run `eg scan . --out graph.jsonl` (or any other default export).
3. Assert that raw payload content does not appear in `graph.jsonl`:

```sh
# These should produce no output:
grep -F "$(head -1 transcript.txt)" graph.jsonl
grep -F "test result: ok" graph.jsonl
```

4. Run `eg protected list --store .egregore/protected` and confirm the output contains only
   metadata fields (`handle`, `source_class`, `content_hash`, `byte_len`, `captured_at`,
   `producer_id`) with no inline bytes.

## Diagnostic codes

All error envelopes are written to stderr as `{"ok":false,"error":{"code":"…","detail":{…}}}`.
No error ever echoes raw payload bytes, bearer tokens, patch hunks, secrets, or command output.

| Code | Exit | Meaning |
|------|------|---------|
| `raw_artifact_mode_disabled` | 1 | Store or manifest does not exist; capture was not enabled |
| `unauthorized` | 1 | Operator ID is not in the store's authorised set |
| `malformed_handle` | 1 | Handle does not start with `protected:v1:` |
| `payload_not_found` | 2 | Handle is valid but not present in the manifest |
| `missing_protected_payload` | 1 | Handle is in the manifest but the blob file is absent |
| `hash_mismatch` | 1 | Stored blob bytes do not match the recorded BLAKE3 content hash |
| `unsupported_payload_class` | n/a | Capture-manifest entry `class` is unrecognised (per-entry diagnostic, not a fatal error) |
| `stale_source_path` | n/a | Source file is not readable at capture time (per-entry diagnostic) |
| `log_payload_requires_scan_logs` | n/a | Capture-manifest entry declares `class: "log_payload"`, which the generic (no-redaction) capture path refuses; produce it via `eg scan-logs --protected-raw-artifacts` instead (per-entry diagnostic) |

## Scope

This slice consumes existing artifact, redaction, transcript-import, project, verification, and
daemon/query contracts.  It introduces no new trust model, schema domain, embedding workflow,
hosted sync, remote repository crawler, or mandatory remote storage service (issue #60, AC9).
