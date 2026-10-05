# Project configuration — `egregore.toml` (issue #261)

Egregore's scan/ingest/query behavior used to live entirely in transient CLI
flags, so two operators scanning the same source could produce different
stores with no record of the configuration that produced them. A checked-in
`egregore.toml` pins the per-repo defaults — data directory, repository
identity, embedding model, scan scope, and redaction policy — so every agent,
machine, and CI run reproduces the same store from the same source without
re-specifying flags. The configuration itself becomes a version-controlled,
citable fact.

## Discovery

`scan`, `scan-history`, `scan-logs`, `ingest`, `refresh`, and `query`
auto-discover the file by **walking up from the working directory**: the
nearest `egregore.toml` in the current directory or any parent directory
wins. Absence is not an error — built-in defaults apply, exactly as before.

Daemon operational settings (host, port, write-queue capacity) are
deliberately *not* pinned here: those are runtime concerns, not per-repo
source of truth, and stay on their existing flags. No secrets or auth tokens
belong in this file either (see issue #70 for the credential mechanism).

## Fields

```toml
# egregore.toml — checked-in per-repo defaults for Egregore.

# Default --data-dir for ingest / refresh / query. A relative path resolves
# against the directory containing this file (the repository root), so
# `eg ingest` run from a subdirectory hits the same store as from the root.
data_dir = ".egregore"

# Default --repo-id-override: forces `identity_source = operator_override`
# for repository identity. Pin this (with scan.transaction_time below) for
# fixture-stable scans whose output is byte-identical across machines.
repo_id_override = "my-org/my-repo"

[embeddings]
# Identifier of the embedding model --embed runs produce
# (a Hugging Face model id). The resolved model is what the embedder loads AND
# what is recorded on the store's EmbeddingModel identity node (issue #104);
# changing it refuses to blend vector spaces. Precedence: --embed-model >
# [embeddings].model > built-in default.
model = "sentence-transformers/all-MiniLM-L6-v2"

[scan]
# Scan-scope include/exclude patterns. RESERVED for the issue #99 scoping
# engine: pinned here so the values are version-controlled and reported by
# `eg config show`, but not yet consumed by `eg scan`, which warns when they
# are set rather than silently ignoring them.
include = ["src/**"]
exclude = ["vendor/**", "third-party/**"]

# Fixed RFC 3339 transaction time stamping the scan. Fixture/CI use: two
# runs sharing the checked-in config produce byte-for-byte identical graph
# JSONL. Omit for the normal wall-clock behavior.
transaction_time = "2026-05-19T00:00:00Z"

[redaction]
# Which redaction policy applies (docs/schema/redaction.md). Only "v1" is
# known; any other value fails fast so a future policy can never apply
# silently.
policy = "v1"

# Mirror of --raw-literals: keep source-embedded secrets in raw form instead
# of redacting them. Default false.
raw_literals = false
```

Every key is optional; an empty file means "all built-in defaults". Unknown
keys or tables, wrong value types, an unknown `redaction.policy`, and an
unparsable `scan.transaction_time` all **fail fast**: the command exits
non-zero and prints a machine-readable diagnostic to stderr, e.g.

```json
{"code":"config_invalid","config_file":"/repo/egregore.toml","message":"unknown config key `mystery_option`: egregore.toml is strictly validated; fix the key or remove it (see docs/cli/config.md)"}
```

## Precedence

Explicit CLI flag **>** config file value **>** built-in default.

- `eg scan --repo-id-override X` beats `repo_id_override` in the file, which
  beats auto-detection.
- `eg ingest --data-dir Y` beats `data_dir` in the file, which beats
  `.egregore`.
- `--embed-model` beats `[embeddings] model`, which beats the built-in default.
  The resolved model is what the embedder loads and what the store's
  vector-index identity records, so a config pin is honored and described
  honestly.
- `--raw-literals` (a boolean flag) beats `[redaction] raw_literals`, which
  beats `false`.
- `eg query --graph graph.jsonl` beats a config-pinned `data_dir`: the config
  store applies to query lanes only when neither `--graph` nor `--data-dir`
  was passed, so an explicit file read is never shadowed — and never misread
  as "both provided".

Boolean flags compose with OR: passing `--raw-literals` always wins; otherwise
the config value applies.

## Reporting the effective configuration

`eg config show` prints the resolved configuration as JSON — the discovered
file (or `null`), every value, and where each value came from (`"config"` or
`"default"`):

```powershell
eg config show
# {
#   "config_file": "/repo/egregore.toml",
#   "data_dir": {"value": "/repo/.egregore", "source": "config"},
#   "repo_id_override": {"value": "my-org/my-repo", "source": "config"},
#   "embeddings": {"model": {"value": "sentence-transformers/all-MiniLM-L6-v2", "source": "default"}},
#   ...
# }
```

An agent or CI step can run this before (or after) a scan to verify — and cite
— exactly what governed the run. Source is determined by key presence, not
value shape: an explicitly set-but-empty array such as `include = []`
reports `"source": "config"`, while an absent key reports `"default"`.

## Reproducible scans

For byte-for-byte reproducible graph JSONL across environments, pin the two
inputs that otherwise vary per machine:

```toml
repo_id_override = "fixture-stable-repo"

[scan]
transaction_time = "2026-05-19T00:00:00Z"
```

With those pinned, `eg scan <repo> --out graph.jsonl` run from any working
directory under the config (no per-invocation flags) yields identical JSONL
on a developer machine and in CI. The pinned instant governs the whole scan —
transaction time, the coverage-generation stamp, and `producer_started_at` —
so same-second re-runs stay orderable and identical.
