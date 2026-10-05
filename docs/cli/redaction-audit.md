# eg query redaction-audit

Audit the resting store for secret-shaped values that bypassed the
write-boundary redaction gate — the verification counterpart to the gate
(issues #41, #101), not a replacement for it. Strictly read-only: no schema
change, no write-path change.

> **Advisory, never evidentiary.** Every finding is a pattern- or
> entropy-heuristic match over a persisted field — never a confirmed secret,
> and never a raw value. A clean sweep is not proof the store holds no
> secrets; it only reports that no secret-shaped value was found at rest in
> the swept fields. Remediation (retraction) is owned by issue #231; this
> lane never removes or remediates findings.

## Synopsis

```text
eg query redaction-audit --graph <PATH>    [--repo <SELECTOR>] [--format json|text]
eg query redaction-audit --data-dir <DIR>  [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--repo <SELECTOR>` restricts the sweep to one repository in
a multi-repo store; an unknown or ambiguous selector is rejected with a
machine-readable stderr diagnostic (exit 1), never resolved implicitly.
`--data-dir` reads from a throwaway copy of the store, never the live files
(same read-only contract as the sibling inventory lanes). Strictly
read-only: no records, indexes, or runtime files are created, modified, or
deleted.

| Condition | Exit | Output |
|-----------|------|--------|
| ≥1 finding reported | `3` | JSON or text on stdout, `ok:true` (JSON) |
| Clean store: zero findings | `0` | `{"ok":true, "findings":[], "empty_reason":"no_unredacted_secrets_in_scope", ...}` on stdout |
| Unknown / ambiguous `--repo` selector | `1` | `{"code":"unknown_repository_selector",...}` on stderr |
| Unreadable / missing graph input, or both/neither of `--graph`/`--data-dir` | `1` | Error message on stderr |

## Response fields (JSON)

Top-level envelope, one compact JSON document on a single line:

| Field | Meaning |
|-------|---------|
| `ok` | `true` on every stdout response (exits 0, 3) |
| `lane` | `"redaction_audit"` |
| `repo_scope` | The resolved `--repo` selector, when one was given |
| `disclaimer` | One-line soundness boundary: heuristic matches, never confirmed secrets, never raw values |
| `findings[]` | Finding rows in canonical order: (`record_id`, `field_path`, `classification`, `hash_prefix`) |
| `counts` | `records_scanned`, `fields_scanned`, `findings` |
| `empty_reason` / `message` | Clean sweep only: `"no_unredacted_secrets_in_scope"` plus the human message |

Each finding row:

| Field | Meaning |
|-------|---------|
| `record_id` | Stable record ID — the citable handle |
| `domain` | Record domain (`agent_memory`, `artifact`, `verification`, `project`, `user_context`, `log`, `codegraph`, or `unknown`): the record's stamped `domain`, else derived from the node kind |
| `field_path` | Swept field path, e.g. `text`, `stdout_handle.inline`, `log.template_excerpt`, `summary` |
| `classification` | A `secret_class` name (`api_token`, `ssh_private_key`, `database_url`, `cloud_credential`, `webhook_secret`, `session_cookie`, `env_secret`, `email`) or `high_entropy` |
| `hash_prefix` | First 12 lowercase-hex chars of the BLAKE3 hash of the detected value — the same scheme `<REDACTED:class:hash_prefix>` markers use, so a finding correlates with the marker that would have replaced the value. Non-reversible. |

The `--format text` rendering prints one line per finding
(`- <record_id> [<domain>] <field_path>: <classification>
hash_prefix=<hash>`), then a `counts:` tally line. A clean sweep prints the
`# no_unredacted_secrets_in_scope:` marker. Raw values never appear in either
format — findings carry no raw-value field at all.

## Swept fields

The audit sweeps these persisted string fields on every node record, in
canonical field order:

1. **The canonical sensitive-field index** (`crate::redaction::sensitive_fields`,
   documented in [redaction.md](redaction.md) §Sensitive field index): the
   redactable agent-memory, artifact, verification, project, and user-context
   fields — `text`, `validation_summary`, `arguments_summary`,
   `arguments_handle.inline`, `result_handle.inline`, `patch_handle.inline`,
   `stdout_handle.inline`, `stderr_handle.inline`, `title`,
   `body_handle.inline`, `diff_hunk_handle.inline`, `url`, `assignees[i]`,
   `labels[i]`, and the user-context texts (`proposed_rule_text`,
   `prompt_text`, `decision_rationale`, `edited_rule_text`, `rule_text`,
   `action_summary`, `constraint_text`).
2. **Log-graph excerpts**: `log.template_excerpt` (`ErrorSignature`) and
   `log.event_excerpt` (`LogEvent`) — the bounded post-redaction excerpts
   that pass through the redaction policy before persistence.
3. **Code-graph free text** — the prime unredacted-at-rest risk, since
   code-graph records are exempt from the write gate: `summary` (the symbol
   body), `signature`, `doc`, `note` (debt markers), `name` (import paths on
   `Import` nodes), `deprecated.since` / `deprecated.note`, and
   `author_name`.

Deliberately excluded:

- `Commit.author_email`: a redaction-off local store retains the raw author
  email by design (`docs/schema/redaction.md`); export-time redaction is the
  bundle lane's contract, not this audit's.
- Structural identifiers (record IDs, BLAKE3 hashes, repo-relative paths,
  spans, enum labels, timestamps): not queryable free text, never swept.

## Detection

Two layers, applied per field — patterns first, entropy second:

**(a) Known token patterns** via the same matchers as the write gate
(`crate::redaction::detect_secret_span`), aligned to the `secret_class`
taxonomy in `docs/schema/redaction.md`: `api_token` (e.g. `sk-…`, `ghp_…`,
`Bearer …`), `ssh_private_key` (PEM/OpenSSH markers), `database_url`
(`postgres://user:pass@…`, …), `cloud_credential` (`AKIA…`), `webhook_secret`
(`whsec_…`), `session_cookie` (`eyJ…` JWTs, `sessionid=…`), `env_secret`
(`KEY=VALUE` with a secret-name key), and `email`. Every match in a field
becomes a finding; matched spans are masked before the next pass so a
lower-priority pattern starting earlier in the field cannot be shadowed.

**(b) Generic high-entropy tokens**: maximal runs of token characters
(`[A-Za-z0-9+/=_.-]`) with **length ≥ 20 characters** and **Shannon entropy
≥ 4.2 bits per character**. The threshold pair is chosen so uniform hex
digests (4.0 bits/char — SHA-1/SHA-256, commit SHAs) and UUIDs (~4.08) stay
silent, while base32 (5.0), base62 (~5.95), and base64 (6.0) key material is
flagged. Constants: `MIN_HIGH_ENTROPY_TOKEN_LEN = 20`,
`MIN_HIGH_ENTROPY_BITS_PER_CHAR = 4.2` in `src/query/redaction_audit.rs`.

## Already-handled values are never flagged

- Records stamped `redaction_policy_version: "v1"` are skipped wholesale —
  the gate already accepted them.
- `<REDACTED:secret_class:hash_prefix>` markers never match a secret
  pattern, so marker-only values are silent. A raw secret sharing a field
  with a marker is still flagged — mirroring the gate, which rejects such
  fields until fully redacted.
- The **allowlist** suppresses documented known-safe matches so the audit is
  CI-usable without false-positive noise:
  - exact values: `AKIAIOSFODNN7EXAMPLE` (AWS IAM docs example),
    `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` (AWS docs example secret),
    `dGhpcy1pcy1hLXRlc3QtZml4dHVyZS10b2tlbi0wMTIz` (documented CI-harness
    token);
  - prefixes: `sk-test-`, `rk-test-`, `pk-test-` (Stripe test keys,
    docs.stripe.com).

  Constants: `ALLOWLIST_EXACT`, `ALLOWLIST_PREFIXES` in
  `src/query/redaction_audit.rs`. A genuinely leaked *test* key is suppressed
  by design; the audit targets production-shaped secrets.

## Determinism

Findings sort canonically by (`record_id`, `field_path`,
`classification`, `hash_prefix`); identical repeats within one field
deduplicate to a single finding (the handle is the citable unit). Re-running
the identical command against an unchanged store produces byte-identical
output.

## When to use it

Use `eg query redaction-audit` as the **verification counterpart to the
write-boundary gate**: after ingests, imports, or scans, confirm no
secret-shaped value is sitting at rest unredacted — especially in code-graph
symbol bodies, which the gate never sees.

Prefer something else when:

- **Blocking a write** — the gate itself (`eg` import/scan paths,
  `crate::redaction::validate_record`, issues #41/#101): rejects unredacted
  records before persistence. The audit verifies; the gate enforces.
- **Removing findings** — retraction is owned by issue #231 (`eg forget`
  lanes). This lane never deletes or redacts anything.
- **Validating a secret against its provider** — out of scope by design
  (no network-based secret validation); findings are never confirmed
  secrets.

## Contracts consumed

The lane reuses the existing redaction-policy contracts
(`docs/schema/redaction.md`, `crate::redaction`: `SecretClass`,
`detect_secret_span`, `sensitive_fields`, `REDACTION_POLICY_VERSION`,
`is_code_graph_kind`) and the query-loader / `--repo` / `--format`
contracts of the sibling inventory lanes. It introduces no new graph domain,
importer, trust class, or write path. Agent observations, project/task
state, and verification records are swept read-only; findings cite them by
record ID and never echo their values.
