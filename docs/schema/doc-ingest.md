# Design-Doc Ingest (`import docs`)

**Status:** Active (issue #149).

**Scope:** `eg import docs` ingests repo-local markdown design docs (ADRs,
PRDs, plans) as linkable artifact-domain records. It implements the reserved
`ADR` / `PRD` / `Plan` shapes from `docs/schema/agent-actions.md` §10. No new
graph domain, no new edge vocabulary, no network access.

**Coordination:** `docs/schema/producer-version.md` (the `doc_importer`
producer kind), `docs/schema/schema-versioning.md` (artifact `schema_version`
stays 1 — this slice is additive), `docs/schema/redaction.md` (title handling).

---

## 1 — Roots and kind rule

The importer scans exactly these repo-relative roots, in this canonical order:

| Root | Kind | Wire name (`NodeKind`) |
|------|------|------------------------|
| `docs/adr/` | ADR | `ADR` (`NodeKind::Adr`) |
| `docs/prd/` | PRD | `PRD` (`NodeKind::Prd`) |
| `docs/plans/` | Plan | `PlanDoc` (`NodeKind::PlanDoc`) |

Kind comes ONLY from the normalized root path or from an explicit documented
`kind:` front-matter field (`adr`/`prd`/`plan`, case-insensitive). The importer
never guesses: if the front-matter kind disagrees with the root, a
`kind_mismatch` diagnostic is emitted and the **path rule wins**. An unknown
`kind:` value is a `malformed_document` diagnostic.

`Plan` is materialized as wire name `PlanDoc` because `Plan` is already the
project-domain task-plan kind (`NodeKind::Plan`); the artifact-domain wire name
disambiguates.

A root that is not one of the three documented roots, or that does not resolve
to a directory in the target repo, yields an `unknown_root` diagnostic and is
skipped; the remaining roots still import. Absent *default* roots are the
exception: most repos do not carry all three, so a documented default root
that is simply missing is skipped silently. Only an explicitly passed
`--root` that does not resolve is diagnosed.

Discovery is deterministic and filesystem-local: recursive scan for `*.md`
files (lowercase extension), sorted by normalized repo-relative path (forward
slashes). No network, no crawling, no watching.

---

## 2 — Record shape

One node per markdown doc:

| Field | Value |
|-------|-------|
| `id` | `artifact_stable_id(["doc", <kind-slug>, <repo-relative-path>, <content-hash>])` → `artifact:v1:<blake3-hex>` |
| `kind` | `ADR` / `PRD` / `PlanDoc` (serde wire names) |
| `domain` | `"artifact"` (via `with_domain("artifact", ARTIFACT_SCHEMA_VERSION)`) |
| `schema_version` | `1` (`ARTIFACT_SCHEMA_VERSION`) |
| `repo_relative_path` | Normalized repo-relative doc path |
| `title` | Bounded (200 chars), redacted title — front-matter `title:` else first `# ` heading |
| `name` | Same as `title` (node display name) |
| `summary` | `"<WIRE> <title>"` or `"<WIRE> (untitled)"` — templated, no body text |
| `source_artifact_hash` | BLAKE3 hex of the raw doc bytes = the durable content handle (`content_hash`) |
| `producer` | `doc_importer` envelope (`importer_schema_version: 1`, `source_format_version: doc-ingest/v1`) |

Edges use the existing vocabulary via `GraphRecord::artifact_edge` (artifact
stable-id namespace, artifact schema version):

| Reference | Edge | Target |
|-----------|------|--------|
| Literal repo-relative file path | `RELATES_TO` | codegraph `File` node |
| Fully-qualified symbol name | `MENTIONS_SYMBOL` | codegraph `Symbol` node |

The edge `id` is `artifact_stable_id(["artifact", "edge", <label>, <source>, <target>])`;
the edge summary is templated (`doc '<path>' explicitly references '<handle>'`)
and carries no body text.

---

## 3 — Reference extraction and resolution

References are explicit literal handles from two documented sources:

1. Front-matter lists:
   ```yaml
   ---
   title: Human title
   kind: adr            # optional; must agree with the root path
   references:          # literal repo-relative file paths
     - src/ir.rs
   symbols:             # fully-qualified symbol names
     - ir::GraphRecord
   ---
   ```
   Unknown front-matter keys are ignored (forward-compatible). A `references:`
   / `symbols:` key with an inline scalar takes one handle; an empty value
   starts a `- ` list. Any other shape is a `malformed_document` diagnostic.
2. Backtick-delimited literals in the body. A literal containing `::` is a
   symbol handle; one containing `/` or `.` is a file-path handle (leading
   `./` and `/` stripped, backslashes normalized); anything else is prose.

Resolution reuses the `link_evidence` contract: exact lookup against indexes
built from the supplied code graph (`File` by `repo_relative_path`,
`Symbol` by fully-qualified `name`). One handle → one edge. Zero matches →
`unresolved_reference` diagnostic. Multiple matches → `ambiguous_reference`
diagnostic. No edge is ever invented, and no failed reference is silently
dropped: every unlinked handle is a diagnostic.

A doc with zero resolved references still materializes its node, plus a
`zero_resolvable_references` diagnostic.

---

## 4 — Trust separation

Document facts remain artifact trust (`TrustClass::Artifact`, agent-authored
side of the trust partition — see `docs/schema/producer-version.md` §10 twin
`node_trust_class`). The importer never writes codegraph records and never
mutates the supplied code graph, so document claims can never become code
truth or overwrite codegraph facts. `query symbol` and subsystem context
surface docs as explicitly-linked artifact rows alongside — never merged
into — code facts.

---

## 5 — Idempotence and history

The node id binds kind + repo-relative path + BLAKE3 content hash:

* Re-importing unchanged docs with a fixed `--transaction-time` is
  byte-identical (no-op).
* Changing the body changes the content hash and therefore the record id: the
  new import is a new record, and the previous record remains addressable as
  history (old id + old hash coexist with new id + new hash).
* The producer envelope is a non-identity envelope: it never contributes to
  stable IDs.

---

## 6 — Diagnostics and exit codes

Every diagnostic is a stable machine-readable JSON object on stderr:

```json
{"code":"unresolved_reference","doc":"docs/adr/0001-x.md","handle":"src/nope.rs","title":"…","message":"…"}
```

| Code | Meaning |
|------|---------|
| `unknown_root` | Root not documented, or not a directory in the repo |
| `empty_document` | Empty or whitespace-only doc |
| `malformed_document` | Not UTF-8, unterminated front matter, unknown `kind:` value, or other unparseable shape |
| `kind_mismatch` | Front-matter `kind:` disagrees with the root path (path wins) |
| `unresolved_reference` | Handle matches no codegraph record |
| `ambiguous_reference` | Handle matches multiple codegraph records |
| `zero_resolvable_references` | Doc produced no edges; the doc emits no record at all |

Diagnostics carry at most the bounded, redacted title — never raw body text.

`eg import docs` exits `0` when there are no diagnostics and `2` when any
diagnostic was emitted. Records are still written when diagnostics exist
(partial import).

---

## 7 — Query surfacing

`eg query symbol <name>` prints `design-doc:` rows after the symbol rows for
docs explicitly linked via `MENTIONS_SYMBOL` to a matched symbol. Subsystem
queries surface linked docs through the existing context `artifacts` section
(`RELATES_TO` / `MENTIONS_SYMBOL` are followed bidirectionally), classified
as artifact trust. Each row carries the artifact record id, kind,
repo-relative path, and BLAKE3 content hash — never body text. With no
governing docs the result is empty: honest empty, never fabricated.
