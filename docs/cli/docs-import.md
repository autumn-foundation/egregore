# `eg import docs`

Imports repo-local design docs (ADR/PRD/Plan markdown) as linkable
artifact-domain records (issue #149).

## Synopsis

```
eg import docs [--repo-root <path>] [--root <dir>]... [--code-graph <path>]
               [--out <path>] [--transaction-time <rfc3339>]
```

## Description

Deterministic and filesystem-local: scans the documented doc roots
(`docs/adr/`, `docs/prd/`, `docs/plans/` — override with repeatable `--root`),
emits one artifact-domain node per markdown doc (`ADR` / `PRD` / `PlanDoc`
shapes), plus `RELATES_TO` / `MENTIONS_SYMBOL` edges only for explicit literal
references (repo-relative file paths, fully-qualified symbol names) resolved
against the supplied `--code-graph` JSONL (from `scan`). No network, no
crawling. Without `--code-graph` every reference is an `unresolved_reference`
diagnostic.

Records go to `--out` (default: stdout) as JSONL. Diagnostics go to stderr as
JSON lines with stable `code` values (`unknown_root`, `empty_document`,
`malformed_document`, `kind_mismatch`, `unresolved_reference`,
`ambiguous_reference`, `zero_resolvable_references`); they carry at most the
bounded, redacted title — never raw body text.

Exit `0` when there are no diagnostics, `2` when any diagnostic was emitted
(records are still written on partial imports).

Re-importing unchanged docs with a fixed `--transaction-time` is
byte-identical. Changing a doc's body yields a new record id and content hash;
the previous record remains addressable as history.

Full contract: `docs/schema/doc-ingest.md`.

## Examples

```powershell
# Import this repo's design docs against its code graph.
eg scan . --out graph.jsonl
eg import docs --code-graph graph.jsonl --out docs.jsonl

# Deterministic re-import for tests.
eg import docs --code-graph graph.jsonl --transaction-time 2026-09-28T12:00:00Z --out docs.jsonl
echo $?  # 2 when any diagnostic was emitted
```

`eg query symbol <name>` prints `design-doc:` rows after the symbol rows for
docs explicitly linked to a matched symbol; subsystem queries surface linked
docs through the context `artifacts` section.
