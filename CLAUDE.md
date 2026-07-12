# Egregore Agent Guide

## Project Shape

Egregore is a standalone Rust project for building an AletheiaDB-backed knowledge graph substrate for agentic software engineering. Keep the repo independent from AletheiaDB release mechanics; integrate through thin adapter boundaries and documented schema contracts.

The current implemented domain is code graph extraction: current and historical codebase structure becomes deterministic graph records that can share an AletheiaDB store with future agent memory, project/task, artifact, and verification-evidence domains.

## Current Status

The MVP extraction, CLI, embedded ingest, history replay, incremental cache, semantic drift, and query helper surfaces are implemented with tests.

Primary commands:

```powershell
cargo run -- scan . --out graph.jsonl
cargo run -- scan-history . --out history.graph.jsonl
cargo run -- scan-logs app.log --repo-path . --out log.graph.jsonl
cargo run -- inspect graph.jsonl
cargo run -- ingest graph.jsonl --adapter dry-run
cargo run -- ingest history.graph.jsonl --adapter embedded --data-dir .egregore
cargo run -- inspect --data-dir .egregore
cargo run -- query semantic-memory "parser edge case on empty input" --data-dir .egregore
```

`eg scan-logs <log> --repo-path <repo>` extracts runtime log signatures from one
captured log file (issues #319/#320): a `LogSource`, one `ErrorSignature` per
`template-v1` fingerprint (a 1000×-repeated error collapses to one signature with
`occurrence_count` == the raw count), capped `LogEvent` exemplars, and hourly
`LogOccurrenceBucket` counts. Trust class `runtime_observation` — a program's own
claim, deterministically parsed but never verified. Raw log text never enters the
graph; excerpts are `template-v1`-normalized, redacted, and bounded. Deterministic
and byte-stable; CRLF and LF checkouts yield identical IDs. Unrecognized
(binary/non-UTF-8) input exits 1 with a machine-readable diagnostic and no partial
output. Add `--protected-raw-artifacts --protected-store <dir> --producer <id>`
(issue #321) to also capture the log's post-redaction bytes into the #60 protected
store as a `log_payload` blob (disabled by default; the graph never stores the
handle; capture I/O failure exits 3 with no partial manifest). See
`docs/cli/scan-logs.md` and `docs/schema/log-graph.md`.

`eg resolve-frames <log.graph.jsonl> --graph <code.graph.jsonl> --out <out>`
(issue #322) resolves the structured, redaction-safe backtrace frames captured
on each `ErrorSignature` (scan-time capture, since `template-v1` normalization
drops file paths and long backtraces) to code-graph targets, emitting
`FRAME_RESOLVES_TO` edges each labeled with a closed-set `frame_resolution`
(`resolved` / `ambiguous` / `path_only` / `unresolved`) plus a `frame_index`,
mirrored by an `EvidenceLink` on the signature. The ladder mirrors
`CallResolution` (#152/#134): `ambiguous` enumerates every candidate and never
silently picks one; `unresolved` targets a `Diagnostic` carrying the redacted
frame text, never an invented symbol; `path_only` targets the `File` node.
Frames into stdlib/deps are classified `external` in a per-signature tally and
mint no edge. Accepts `--data-dir` and `--at`/`--as-of` (mutually exclusive).
Frames are non-identity. A binding proves the frame NAMES the symbol, never that
the symbol is at fault. Deterministic and byte-stable; raw log text never enters
the graph. See `docs/cli/resolve-frames.md`.

`eg link-logs --graph <graph.jsonl>... --out <out>` (issue #323) links each
`ErrorSignature` (#320) to the agent runs/commands that produced it, emitting
`EMITTED_DURING` evidence-link edges over a union graph of log + agent-memory +
verification + project records. Every edge carries exactly one closed-set
`basis`: `content_hash_join` (a `CommandRun`'s captured stdout/stderr
`OutputHandle.hash` equals the signature's `CAPTURED_FROM` `LogSource`
`source_artifact_hash` — exact byte equality, confidence `1.0`, inherently
within-repo) or `temporal_correlation` (the signature's `last_seen`/`first_seen`
falls inside an `AgentRun`/`AgentTurn` window `[started_at, finished_at]` ±
`--tolerance`, default 0, for the SAME repository — confidence `0.5`, "a
correlation lead, never causation"). Overlapping runs each mint one edge (no
silent winner); a signature with zero edges is tallied `uncorrelated`.
Repository boundary: `temporal_correlation` requires exactly one `Repository`
anchor and verifies each signature's `LogSource` recomputes to it (reading the
repository id the log ID hash already encoded — no new field); foreign or
anchor-ambiguous candidates are tallied `cross_repo_rejected`. Task/issue links
reuse `REFERENCES_TASK` (`ErrorSignature` → `Task`/`GitHubIssue`/`LocalTask`) —
no new project label. Every edge is mirrored by an `EvidenceLink` on the
signature (dual representation). Deterministic, byte-stable, idempotent; raw
log/transcript/command text never enters the graph; `--at`/`--as-of` mutually
exclusive (exit 1). See `docs/cli/link-logs.md` and the `EMITTED_DURING` basis
section of `docs/schema/log-graph.md`.

`eg inspect --data-dir` inspects an embedded store directly — no daemon, no
network, no embeddings (issue #125, the daemon-free analog of #47). It reports
totals plus per-domain/per-kind/per-schema-version counts grouped by trust
class, counts unknown `(domain, kind, schema_version)` tuples distinctly, is
strictly read-only (the store is read via a throwaway temporary copy), and by
default emits one deterministic JSON line that is byte-identical across runs on
an unchanged store (`--format text` matches the JSONL inspect style). A missing
store, an empty directory, or an initialized store holding zero Egregore
records fails with a diagnostic naming the path — never successful zero
counts. See `docs/cli/inspect.md` for the JSON contract.

Query commands (local JSONL graph, no network):

```powershell
# Symbol-level cross-domain context
cargo run -- query context <symbol_name> --graph graph.jsonl

# Subsystem-scoped cross-domain context (issue #83)
cargo run -- query subsystem src/adapters --graph graph.jsonl   # exit 0 on match
cargo run -- query subsystem src/nonexistent --graph graph.jsonl  # exit 2 (no_match)
cargo run -- query subsystem "" --graph graph.jsonl               # exit 1 (malformed_prefix)

# Change-impact blast-radius leads for a symbol or file (issue #76)
cargo run -- query change-impact <symbol_name> --graph graph.jsonl        # exit 0 on match
cargo run -- query change-impact src/lib.rs --graph graph.jsonl           # file handle
cargo run -- query change-impact codegraph:v1:zzz --graph graph.jsonl     # exit 1 (Unsupported)
cargo run -- query change-impact does_not_exist --graph graph.jsonl        # exit 2 (no_match)
cargo run -- query change-impact <symbol_name> --graph graph.jsonl --depth 2  # wider neighborhood

# Transitive inbound callers with call paths (issue #139)
cargo run -- query transitive-callers <symbol_name> --graph graph.jsonl       # exit 0 (even when empty)
cargo run -- query transitive-callers <ambiguous_name> --graph graph.jsonl    # exit 1 (candidates listed)
cargo run -- query transitive-callers does_not_exist --graph graph.jsonl      # exit 2 (no_match)
cargo run -- query transitive-callers <symbol_name> --graph graph.jsonl --max-depth 3
cargo run -- query transitive-callers <symbol_name> --graph history.graph.jsonl --at <sha>  # commit view

# Transitive outbound callees/dependencies with paths (issue #253)
cargo run -- query transitive-callees <symbol_name> --graph graph.jsonl       # exit 0 (even when empty)
cargo run -- query transitive-callees <ambiguous_name> --graph graph.jsonl    # exit 1 (candidates listed)
cargo run -- query transitive-callees does_not_exist --graph graph.jsonl      # exit 2 (no_match)
cargo run -- query transitive-callees <symbol_name> --graph graph.jsonl --max-depth 3
cargo run -- query transitive-callees <symbol_name> --graph history.graph.jsonl --at <sha>  # commit view

# Direct outbound dependencies of a symbol (issue #123)
cargo run -- query deps <symbol_name> --graph graph.jsonl          # exit 0 (even when empty)
cargo run -- query deps <ambiguous_name> --graph graph.jsonl       # exit 1 (candidates listed)
cargo run -- query deps does_not_exist --graph graph.jsonl         # exit 2 (no_match)
cargo run -- query deps <symbol_name> --graph history.graph.jsonl --at <sha>  # commit view

# Symbol- and file-level deltas across a commit range (issue #118)
cargo run -- query deltas <base_sha> <head_sha> --graph history.graph.jsonl  # exit 0 on match
cargo run -- query deltas <sha> <sha> --graph history.graph.jsonl            # exit 1 (identical_endpoints)
cargo run -- query deltas ffffffffffff <head_sha> --graph history.graph.jsonl # exit 2 (missing_commit)

# Runtime error-signature deltas across a commit range (issue #326)
cargo run -- query log-deltas <base_sha> <head_sha> --graph combined.graph.jsonl  # exit 0 on match
cargo run -- query log-deltas <sha> <sha> --graph combined.graph.jsonl            # exit 1 (identical_endpoints)
cargo run -- query log-deltas ffffffffffff <head_sha> --graph combined.graph.jsonl # exit 2 (missing_commit)

# A file's defined-symbol set at a past commit or instant (issue #158)
cargo run -- query file src/lib.rs --graph history.graph.jsonl --at <commit_sha>             # exit 0 on match
cargo run -- query file src/lib.rs --graph history.graph.jsonl --as-of 2026-01-02T00:00:00Z  # exit 0 on match
cargo run -- query file src/nope.rs --graph history.graph.jsonl --at <commit_sha>            # exit 2 (unknown_path)
cargo run -- query file src/lib.rs --graph history.graph.jsonl --tx-as-of <instant>          # exit 1 (not_implemented)

# Ranked historical co-change partners for a file (issue #153)
cargo run -- query coupling src/lib.rs --graph history.graph.jsonl            # exit 0 (even when empty)
cargo run -- query coupling src/lib.rs --graph history.graph.jsonl --min-support 5 --limit 10
cargo run -- query coupling src/lib.rs --graph history.graph.jsonl --base <sha> --head <sha>
cargo run -- query coupling src/lib.rs --graph history.graph.jsonl --at <sha>   # history as of one commit
cargo run -- query coupling src/nope.rs --graph history.graph.jsonl            # exit 2 (unknown_file)

# One symbol's full evolution timeline across commit history (issues #96, #215)
cargo run -- query lifeline <symbol_name> --graph history.graph.jsonl     # exit 0, NDJSON events
cargo run -- query lifeline does_not_exist --graph history.graph.jsonl    # exit 2 (unknown_symbol)
cargo run -- query lifeline <symbol_name> --graph history.graph.jsonl --format text  # timeline view

# Externally-reachable public API surface (issue #213)
cargo run -- query public-api --graph graph.jsonl                 # exit 0 (even when surface is empty)
cargo run -- query public-api --graph graph.jsonl --repo acme/widget  # scope one repo; bad selector exits 1

# Public-API surface changes across a commit range (issue #157)
cargo run -- query public-api-deltas <base_sha> <head_sha> --graph history.graph.jsonl  # exit 0 on match
cargo run -- query public-api-deltas <sha> <sha> --graph history.graph.jsonl            # exit 1 (identical_endpoints)
cargo run -- query public-api-deltas ffffffffffff <head_sha> --graph history.graph.jsonl # exit 2 (missing_commit)
cargo run -- query public-api-deltas <base> <head> --graph history.graph.jsonl --include-internal --callers

# Undocumented public API symbols — doc-debt triage (issue #257)
cargo run -- query undocumented --graph graph.jsonl               # exit 0 (even when nothing is undocumented)
cargo run -- query undocumented --graph graph.jsonl --limit 20 --format text
cargo run -- query undocumented --graph graph.jsonl --include-private  # whole-crate doc audit

# Per-file ownership shares and bus factor from Git authorship (issue #245)
cargo run -- query ownership --graph history.graph.jsonl                    # exit 0 (even when surface is empty)
cargo run -- query ownership src/lib.rs --graph history.graph.jsonl         # one file
cargo run -- query ownership --graph history.graph.jsonl --at <sha>         # ownership as-of a commit
cargo run -- query ownership does/not/exist.rs --graph history.graph.jsonl  # exit 2 (unknown_path)
cargo run -- query ownership --graph history.graph.jsonl --as-of not-a-time # exit 1 (malformed_timestamp)

# Zero-inbound-reference prune-triage leads (issue #113)
cargo run -- query unreferenced --graph graph.jsonl               # exit 0 (even when no candidates)
cargo run -- query unreferenced --graph graph.jsonl --repo acme/widget  # scope one repo; bad selector exits 1

# TODO/FIXME/HACK/XXX debt-comment marker inventory (issue #218)
cargo run -- query debt-markers --graph graph.jsonl                         # exit 0, full inventory
cargo run -- query debt-markers --graph graph.jsonl --path src/adapters     # subsystem-scoped
cargo run -- query debt-markers --graph graph.jsonl --path src/nonexistent  # exit 2 (scope_not_found)
cargo run -- query debt-markers --graph history.graph.jsonl --at <commit>   # pinned valid-time view

# Unsafe-code surface inventory (issue #222)
cargo run -- query unsafe-sites --graph graph.jsonl                       # exit 0, full inventory + count
cargo run -- query unsafe-sites --graph graph.jsonl --path src/ffi        # subsystem-scoped
cargo run -- query unsafe-sites --graph graph.jsonl --path src/nonexistent  # exit 2 (scope_not_found)
cargo run -- query unsafe-sites --graph history.graph.jsonl --at <commit>   # pinned valid-time view

# file:line location → smallest enclosing symbol (issue #151)
cargo run -- query at src/lib.rs:42 --graph graph.jsonl           # exit 0 on match
cargo run -- query at src/lib.rs:2 --graph graph.jsonl            # exit 2 (no_enclosing_symbol)
cargo run -- query at src/lib.rs:42 --graph history.graph.jsonl --at <sha>  # spans as of that commit
cargo run -- query at src/lib.rs --graph graph.jsonl              # exit 1 (malformed_location)

# file:line position → innermost symbol + its query-context bundle (issue #212)
cargo run -- query locate src/lib.rs:42 --graph graph.jsonl       # exit 0, symbol + context bundle
cargo run -- query locate src/lib.rs:2 --graph graph.jsonl        # exit 2 (no_enclosing_symbol)
cargo run -- query locate src/lib.rs:9999 --graph graph.jsonl     # exit 2 (line_out_of_range)
cargo run -- query locate src/lib.rs:42 --graph history.graph.jsonl --at <sha>       # spans as of a commit
cargo run -- query locate src/lib.rs:42 --graph history.graph.jsonl --as-of <instant> # spans as of an instant
cargo run -- query locate src/lib.rs --graph graph.jsonl          # exit 1 (malformed_location)

# Declared Cargo dependencies with lockfile resolution (issue #180)
cargo run -- query manifest-deps --graph graph.jsonl                # exit 0 (even when surface is empty)
cargo run -- query manifest-deps --graph graph.jsonl --name serde   # direct "do we depend on X?" lookup
cargo run -- query manifest-deps --data-dir .egregore --format text # store-backed, human-readable

# File churn hotspots over a scan-history store (issue #128)
cargo run -- query churn --graph history.graph.jsonl               # exit 0, ranked files
cargo run -- query churn --graph history.graph.jsonl --limit 10    # cap output (default 50, max 500)
cargo run -- query churn --graph graph.jsonl                       # exit 2 (no_history: not a history store)

# Producer-identity drift audit against the current binary (issue #234)
cargo run -- query producer-drift --graph graph.jsonl                  # exit 0 (even with drift)
cargo run -- query producer-drift --data-dir .egregore --repo acme/widget  # scope one repo; bad selector exits 1
cargo run -- query producer-drift --graph graph.jsonl --format text

# Dependency cycles among files over IMPORTS/CALLS edges (issue #138)
cargo run -- query cycles --graph graph.jsonl                     # exit 0 (even when acyclic)
cargo run -- query cycles src/lib.rs --graph graph.jsonl          # only cycles through this node
cargo run -- query cycles does_not_exist --graph graph.jsonl      # exit 2 (no_match)
cargo run -- query cycles codegraph:v1:zzz --graph graph.jsonl    # exit 1 (Unsupported)
```

`eg query subsystem <prefix>` returns code facts, agent observations, project state, artifacts,
verification evidence, and semantic drift for everything under a repo-relative directory prefix.
Path matching is segment-aware: `src/alpha` never bleeds into `src/alphabet`.
Output is a deterministic JSON envelope with trust-separated sections.

`eg query change-impact <handle>` returns graph-derived impact leads grouped by relation
(`direct_callers`, `direct_callees`, `referencing_files`, `implementation_symbols`,
`containing_context`) for a symbol name, canonical record ID, or repo-relative file path.
Rows are leads to inspect before editing, not proof of breakage. The response is deterministic
and byte-identical across runs. Default depth is 1 (direct neighbors only); use `--depth 2`
for a wider BFS neighborhood. Output is a JSON envelope with an always-present disclaimer.

`eg query transitive-callers <handle>` walks the transitive inbound `CALLS`/`REFERENCES`
closure of a symbol (record ID or exact name) up to `--max-depth` (default 5) and returns
every reachable symbol with its hop distance and one concrete shortest connecting call path
of record-ID/edge-label handles. Cycles terminate deterministically (each symbol reported
once, shortest path); reaching the bound emits a truncation diagnostic counting dropped
frontier nodes per depth. Call-resolution labels (issues #152/#134) propagate along paths:
each row carries the weakest resolution on its chain. Ambiguous names exit 1 listing all
candidate record IDs; `--at`/`--as-of` walk a single-commit history view. Output is
newline-delimited JSON (summary envelope line, then one row per line), byte-identical across
runs. Rows are reachability leads, never proof of breakage. See
`docs/cli/transitive-callers.md`.

`eg query transitive-callees <handle>` is the outbound mirror of transitive-callers: it walks
the transitive outbound `CALLS`/`IMPLEMENTS`/`IMPORTS`/`REFERENCES` closure of a symbol
(record ID or exact name) up to `--max-depth` (default 5) and returns every reachable node
with its hop distance and one concrete shortest connecting dependency path of
record-ID/edge-label handles running from the anchor out to the node. The `--max-depth=1`
reachable-plus-unresolved result is exactly the `eg query deps` (#123) direct dependency set,
so it carries #123's explicit `unresolved` category: an outbound edge whose target is a
Diagnostic marker, an `unresolved` call, or a missing record is reported there — never
silently dropped and never counted as reachable. Cycles terminate deterministically (each
node reported once, shortest path); reaching the bound emits a truncation diagnostic counting
dropped frontier nodes per depth. Call-resolution labels (issues #152/#134) propagate along
paths: each row carries the weakest resolution on its chain. Ambiguous names exit 1 listing
all candidate record IDs; `--at`/`--as-of` walk a single-commit history view. Output is
newline-delimited JSON (summary envelope line, then reachable rows, then unresolved rows),
byte-identical across runs. Rows are reachability leads, never proof of breakage or runtime
behavior. See `docs/cli/transitive-callees.md`.

`eg query deps <handle>` returns the direct outbound dependencies of a symbol (record ID or
exact name) — its `CALLS`/`IMPLEMENTS`/`IMPORTS`/`REFERENCES` neighbors — each labeled with
the edge type that produced it and carrying a stable record ID, repo-relative file/span
handle, and any `CALLS` resolution status (issues #152/#134). Unresolved targets (a call
with no in-repo definition, or a missing target record) are an explicit `unresolved`
category with a stable reason, never silently dropped. Ambiguous names exit 1 listing all
candidate record IDs; `--at`/`--as-of` return the dependency set at a single-commit history
view. Output is newline-delimited JSON (summary envelope line, then one row per line),
byte-identical across runs, with a `--format text` mode. Rows are dependency leads, never
proof of runtime behavior. See `docs/cli/deps.md`.

`eg query deltas <base> <head>` returns the observed structural deltas between two commit
handles (full SHA or unique prefix) from a `scan-history` graph or embedded store, grouped by
stable change class (`added_symbols`, `removed_symbols`, `modified_symbols`, `added_files`,
`removed_files`, `modified_files`, plus an `unresolved` diagnostic group). Renames surface as
a removed+added pair. Each row carries a stable record ID, schema version, path, span when
available, and the introducing commit with its valid time. Semantic drift inside the range is
folded in where drift records exist and marked unavailable otherwise. Rows are observed
deltas, never proof of behavior change; the response is deterministic and byte-identical
across runs. See `docs/cli/deltas.md`.

`eg query log-deltas <base> <head>` classifies runtime error-signatures across a commit range
(issue #326), composing the #118 range mechanics with the #319/#320 `ErrorSignature`
valid-time model and the #322 `FRAME_RESOLVES_TO` edges. It derives a valid-time window from
the committer dates of the range commits (`window_start`/`window_end` = min/max) and sorts
every in-scope signature into a closed, mutually exclusive 3-class set by precedence:
`new_signatures` (`window_start <= first_seen <= window_end` — the regression signal, even for
a signature that also ceased in-window), `ceased_signatures` (existed before the range,
`last_seen < window_end`), and `continuing_signatures` (existed before, `last_seen >=
window_end`). A signature first observed after the window (`first_seen > window_end`) is
out-of-range and excluded from all three classes. Each `new_signatures` row joins its
`FRAME_RESOLVES_TO` targets against the reused `range_deltas` symbol groups into
`overlapping_symbol_deltas` (never re-derived). Per-window occurrence counts come from the
signature's `LogOccurrenceBucket` records via `AGGREGATES` edges (`base_window_occurrences`/
`head_window_occurrences` = buckets at/before each endpoint's committer date); a signature
with no linked buckets falls back to its aggregate `occurrence_count` with
`occurrence_source: aggregate_only` — counts are never fabricated. These per-window counts are
hour-bucket-granular, not endpoint-exact (the envelope carries
`occurrence_count_granularity: "hourly_bucket"` and the disclaimer states it): a
`LogOccurrenceBucket` retains only an hour-aligned `bucket_start` and an aggregate count (no
per-occurrence timestamps), so a bucket straddling a commit instant cannot be sub-divided and a
count may include occurrences up to one bucket width (1 hour) past the exact endpoint when it
falls mid-hour; the "fully-before" predicate is deliberately not used (it would under-count
instead), and endpoint-exact counts require a #320 schema change tracked in #364. Per-window bucket counts
and the aggregate `occurrence_count` both SUM across all scanned sources: a
`LogOccurrenceBucket` record ID is `(repository/signature/hour/width)` and omits `LogSource`,
so distinct sources sharing a bucket ID are preserved by summing (never deduped by bucket ID);
the symmetric cost is that concatenating the identical `scan-logs` output multiplies counts,
so scan each source once (or use per-source stores). Fully source-attributed counts require
source-aware bucket identity, a #320 schema change out of #326's scope, tracked in issue #361. All timestamp comparisons
(window derivation, classification, bucket cutoffs) are by parsed UTC instant, never raw RFC
3339 string order, because commit committer dates carry local offsets (`%cI`) while scan-logs
times are Z-normalized — a lexical comparison would misclassify across offsets. `--repo`
scopes only the code side (commit/window resolution and the symbol-delta join): log records
carry no retrievable repository attribution (the repo ID is only hashed into their stable
IDs), so all in-window log signatures are always included and per-repository log separation
requires per-repository stores. Whenever `--repo` is set the response envelope discloses this
in a machine-readable `repo_scope_caveat` field (log signatures are NOT repository-filtered and
a scoped run cannot be guaranteed repo-specific for them). The disclosure NEVER asserts log
isolation from the `Repository`-node count: log records add no `Repository` node, so a store
reporting a single `Repository` node can still hold another repository's log graph (repo-A
history plus a repo-B `scan-logs` graph) whose in-window signatures classify here regardless of
`--repo`. `distinct_repository_count`/`multi_repository_store` are informational raw counts, not
an isolation verdict; the multi-repository case only ADDS a higher-known-risk note and a single
count is never downgraded to "safe". The schema-level fix that would let `--repo` filter log
signatures is tracked in #362. Because `LogSource` is a non-identity input (a signature's
stable ID is `(repository_id, fingerprint_algorithm, template, severity)` only), a graph
combining multiple `scan-logs` outputs for one repo carries the same signature record ID more
than once; those records are grouped by stable ID and merged BEFORE classifying — earliest
`first_seen`, latest `last_seen` (by instant), buckets summed across the group (not deduped by
bucket record ID, per #361), aggregate `occurrence_count` summed — so exactly one row per signature ID is emitted, never
split across conflicting classes. Exit codes and the error
taxonomy mirror #118 exactly. Rows are regression LEADS, never proof this range caused the
failure; a ceased signature is not proof of a fix; occurrence data only reflects scanned log
sources. Cross-scan signature coalescing is a `--graph` capability: the embedded (`--data-dir`)
read path retains one record per stable non-temporal log ID (last-write-wins for
`ErrorSignature`/`LogOccurrenceBucket`), so multiple `scan-logs` ingests of the same stable ID
collapse before the query runs and coalescing is not reconstructable there — a single ingest is
unaffected. When run over `--data-dir` with log records present, the envelope carries an
`embedded_log_retention_caveat` disclosing this; for multi-scan aggregation combine `scan-logs`
outputs at the `--graph` level (concatenated JSONL) or use per-source stores (the adapter-level
retention fix is tracked in #363). Read-only, redaction-safe (no raw log text), deterministic and
byte-identical across runs. See `docs/cli/log-deltas.md`.

`eg query file <path> --at <commit>` / `--as-of <instant>` reconstructs the deterministic
set of symbols a file defined at a chosen commit or valid-time instant (issue #158) from a
`scan-history` graph or embedded store. A symbol tombstoned at or before the point never
appears; spans and names are the recorded state as-of the point, not the current tree. Each
row carries a stable record ID plus a repo-relative file/span handle, and the envelope
records the resolved commit/instant. A file that existed but defined zero symbols is an
explicit `empty_symbol_set` success (exit 0); an unknown path or a path absent at the point
is a machine-readable error (exit 2); `--tx-as-of` on `query file` is reserved and returns
`not_implemented` (exit 1). Code-facts only, read-only, and byte-identical across runs.
See the `eg query file` section of `docs/cli/query.md`.

`eg query coupling <path>` ranks the files that historically changed in the same commits as
a target file over a `scan-history` store, using the distinct-commit co-change count and a
documented normalized strength (`jaccard_v1`: shared commits over the union of both files'
change sets) plus a directional confidence (shared commits over the target's changes), so
high-churn files cannot dominate purely by volume. A minimum-support threshold
(`--min-support`, default 2, max 100) suppresses noise pairs and is echoed in the answer;
`--limit` (default 20, max 500) caps rows with an explicit truncation signal. Temporal scope
follows the existing selector contract: full history, `--base`+`--head` range, `--at`, or
`--as-of`. Both target and partners must resolve to `File` nodes, so untracked, ignored, and
non-source paths never appear. Rows are historical co-change leads — never proof of
dependency, and absence of coupling is not proof of independence. Output is deterministic
and byte-identical across runs. See `docs/cli/coupling.md`.

`eg query lifeline <symbol>` returns one symbol's chronologically ordered lifecycle events
(`introduced`, `modified`, `removed`, `reintroduced`) from a `scan-history` graph or embedded
store, keyed on the stable symbol-identity contract (ADR-0004) so same-name symbols never
bleed into the answer. Each event carries the commit SHA, its valid time, a stable record ID,
a repo-relative file/span handle (or documented absent-span reason), and the `SemanticDrift`
record ID + score for a modifying step when a drift record exists (drift-absent otherwise,
never a fabricated 0). Output is newline-delimited JSON by default — one event per line,
byte-identical across runs; `--format text` prints a human-readable timeline. Unknown symbols
and symbols with no commit-linked history exit 2; ambiguous names list all candidate record
IDs and exit 6. Events are advisory temporal facts, never a risk or behavior claim.
See `docs/cli/lifeline.md`.

`eg query public-api` enumerates the Rust library crate's externally-reachable public API
surface from recorded per-symbol visibility (issue #124) and module containment — never a
`pub` token grep. `pub` items trapped in non-`pub` modules are excluded; `pub use` re-exports
are included and attributed to the re-export site; `pub(crate)`/`pub(super)`/`pub(in path)`
items are crate-internal and tallied, not listed. Every row carries a stable record ID plus a
repo-relative file/span handle. An empty surface is an explicit machine-readable success
(exit 0 with an `empty_surface` diagnostic), not an error. Output is deterministic and
byte-identical across runs. Parse-derived, never a build-verified or semver claim.
See `docs/cli/public-api.md`.

`eg query unsafe-sites` inventories the scanned repo's own `unsafe`-code surface detected
over the Tree-sitter AST (never the word `unsafe` in comments, strings, doc comments, or
identifiers). Each row carries a closed site kind (`block` / `fn` / `impl`), the stable record
ID, the repo-relative file/span handle, and the enclosing symbol handle (explicit `null` when
top-level); the envelope reports an aggregate count equal to the number of returned sites.
Rows are an advisory inventory from deterministic extractor facts, never a soundness verdict —
a zero count is not a safety guarantee (macro-generated, build-script, and dependency `unsafe`
are out of this slice). Accepts `--path` (subsystem prefix), `--repo`, and `--at <commit>`
(valid-time pin). An empty scope reports `no_sites_in_scope`; an out-of-store scope is
`scope_not_found` (exit 2). The kind set is closed for this slice. See
`docs/cli/unsafe-sites.md`.

`eg query at <path>:<line>` resolves a raw location (compiler diagnostic, backtrace frame,
diff hunk, `git blame -L` output) to the smallest enclosing `Symbol` node whose recorded span
contains that line, plus the enclosing chain of containing modules and symbols ordered
outermost → innermost. Pure span-containment over already-stored data — no daemon, no
embeddings. A line outside every symbol span is a typed `no_enclosing_symbol` answer (exit 2),
never a nearest-neighbor guess; malformed locations exit 1 (`malformed_location`). The `--at
<commit>` temporal pin resolves spans as they existed at that commit. Output is a
deterministic JSON envelope, byte-identical across runs. See `docs/cli/query.md`.

`eg query locate <path>:<line>` is the positional sibling of `query at` and positional entry
into the `eg query context` contract (issue #212): it resolves the innermost enclosing `Symbol`
(reusing the #151 span-containment resolver, same innermost-of-nested selection and enclosing
chain) and returns that symbol's same trust-separated cross-domain bundle as `query context` —
`source_facts`, `observations`, `project_state`, `artifacts`, `verification_evidence`, and
`unresolved` — anchored on the located record ID so same-name symbols never bleed in. Absence is
always typed, never a guess: a line in a gap is `no_enclosing_symbol` (exit 2), a line beyond the
file's last recorded structural span is `line_out_of_range` (exit 2, carrying `max_known_line` —
`File` nodes store no line count, so the recorded extent is the deterministic upper bound), and
an unknown path is `no_match` (exit 2). Both `--at <commit>` and `--as-of <instant>` temporal
pins select which symbol is located (the bundle itself is not temporally filtered, matching
`query context`); an unscoped cross-repository path collision fails closed (`ambiguous_repository`,
exit 1). Read-only over `--graph`/`--data-dir`; output (JSON or `--format text`) is deterministic
and byte-identical across runs. See `docs/cli/query.md`.

Pre-ingest referential-integrity validation (issue #103):

```powershell
# Gate a graph JSONL between scan and ingest
cargo run -- validate graph.jsonl                 # exit 0 clean, 1 defects, 2 load error
cargo run -- validate graph.jsonl --format text   # human-readable one-line-per-defect form
```

`eg validate` runs one read-only, offline pass asserting a graph JSONL (from `scan` or
`scan-history`) is referentially closed: every edge endpoint resolves to a present node,
every `DEFINES`/`CONTAINS`/`CALLS`/`IMPORTS`/`MENTIONS` edge targets an allowed node kind,
no tombstoned-and-unsuperseded record is still referenced by a live edge, and no topology
node is orphaned. Zero defects exit 0; any defect exits 1 with one machine-readable JSONL
diagnostic per defect in deterministic canonical order (byte-identical across runs). Output
is redaction-safe — record IDs, categories, relation labels, paths, spans, and counts only.
Structural reference closure only: never parse correctness, semantic accuracy, schema-version
compatibility, or extraction completeness. See `docs/cli/validate.md`.
`eg query public-api-deltas <base> <head>` classifies changes to the externally-reachable
public API surface between two commit handles from a `scan-history` graph or embedded store,
composing the issue #118 range mechanics with the issue #124 visibility/signature capture.
Exported-symbol changes land in a closed change-class set (`added`, `removed`,
`signature_changed`, `visibility_narrowed`, `visibility_widened`); removal, signature change,
and narrowing carry `potentially_breaking: true` — a review flag, never a semver or breakage
claim. Non-exported symbol deltas never appear as public-API changes; they are tallied and
listed only with `--include-internal` in a group labeled `internal_not_public_surface`.
`--callers` joins base-endpoint caller leads onto removed/signature-changed rows. Rows carry
stable record IDs, file/span handles (base-side tombstone handles for removals), the
introducing commit with valid time, and before/after visibility/signature surface text.
Output (JSON or `--format text`) is deterministic and byte-identical across runs.
See `docs/cli/public-api-deltas.md`.

Record retraction (issue #231):

```powershell
# Retract one persisted agent-authored or sensitive record by stable handle
cargo run -- forget agent_memory:v1:<hex> --data-dir .egregore --reason "leaked customer name"   # exit 0
cargo run -- forget agent_memory:v1:<hex> --data-dir .egregore --reason "anything"               # exit 0 (already_retracted no-op)
cargo run -- forget codegraph:v5:<hex> --data-dir .egregore --reason "wrong"    # exit 1 (deterministic_code_fact)
cargo run -- forget agent_memory:v1:missing --data-dir .egregore --reason "x"   # exit 2 (not_found)
```

`eg forget` logically retracts one record from every transaction-time-current read surface
(structural, semantic/vector, context, task, memory, audit, failures, changes, inspect, the
MCP tools, and the daemon's record lookups — direct `GET /v1/records/{id}` and the bulk
`GET /v1/records` serving view) by writing a citable `Retraction` event —
actor, transaction time, redacted reason, prior record handle — plus a tombstone in the
target's domain, through the ordinary
adapter boundary. Deterministic code-graph facts are refused with a machine-readable error
naming `eg refresh`/re-scan; derived semantic measurements (`SemanticDrift` and its edges)
are refused the same way naming re-scan/re-ingest, as is any other commit-anchored temporal
node (`temporal_record`) a tombstone could never suppress; tombstones and retraction events
are also refused. Citing
records survive with their link reported as a `stale_evidence_target` diagnostic, historical
transaction-time views predating the retraction still see the record (bi-temporal honesty),
and re-running on an already-retracted handle is a no-op success returning the original
event. With a pinned `--transaction-time` the envelope is deterministic and byte-identical
across runs. See `docs/cli/forget.md`.
`eg query undocumented` lists externally-reachable public symbols whose captured doc-comment
fact (issue #124) is absent, by joining the issue #213 public surface with the recorded doc
facts — never a `pub` grep and never a rustdoc build. Any doc form (`///`, `/** */`,
`#[doc = "..."]`) excludes a symbol; a plain `//` comment does not. Each row carries a stable
record ID, a repo-relative file/span handle, and the concrete evidence asserted
(`externally_reachable`, `doc_comment_absent`). Re-export rows are attributed to the `pub use`
site with the checked target cited; a doc comment at either the re-export site or the target
counts as documentation. `--include-private` widens to a whole-crate doc audit;
`--limit` truncates deterministically with a diagnostic. The lane asserts doc presence/absence
only — never doc quality — and a pre-#124 store yields an explicit `doc_facts_unavailable`
capability verdict instead of treating every symbol as undocumented. Zero undocumented symbols
is an explicit success (exit 0, `no_undocumented_items` diagnostic; when unresolved re-exports
or missing doc capture leave blind spots, `empty_result_with_blind_spots` instead — never a
certified-clean claim). Output is deterministic and byte-identical across runs.
See `docs/cli/undocumented.md`.

`eg query ownership [path]` aggregates the issue #116 author-attributed history into a
per-file ownership and bus-factor map: for every indexed source file present at the resolved
anchor commit (repository head, `--at <sha>`, or `--as-of <rfc3339>`), a ranked author list
with distinct in-scope commit counts and ownership shares, a primary owner (max share; ties
break to the lexicographically smallest `(author_email, author_name)` identity), and the bus
factor — the minimum number of top authors whose cumulative share reaches `--threshold`
percent (default 50, integer arithmetic). Rows carry the `File` node's stable record ID and
are ordered most-concentrated-first; `--limit` (default 100, max 1000) truncates with an
explicit `truncated` signal. Rows are empirical history-derived leads, never declared
ownership, review authority, or expertise; CODEOWNERS is never consulted. `author_email` is
redaction-eligible PII per `docs/schema/redaction.md`: raw at rest in a local store, a stable
`<REDACTED:email:hash_prefix>` marker in redaction-on exports. Unknown paths, missing/ambiguous
commits, malformed timestamps, and out-of-range threshold/limit fail with stable
machine-readable diagnostics (deltas-style exit codes); output is deterministic and
byte-identical across runs. See `docs/cli/ownership.md`.
`eg query unwrap-expect` inventories `.unwrap()` / `.expect()` panic-risk method-call sites
detected over the Tree-sitter AST (never text in comments, strings, or doc comments). Each row
carries a closed category (`unwrap` / `expect`), a `production` vs `test` context, the stable
record ID, the repo-relative file/span handle, and the enclosing symbol handle (explicit `null`
when top-level). Rows are advisory triage leads from deterministic extractor facts, never
verdicts. Accepts `--path` (subsystem prefix), `--repo`, and `--at <commit>` (valid-time pin).
An empty scope reports `no_sites_in_scope`; an out-of-store scope is `scope_not_found` (exit 2).
The method set is closed for this slice. See `docs/cli/unwrap-expect.md`.

`eg query debt-markers` inventories human-authored `TODO` / `FIXME` / `HACK` / `XXX`
debt-comment markers detected inside Tree-sitter comment nodes (never text in string or
character literals, never identifier substrings). Each row carries a closed lowercase
category, the trimmed single-line note text, the stable record ID, the repo-relative
file/span handle, and the enclosing symbol handle (explicit `null` at module top level).
Rows are advisory triage leads from deterministic extractor facts, never verdicts. Accepts
`--path` (subsystem prefix), `--repo`, and `--at <commit>` (valid-time pin). An empty scope
reports `no_markers_in_scope`; an out-of-store scope is `scope_not_found` (exit 2). The
marker set is closed for this slice. See `docs/cli/debt-markers.md`.

`eg query unreferenced` lists code symbols with zero recorded inbound reference edges
(`CALLS`/`IMPORTS`/`MENTIONS`, plus the extractor's `REFERENCES` and `IMPLEMENTS` usage
edges) as prune-triage candidates. The structural `DEFINES`/`CONTAINS` edge from a symbol's
own file or module never counts. Rows are leads, never proof of dead code — public API
consumed elsewhere, trait dispatch, macro-generated call sites, FFI, derives, and entry
points are documented false-positive classes. Candidates in `Diagnostic`-marked file scopes
carry an advisory extraction-completeness caveat (issue #87). Tombstoned symbols are
excluded; an empty candidate set is an explicit success (exit 0 with a `no_candidates`
diagnostic, distinct from `no_symbols`). Output is deterministic, byte-identical across
runs, and sorted by path, start line, then record ID. See `docs/cli/unreferenced.md`.

`eg query manifest-deps` lists every directly-declared Cargo dependency captured at scan time from
`[dependencies]`, `[dev-dependencies]`, and `[build-dependencies]` as citable
`DependencyDeclaration` facts: crate name (plus the `declared_as` manifest key for
`package = "…"` rename pairs, which are never collapsed), kind (`normal`/`dev`/`build`), the
declared version requirement as written, the resolved version from the nearest `Cargo.lock`
(a parseable declared requirement gates every path, including a stale sole locked version;
markers `locked` / `no_lockfile` / `not_in_lockfile` / `ambiguous_in_lockfile` /
`requirement_unsatisfied_in_lockfile` / `lockfile_unreadable` — never a guessed version, and
never a fallback past an invalid nearest lockfile), the
declaring package, and the repo-relative manifest handle. `--name <crate>` answers the direct
"do we depend on X?" lookup; rows carry their owning repository label and `--repo <selector>`
scopes a shared multi-repo store. Manifests are parsed with a real TOML parser; `cargo metadata`
is never invoked and the scan stays read-only. An empty surface or a name miss is a machine-
readable success (exit 0 with a stable diagnostic). Output (JSON or `--format text`) is
deterministic and byte-identical across runs. Rows are declaration facts, never usage or build
proof. See `docs/cli/manifest-deps.md`.

`eg query churn` ranks Git-tracked files by descending count of distinct commits that
modified them across a `scan-history` temporal store. Every row carries the stable `File`
record ID, the repo-relative path, the integer commit count, and the inclusive commit range
used. Ordering is deterministic (ties break on repo-relative path) and byte-identical across
runs; untracked or ignored paths never appear. The answer states explicitly whether `--limit`
truncated it. See `docs/cli/churn.md`.

`eg query producer-drift` audits stored producer identity against the running binary
(issue #234): every code-graph record whose recorded `egregore_version` and/or grammar
component versions differ from the current extractor is flagged, grouped by the distinct
producer signature `(producer_kind, egregore_version, component set)`, with per-field
mismatches and per-record file/span handles. Only code-graph-extraction producers
(`code_graph_extractor`, `history_replay`, `incremental_cache`) are compared; agent-memory
and importer producers land in a never-flagged `non_code_producer` bucket and
`legacy_pre_v1` records keep their own bucket, never merged. A single-binary store yields
an explicit empty drift result (`no_drift` diagnostic, exit 0) — zero false positives.
Read-only: reports what re-extraction could change, never re-extracts or mutates the
store, and drift never changes the exit code. Output (JSON or `--format text`) is
deterministic and byte-identical across runs. See `docs/cli/producer-drift.md`.

`eg query cycles [scope]` enumerates dependency cycles among files over the already-extracted
edges: resolved cross-file `CALLS` edges (issues #152/#134) and import declarations that
name-resolve to exactly one in-repo defining file. `ambiguous`/`unresolved`/unlabeled
cross-file CALLS edges and ambiguous imports are excluded from cycle detection and tallied —
ambiguity never fabricates a cycle and an unlabeled edge is never treated as resolved.
Import name resolution is Rust-only in this slice; non-Rust imports are excluded and tallied
with a diagnostic, never silently reported acyclic.
Each cycle lists the ordered member files closing the loop, with stable record IDs,
repo-relative handles, and the citable records behind every closing edge. Cycles are
canonical (rotated to the lexicographically smallest member, reported once, sorted by a
stable key) and byte-identical across runs. The optional scope handle (symbol name/record ID
or file path) filters to cycles through that node — the pre-refactor check. An acyclic graph
is an explicit success (exit 0 with an `acyclic` diagnostic), not an error.
See `docs/cli/cycles.md`.

Protected raw-artifact commands (issue #60):

```powershell
# Preview (no writes) — compute hashes and handles only
cargo run -- protected capture --manifest evidence.jsonl --store .egregore/protected

# Enabled — store raw bytes for each entry
cargo run -- protected capture --manifest evidence.jsonl --store .egregore/protected --protected-raw-artifacts --producer op-1

# Retrieve by handle (verifies BLAKE3 hash before returning bytes)
cargo run -- protected get protected:v1:<hex> --store .egregore/protected --operator op-1 --out retrieved.txt

# List metadata (no raw bytes)
cargo run -- protected list --store .egregore/protected
```

`eg protected capture` captures raw transcript text, command output, patch bytes, task/issue
narratives, and generated reports into a local content-addressed store that the graph, query,
and semantic surfaces never read. Disabled by default; enabled with `--protected-raw-artifacts`.
After sources are moved or deleted, payloads remain retrievable by stable handle with BLAKE3
hash verification. See `docs/cli/protected-artifacts.md` for the full operator workflow.

Citation-completeness audit (issue #65):

```powershell
# Gate over a local JSONL graph (semantic reported disabled without embeddings)
cargo run -- audit citations --graph graph.jsonl        # exit 0 pass, 1 gate fail, 2 load error

# Gate over an embedded store (drives the semantic workflow too)
cargo run -- audit citations --data-dir .egregore --min-code-citation 0.95
```

`eg audit citations` drives every public query workflow over a seeded local record set and
reports, per workflow and overall, whether returned rows carry the citation handles their
trust class requires. The default gate fails when fewer than 95% of code-answer rows carry a
stable record ID plus a repo-relative file/span handle (or a documented absent-span reason),
or when any non-code trust-class row lacks a source/evidence/policy handle. Output is
deterministic and redaction-safe — record IDs, handles, hashes, markers, and counts only,
never raw payloads. See `docs/cli/citation-audit.md`.

Query-answer token-cost gate (issue #84):

```powershell
# Measure eg answer token cost vs the ripgrep baseline over a pinned corpus
cargo run -- audit token-cost                                  # exit 0 pass, 1 gate fail, 2 load error
cargo run -- audit token-cost --min-ratio 5                    # stricter savings floor
```

`eg audit token-cost` measures, per question class (exact-symbol, file-defines,
semantic) and in aggregate, the baseline-to-Egregore token-savings ratio with raw counts,
using one pinned deterministic token-count method (`word-punct-v1`) on both sides. The grep
baseline is computed in-process (no ripgrep dependency, no network). Correctness is held
constant: an answer counts only if it carries the expected record ID plus a repo-relative
file/span or commit handle; an uncited answer is a miss, not a win. A class below threshold
yields a distinct exit code and a `below_token_savings_threshold` diagnostic naming the class
and ratio. Output is deterministic and redaction-safe. See `docs/cli/token-cost.md`.

SOC2 control-catalog loader/validator/pin (issue #337):

```powershell
cargo run -- audit control-catalog                              # exit 0 valid, 2 load/parse error
cargo run -- audit control-catalog --catalog path/to/soc2.json  # validate a catalog file
cargo run -- audit control-catalog --format text                # human-readable summary
```

`eg audit control-catalog` loads, validates, and BLAKE3 hash-pins a versioned
SOC2 control→evidence-class catalog (the embedded `soc2-v1` by default, or a
`--catalog` override). It checks the schema-version tuple
`(control_catalog, ControlCatalog, 1)` and the closed 11-value evidence-class
vocabulary, then prints a deterministic single-line report carrying the catalog
identity, its `control_catalog:v1:<hex>` hash-pin handle, and the per-control
evidence-class map (`required`/`optional`). Pure, offline, read-only;
byte-identical across runs. Unknown schema version, unknown evidence class,
malformed JSON, and unreadable files exit 2 with a redaction-safe JSON error.
The catalog maps a control ID to Egregore evidence classes — not an
interpretation of the AICPA criteria and not legal advice; evidence of process
execution, never proof of control effectiveness. This is the catalog surface
only; evidence-pack assembly is #338. See `docs/cli/control-catalog.md` and
`docs/controls/README.md`.

Control-scoped, time-windowed evidence-pack assembly + verify (issue #338):

```powershell
cargo run -- audit evidence-pack assemble --control CC8.1 \
  --from 2026-03-01T00:00:00Z --to 2026-04-01T00:00:00Z --graph history.graph.jsonl
cargo run -- audit evidence-pack assemble --control CC8.1 \
  --from <T0> --to <T1> --data-dir .egregore --min-review-coverage 0.8
cargo run -- audit evidence-pack verify pack.json   # exit 0 clean, 1 defect, 2 load error
```

`eg audit evidence-pack assemble` builds a deterministic, redaction-safe pack
scoped to one control and one half-open valid-time window (`from <= t < to`),
composing the #337 catalog (id/version/`control_catalog:v1:` hash pin echoed in
the manifest), #68 bundle scrub-to-hash + integrity/coverage/safety verify
mechanics, #65 citation classification (reused byte-for-byte for the citation
gate), #118/#157 section-level delta disclaimers, #103 pre-ingest validation,
#60 protected handles (raw bytes never enter), and the #333 PR head/base/merge
SHA fields. Every catalog class the control maps becomes a section, always
present even when empty; class availability drives `evaluate_requirement`'s
three-way outcome, so a required class with no records anywhere gate-fails
(`required_class_unavailable`) while an optional one degrades to
`{"status":"unavailable","unavailable_reason":...}` +
`evidence_class_unavailable` (log-domain classes report `log_domain_absent`).
Per-record valid time resolves `temporal.valid_time -> node valid_time ->
executed_at`; a class-relevant record with no resolvable valid time is excluded
under a counted `missing_valid_time` diagnostic. The closed `gaps` enum is
`{merged_pr_without_approving_review, approval_precedes_final_head,
review_unanchored_no_commit_sha, commit_outside_any_pr, missing_valid_time}`;
each row cites its source record IDs. Because issue #334 (the `review_commit_sha`
field / `REVIEWS_COMMIT` edge) is not yet merged, the two #334-dependent gap
classes detect their backing facts at runtime and, when absent, emit a single
`capability_unavailable` diagnostic naming #334 with zero rows — never
fabricated, and wired to populate once #334 lands. Exit 0 when every verdict
passes (an empty window is a vacuous success), 1 on any verdict failure (report
still printed) including review coverage below `--min-review-coverage` (default
1.0), 2 on usage/load errors (`unknown_control` naming the known IDs,
`reversed_window`, `invalid_timestamp`, both/neither input flag, unreadable
store/graph/catalog). Output is byte-identical across runs and across `--graph`
vs `--data-dir`, allow-list only (IDs, handles, hashes, bounded labels, valid
times, counts — never bodies), reads no wall clock unless `--captured-at` is
pinned, and carries this verbatim manifest disclaimer: "rows are recorded
observations of process execution as imported; never proof of control
effectiveness, compliance, or completeness; absence of a record means no
imported evidence, not no event; not an auditor opinion". `eg audit
evidence-pack verify <path>` re-checks a pack offline (Integrity, Coverage,
Safety, Window-consistency). Use `eg bundle export` instead when you want a
record-closure-scoped bundle rather than a control/window-scoped pack. See
`docs/cli/evidence-pack.md`.

`eg audit review-coverage --from <T0> --to <T1>` (over `--graph` XOR `--data-dir`,
issue #339) is a standing citable review-coverage gate: for every PR merged in the
half-open valid-time window `[from, to)` keyed on `merged_at` it classifies the PR
into the CLOSED verdict set `{covered, approval_stale_head, self_approved_only,
uncovered}` and gates on `covered / merged_prs` against `--min-coverage` (default
1.0). Its per-PR derivation is the SINGLE shared implementation
(`evidence_pack::derive_review_coverage`) that #338's evidence-pack
`review_coverage` section and `merged_pr_without_approving_review` gap also call —
the pack with lenient options, this lane with its strict `--require-non-author` /
`--require-final-head` defaults (both on) — so the two surfaces never diverge.
Substrate: #333 PR fields (`merged_at`/`head_sha`/`merge_commit_sha`/
`system_native_id`), #334 review anchors (`review_commit_sha`), imported by #46;
#335 identity nodes are NOT merged, so the non-author check compares recorded
author LOGINS and degrades to an `identity_unavailable` sub-label without them,
never fabricating a self-approval and never inventing an `ExternalIdentity` kind.
A stale approval degrades to `approval_stale_head` only under `--require-final-head`;
a review lacking a `review_commit_sha` anchor degrades to an `approval_unanchored`
sub-label, never guessed stale; an anchored approval whose PR carries no `head_sha`
cannot confirm the final head, so it degrades to `approval_stale_head` +
`head_sha_unavailable` rather than silently counting `covered`. Every row cites the PR Task ID + `system_native_id`
+ `merge_commit_sha`; covered rows also cite the approving Review ID +
`review_commit_sha` + approver login. Windowing reuses #338's half-open
`merged_at` semantics; a PR with no window-resolvable merge time is excluded under
a counted diagnostic and an empty window is a vacuous pass (`empty_window`, exit 0).
Below threshold exits 1 with the full report and a
`below_review_coverage_threshold` diagnostic naming the ratio and failing PR IDs;
usage/load errors (reversed window, invalid timestamp, both-or-neither input flag,
out-of-range `--min-coverage`) exit 2. Measures RECORDED review execution — never
GitHub branch-protection configuration, review quality, or unrecorded reviews
elsewhere. Pure core, allow-list-only output, byte-identical across runs. Also
wires the two #334-dependent evidence-pack gap classes
(`review_unanchored_no_commit_sha`, `approval_precedes_final_head`) to their real
derivation now that #334 is merged, emitting the `capability_unavailable`
diagnostic only for a pre-#334 store with no reviewed-commit facts. See
`docs/cli/review-coverage.md`.

`eg import github <owner>/<repo>` preserves review-state HISTORY so a dismissal
never erases an approval (issue #336). For each PR whose `/pulls` list entry
changed — the SAME `pulls_changed` trigger that drives the per-PR review
summaries — the importer also fetches `GET /repos/{o}/{r}/issues/{n}/timeline`,
filtered to the closed transition kinds `{review_dismissed, review_requested,
review_request_removed}` (every other timeline kind skipped), and mints one
append-only `ReviewStateTransition` node per event. A `review_dismissed` event
also mints a `TRANSITIONS_REVIEW` edge (ReviewStateTransition → Review) to the
dismissed review, reconstructed from `dismissed_review.review_id`;
`review_requested`/`review_request_removed` stand alone with no edge. Each
transition is keyed on the timeline event's OWN server-native id
(`project_stable_id(["project","ReviewStateTransition", repo, number,
"timeline:<event_id>"])`), so it NEVER participates in the parent `Review`'s
identity and is byte-stable across re-imports. Epistemic contract:
`Review.review_state` stays a **last-write-wins current-state summary** (a
dismissal overwrites `approved`→`dismissed` under the same record id); the
transitions are the **history**. A consumer needing "review state as of T" (e.g.
#339 review-coverage evaluated retroactively) MUST join the transitions, not read
the summary field. Trust class `project_state`. Raw timeline text never enters the
graph: the dismissal message is redacted via `redact_lines` into a `body_handle`;
event kind, actor login, timestamps, and target review id are plaintext. A
KNOWN-kind event that cannot be turned into a citable transition (no actor, no
event id, or a dismissal with no `dismissed_review`) emits a
`github_timeline_event_unparseable` `Diagnostic`, never a silent drop. The state
file schema bumps 4→5 (new `timeline_event:<id>` resource-hash keys + timeline
ETag entries), migrate-not-discard from v2/v3/v4. Deterministic, byte-stable,
pull-only, per-named-repo — no polling, no webhooks. See
`docs/cli/github-import.md`, `docs/schema/import-github.md` §1/§6/§8/§9, and
`docs/schema/project-graph.md`.

Embedded store write locking (issue #200):

Every embedded write open takes the OS-level exclusive store lease (`egregored.lock`),
shared with the daemon, so one data dir has exactly one live writer at a time. A second
concurrent writer — embedded peer or live daemon — is refused before any write with the
structured `store_contended` error naming the remedy (route concurrent writers through
`eg daemon start` + `--adapter daemon`, or retry after the current writer releases the
store). `eg ingest --adapter embedded` additionally prints the machine-readable
`{"ok": false, "error": {...}}` envelope on stdout. Read-only commands never take the
write lease; strictly read-only audits read a throwaway snapshot copy. See
`docs/cli/embedded-concurrency.md`.

The primary binary is `egregore`; `eg` is also built as a short CLI alias.

## Working Rules

- Use SPEC-PROOF-RED-GREEN-REFACTOR for implementation work.
- No behavior ships without a test.
- Keep core graph extraction deterministic and filesystem-local.
- Keep deterministic code facts separate from agent-authored observations through typed nodes, provenance, and evidence links.
- History replay must read Git objects without mutating the user's active checkout.
- Prefer Tree-sitter for syntax parsing rather than ad hoc regex parsing.
- Keep AletheiaDB writes behind an adapter so embedded, daemon, SDK, or CLI transports can swap without changing the extractor.
- Use the published `aletheiadb` crate. Do not use a local path dependency.
- Do not depend on `embed_anything` directly. Use AletheiaDB's `embeddings` feature and re-export boundary.
- Keep `.cargo/config.toml`'s MSVC C++ CRT flag unless the upstream tokenizers/esaxx-rs link mismatch is fixed.
- Do not introduce network crawling or remote repository fetching in the MVP.

## Expected Verification

Run these before claiming implementation work is done:

```powershell
cargo fmt --all
cargo test --all-targets
cargo test --all-targets --no-default-features
cargo test --all-targets --features nova
cargo test --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
```

For ingestion work, also test against a temporary AletheiaDB data dir before touching the shared memory store.
