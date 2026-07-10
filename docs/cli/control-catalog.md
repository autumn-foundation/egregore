# `eg audit control-catalog`

Loads, validates, and hash-pins a SOC2 control→evidence-class catalog (issue
#337). Pure, offline, read-only: it parses one catalog document, checks its
schema-version tuple and evidence-class vocabulary, and prints a deterministic
report carrying the catalog identity, its BLAKE3 hash-pin handle, and the
per-control evidence-class map.

This command is the catalog loader/validator/pin surface only. It does **not**
assemble an evidence pack — that is issue #338, which consumes this catalog and
its hash-pin.

See `docs/controls/README.md` for the catalog document format, the closed
11-value evidence-class vocabulary, the versioning contract, and the epistemic
boundary.

## Usage

```powershell
# Validate and pin the embedded default SOC2 catalog
cargo run -- audit control-catalog

# Validate and pin a catalog file
cargo run -- audit control-catalog --catalog path/to/soc2.json

# Human-readable summary
cargo run -- audit control-catalog --format text
```

## Flags

- `--catalog <path>` — catalog document to load. Defaults to the embedded
  `soc2-v1` catalog (`docs/controls/soc2-v1.json`).
- `--format <json|text>` — output format. Defaults to `json`.

## Exit codes

- `0` — the catalog is valid; the report is printed to stdout.
- `2` — a read/parse error, an unknown evidence class, or an unknown schema
  version; a redaction-safe JSON error is printed to stderr.

## JSON output

The default JSON output is a **single deterministic line**, byte-identical
across repeated runs on the same catalog:

```json
{
  "ok": true,
  "catalog_id": "soc2-v1",
  "catalog_schema_version": { "domain": "control_catalog", "kind": "ControlCatalog", "version": 1 },
  "catalog_hash": "control_catalog:v1:<hex>",
  "control_count": 3,
  "controls": [
    {
      "control_id": "CC8.1",
      "title": "…",
      "evidence_classes": [
        { "class": "commits", "requirement": "required" }
      ]
    }
  ]
}
```

## Error envelopes (stderr, exit 2)

- Missing/unreadable `--catalog` file:
  `{ "code": "catalog_read_error", "path": "…", "message": "…" }`
- Malformed JSON: `{ "code": "malformed_json", "message": "…" }`
- Unknown schema version:
  `{ "code": "unknown_schema_version", "version": { "domain": "…", "kind": "…", "version": 2 } }`
- Unknown evidence class:
  `{ "code": "unknown_evidence_class", "control_id": "…", "class": "…" }`
- Invalid requirement:
  `{ "code": "invalid_requirement", "control_id": "…", "class": "…", "requirement": "…" }`
