# `eg schema` — Publish JSON Schemas for the Persisted Record Contracts

**Issue:** #226 — _publish JSON Schemas for the persisted graph record contract_

---

## Overview

`eg schema export` emits JSON Schema (draft 2020-12) documents for every
persisted `(domain, kind, schema_version)` record contract, derived from the
Rust types in `src/ir.rs` — never from a disconnected static schema. The
contracts cover all eight persisted domains (`codegraph`, `agent_memory`,
`verification`, `artifact`, `project`, `semantic`, `user_context`, `log`),
including log records, plus historical versions still accepted by current
readers (codegraph `v1`–`v10`, log `v2`–`v3`).

```
eg schema export --domain codegraph --kind Symbol --schema-version 10
eg schema export --out ./schemas
eg schema list
```

Every emitted document:

- declares `$schema: https://json-schema.org/draft/2020-12/schema`,
- carries a stable `$id` under
  `https://github.com/madmax983/egregore/schemas/<domain>/<record_type>/<kind>/v<version>`,
- pins `kind`/`label`, `schema_version`, and the record `id` prefix to the
  addressed tuple,
- requires the base record fields plus the kind's provenance fields, and the
  producer envelope — required at the domain's current version when that
  version postdates the envelope's introduction, optional on legacy versions
  and on current versions that predate the envelope (e.g. project `v1`,
  verification `v1`: their v1 records predate the envelope, so per the
  legacy-record policy they stay producer-less — see
  `docs/schema/producer-version.md` §6),
- forbids subjective/agent-authored fields on deterministic code facts
  (`text`, `agent_id`, `confidence`, `evidence_links`, …) — trust separation,
  `docs/schema/producer-version.md` §10,
- requires agent provenance on agent-authored records
  (`docs/schema/agent-memory.md` §3).

## Subcommands

### `eg schema export`

With no filters, prints every addressable contract as a JSON array. With
filters that match exactly one contract, prints the single document. With
`--out <dir>`, writes one
`<domain>.<record_type>.<kind>.v<version>.schema.json` file per contract and
prints a JSON summary (`{ok, count, out, files}`) instead of the documents.

| Flag | Meaning |
| --- | --- |
| `--domain <d>` | Restrict to one domain, e.g. `codegraph`. |
| `--kind <k>` | Restrict to one node kind or edge label, e.g. `Symbol`, `CALLS`. `"Tombstone"` selects tombstone contracts. |
| `--schema-version <v>` | Restrict to one schema version. |
| `--out <dir>` | Write one file per contract into `<dir>` instead of stdout. |

Unknown filter values fail with exit code 2 and a machine-readable
`{"ok": false, "code": "schema_unknown_contract", ...}` diagnostic on stderr.

### `eg schema list`

Prints every addressable `(domain, kind, schema_version)` contract as a JSON
array of `{domain, record_type, kind, schema_version, $id, file}` entries.
Accepts `--domain` to restrict.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Schemas emitted. |
| 1 | I/O error writing `--out` files. |
| 2 | Unknown domain/kind/version tuple (JSON error on stderr). |

## See also

- `docs/schema/agent-memory.md`, `docs/schema/verification.md`, and the other
  contract documents under `docs/schema/` that the schemas encode.
- `docs/schema/schema-versioning.md` — which versions are published and why.
- `docs/schema/producer-version.md` — the producer envelope and trust
  separation.
