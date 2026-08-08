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
- `2` — a read/parse error, an unknown evidence class, an unknown schema
  version, an invalid requirement, a duplicate control ID, or a duplicate
  evidence class within a control; a redaction-safe JSON error is printed to
  stderr.

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

The parser gates the `schema_version` tuple **first**: any document whose tuple
is not the supported `(control_catalog, ControlCatalog, 1)` is reported as
`unknown_schema_version` before the strict v1 body shape is enforced, so an
unsupported/newer catalog that also adds or renames fields is still reported as
`unknown_schema_version` (not `malformed_json`). The declared `version` is
probed as a raw JSON number, so a well-formed but unrepresentable version
(`1.5`, `-1`, `4294967296`) also reports `unknown_schema_version` echoing the
number as declared, never `malformed_json`. Only supported-version documents
are then held to the strict v1 shape, where an unknown/stray field is
`malformed_json`.

Input normalization mirrors the hash contract: a leading UTF-8 BOM (Windows
PowerShell 5.1 `Out-File` writes one by default) and CRLF line endings are both
stripped before parsing, so neither fails the parse nor perturbs the
`control_catalog:v1:` pin.

Every catalog-sourced string an envelope echoes (`control_id`, `class`,
`requirement`, schema `domain`/`kind`) is control-character-sanitized and
length-capped (`CATALOG_FIELD_MAX_CHARS`, truncation marked with `…`) before
rendering — the values come from the document under validation and are
operator/attacker-controlled. `--format text` likewise neutralizes control
characters in `catalog_id`, `control_id`, and `title`, so a crafted catalog
cannot drive the operator's terminal.

- Missing/unreadable `--catalog` file:
  `{ "code": "catalog_read_error", "path": "…", "message": "…" }`
- Malformed JSON (structurally broken, missing `schema_version`, a wrong-type
  field, or a stray field in a supported-version document):
  `{ "code": "malformed_json", "line": <n>, "column": <n>, "category": "syntax|data|eof|io" }`.
  The envelope carries only a stable code plus the value-free failure location
  (1-based `line`/`column`) and serde `category`; it never echoes catalog field
  values. A wrong-type field (e.g. a string where a number is expected) makes
  `serde_json`'s raw message name the offending value, so the raw message is
  deliberately dropped to honor the redaction-safe error contract.
- Unknown schema version:
  `{ "code": "unknown_schema_version", "version": { "domain": "…", "kind": "…", "version": 2 } }`
- Unknown evidence class:
  `{ "code": "unknown_evidence_class", "control_id": "…", "class": "…" }`
- Invalid requirement:
  `{ "code": "invalid_requirement", "control_id": "…", "class": "…", "requirement": "…" }`
- Duplicate evidence class within a control:
  `{ "code": "duplicate_evidence_class", "control_id": "…", "class": "…" }`
- Duplicate control ID:
  `{ "code": "duplicate_control_id", "control_id": "…" }`
