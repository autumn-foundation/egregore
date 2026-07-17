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
cargo run -- query semantic "request timeout handling" --data-dir .egregore --under src/daemon
```

`eg query semantic <query> --under <prefix>` scopes semantic code search to a
repo-relative directory prefix (issue #198), reusing the #83 SEGMENT-AWARE prefix
matcher so `src/alpha` matches `src/alpha/foo.rs` but never `src/alphabet/x.rs`
(the trailing-slash and bare forms resolve identically). The scope filters the
full candidate pool BEFORE the `--limit` top-N cap, so `--limit N` returns the N
best IN-SUBSYSTEM hits, not N global hits filtered down to fewer; it composes
with `--repo` and `--limit`, and each row keeps the stable
`record_id`/`score`/`name`/`repo_relative_path`/`span` contract. Output is
deterministic (byte-identical across runs on an unchanged store). A
malformed/empty prefix exits 1 with a machine-readable `malformed_under_prefix`
diagnostic on stdout; a valid prefix matching zero embedded nodes exits 2 with a
"scoped, no matches" stderr message worded distinctly from the "no semantic
index" message. `--under` is a local-CLI surface and cannot be combined with
`--daemon`. See `docs/cli/query.md`.

`eg scan-logs <log> --repo-path <repo>` extracts runtime log signatures from one
captured log file (issues #319/#320): a `LogSource`, one `ErrorSignature` per
`template-v1` fingerprint (a 1000×-repeated error collapses to one signature with
`occurrence_count` == the raw count), capped `LogEvent` exemplars, and hourly
`LogOccurrenceBucket` counts. Trust class `runtime_observation` — a program's own
claim, deterministically parsed but never verified. Records mint under the
`log:v3:` prefix (`LOG_SCHEMA_VERSION` == 3): every log record now carries a
retrievable `repository_id` field (issue #362, = the computed repository identity
a `--repo` selector resolves to), and each `LogOccurrenceBucket` also carries a
sorted `occurrence_timestamps` list (issue #364, RFC3339 UTC per-occurrence valid
times, `len == occurrence_count`). Both are `#[serde(default)]` so legacy
`log:v2:` records still deserialize. Raw log text never enters the graph;
excerpts are `template-v1`-normalized, redacted, and bounded. Deterministic
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

`eg scan` makes file-level indexing coverage a stated, deterministic, queryable
fact (issue #135): one `ScanCoverage` node rides in the JSONL (attached to its
`Repository` by a `CONTAINS` edge, so it is citable, non-orphan, and validates
under `eg validate`) carrying `files_walked`, `files_indexed`,
`skipped_by_extension` (a sorted per-lowercased-extension tally, `""` for
no-extension), the named indexed-language scope, and `coverage_complete`.
`files_indexed` counts every walked file that received a graph `File` node — from
source-symbol extraction OR from a non-source File producer such as
manifest/dependency extraction (#180), so a tracked `Cargo.toml` that declares
dependencies is counted INDEXED (it gets a `File` node + queryable
`DependencyDeclaration` facts), never mislabeled under `skipped_by_extension`;
the tally is finalized (`reconcile_scan_coverage`) after all File-producing
extractors run, so any future non-source File producer is covered too. A
`Cargo.toml` with no dependencies mints no `File` node and stays honestly
skipped under `toml`. `indexed_languages` names the parsed SOURCE-language scope
specifically (manifests are not a "language" and add no entry). On a Git working
tree `eg scan` also prints a human-readable summary to stderr
(`scan coverage: N files walked, M indexed, K skipped` + skipped-by-extension +
`indexed languages: Rust, Python, TypeScript, Go`). The named scope is the REAL
4-language extractor capability (Rust/Python/TypeScript/Go), derived from
`languages::Language::ALL` — the issue's "Rust only" text was stale. Excluded
directories (`.git`, `target`, nested Git worktrees) are never walked, so they
never dilute the counts (AC7); when `coverage_complete` is true,
`files_indexed + sum(skipped_by_extension) == files_walked` (AC4). The non-Git
filesystem-walk fallback has no walked/skipped denominator, so it reports
best-effort counts with `coverage_complete: false` and suppresses the stderr
summary rather than fabricate one. `eg inspect` surfaces the same coverage block
over both `--graph` and `--data-dir`, so an agent can tell "0 results because
absent" from "0 results because that file was never indexed". Over `--graph`,
equal-ID `ScanCoverage` versions in an exported JSONL are collapsed to the
freshest by `valid_time` instant; because the full scan stamps seconds precision
and the record carries no subsecond/write-order signal, two scans within the
same UTC second cannot be ordered from the file (tiebreak is arbitrary w.r.t.
recency) — use `--data-dir` (authoritative, ordered by store write-order) for
sub-second re-scan scenarios (known limitation, issue #406). The
`codegraph` `SCHEMA_VERSION` is 6 (bumped 5→6 for the `ScanCoverage` node). `eg
refresh` (the incremental path) maintains this same node (issue #403): each
refresh recomputes coverage against the current tree — running manifest
dependency extraction first, exactly like a full scan, so a dependency-declaring
`Cargo.toml` stays `files_indexed` and is never misclassified as skipped `toml`
— and re-emits the repo-keyed `ScanCoverage`, superseding the prior version in
the store, so `eg inspect --data-dir` never reports stale
`files_walked`/`files_indexed` after files are added or removed. See
`docs/cli/scan.md` and `docs/cli/inspect.md`.

`eg export --data-dir <dir> --out <file.jsonl>` dumps every persisted record of
an embedded store back to canonical JSONL — the inverse of `eg ingest`, distinct
from the #68 evidence bundle (no header, manifest, run id, or timestamp). It
reads the same physical inventory `eg inspect --data-dir` uses
(`inspect_all_records` through the throwaway read-only copy → strictly
read-only) and emits nodes, edges, valid-time tombstones, diagnostics,
superseded versions, and unknown-version records across all domains in the exact
shapes `scan`/`ingest` emit, so re-ingesting the export into a fresh `--data-dir`
reproduces the `eg inspect` totals and per-domain/kind/schema-version counts
(parity holds for stores without retractions). Lines are sorted and `\n`-joined
as `Graph::to_jsonl` does, so repeated exports of an unchanged store are
byte-identical. The one deliberate deviation from "every physical record": a
record hidden by `eg forget` (#231) is never re-emitted (every physical version
of the retracted id is dropped, fail-closed on privacy), while its `Retraction`
event and tombstone audit trail are preserved — so a re-ingested forget-export
is `eg validate`-clean. Unknown `(domain, kind, schema_version)` records (only
bumped versions of known kinds, since ingest rejects unknown kinds at the write
path) re-emit their reconstructed canonical line verbatim; a record that cannot
be reconstructed (a required prop absent — only reachable via artificial raw
injection) is surfaced as an enumerated stderr skip diagnostic, the single
documented lossless exception. Redacted records carry as-is (protected raw
payloads stay `protected:v1:` handles/hashes, never rehydrated). A missing,
empty, unreadable, or record-empty `--data-dir` fails with a diagnostic naming
the path and writes no output file. See `docs/cli/export.md`.

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

# Directed shortest call path between two symbols with a citable witness (issue #225)
cargo run -- query path <from_symbol> <to_symbol> --graph graph.jsonl        # exit 0 (path_found)
cargo run -- query path <to_symbol> <from_symbol> --graph graph.jsonl        # exit 2 (no_path; direction honored)
cargo run -- query path <ambiguous_name> <to_symbol> --graph graph.jsonl     # exit 1 (candidates listed)
cargo run -- query path does_not_exist <to_symbol> --graph graph.jsonl       # exit 2 (no_match)
cargo run -- query path <symbol> <symbol> --graph graph.jsonl                # exit 0 (trivial zero-hop path)
cargo run -- query path <from_symbol> <to_symbol> --graph history.graph.jsonl --at <sha>  # commit view

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

# One error signature's full cross-domain context bundle (issue #324)
cargo run -- query error-context log:v2:<hex> --graph combined.graph.jsonl        # exit 0 (record ID)
cargo run -- query error-context <hex_prefix> --graph combined.graph.jsonl        # exit 0 unique / exit 1 ambiguous
cargo run -- query error-context <symbol_name> --graph combined.graph.jsonl       # exit 0 (frames resolved to it)
cargo run -- query error-context <handle> --graph combined.graph.jsonl --at <sha> # re-resolve frames at a commit view
cargo run -- query error-context <handle> --graph combined.graph.jsonl --as-of <instant>  # bound occurrence view
cargo run -- query error-context <handle> --graph g.jsonl --protected-store .egregore/protected  # read-time protected join
cargo run -- query error-context does_not_exist --graph combined.graph.jsonl      # exit 2 (no_match)

# One cross-domain evidence witness path between two record handles (issue #247)
cargo run -- query evidence-path <source_id> <target_id> --graph graph.jsonl   # exit 0 on a witness path
cargo run -- query evidence-path <source_id> <target_id> --data-dir .egregore  # embedded, read-only
cargo run -- query evidence-path <id> <id> --graph graph.jsonl                 # exit 1 (identical_endpoints)
cargo run -- query evidence-path <live_a> <live_b> --graph graph.jsonl         # exit 1 (no_path) when disconnected
cargo run -- query evidence-path <missing> <target> --graph graph.jsonl        # exit 2 (endpoint_not_found)
cargo run -- query evidence-path <tombstoned> <target> --graph graph.jsonl     # exit 2 (endpoint_tombstoned)

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

# Symbol dormancy triage: least-recent last change first (issue #219)
cargo run -- query recency --graph history.graph.jsonl             # exit 0, most dormant first
cargo run -- query recency --graph history.graph.jsonl --limit 10  # cap output (default 50, max 500)
cargo run -- query recency --graph graph.jsonl                     # exit 2 (no_history: current-tree scan)

# Producer-identity drift audit against the current binary (issue #234)
cargo run -- query producer-drift --graph graph.jsonl                  # exit 0 (even with drift)
cargo run -- query producer-drift --data-dir .egregore --repo acme/widget  # scope one repo; bad selector exits 1
cargo run -- query producer-drift --graph graph.jsonl --format text

# Dependency cycles among files over IMPORTS/CALLS edges (issue #138)
cargo run -- query cycles --graph graph.jsonl                     # exit 0 (even when acyclic)
cargo run -- query cycles src/lib.rs --graph graph.jsonl          # only cycles through this node
cargo run -- query cycles does_not_exist --graph graph.jsonl      # exit 2 (no_match)
cargo run -- query cycles codegraph:v1:zzz --graph graph.jsonl    # exit 1 (Unsupported)

# Verification-coverage of the public API surface — covered/uncovered (issue #109)
cargo run -- query verification-coverage --graph graph.jsonl              # exit 0 (capability verdict when no links)
cargo run -- query verification-coverage src/adapters --graph graph.jsonl # scope to a path prefix
cargo run -- query verification-coverage my_symbol --graph graph.jsonl    # scope to a symbol name
cargo run -- query verification-coverage --graph graph.jsonl --limit 50   # per-bucket truncation
cargo run -- query verification-coverage src/nope --graph graph.jsonl     # exit 2 (scope_not_found)
```

`eg query subsystem <prefix>` returns code facts, agent observations, project state, artifacts,
verification evidence, semantic drift, and runtime error signatures for everything under a
repo-relative directory prefix. Path matching is segment-aware: `src/alpha` never bleeds into
`src/alphabet`. Output is a deterministic JSON envelope with trust-separated sections. The
always-present `log_signatures` section (issue #325) lists `ErrorSignature` records (from
`scan-logs`/`resolve-frames`) whose frames resolve under the prefix: a signature appears iff a
`FRAME_RESOLVES_TO` edge labeled `resolved` or `path_only` targets an in-prefix Symbol/File;
`ambiguous`/`unresolved`-only signatures never appear and their dangling targets fall through to
the existing `unresolved` section. Rows carry `trust_class: "runtime_observation"` — runtime
LEADS, not proof — with a bounded redacted `template_excerpt` (never raw log text); an empty
section means "no scanned source resolved here", not "no errors exist". `occurrence_count`
reflects scanned sources only (#361), and `--graph` coalesces cross-scan duplicate-ID signatures
while `--data-dir` retains one record per stable log ID (#363).

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

`eg query path <from> <to>` traces the directed shortest call path between two symbols
(record ID or exact name) and returns one concrete, citable witness: a summary envelope
line followed by one line per hop, each hop citing the from/to `record_id`, `name`, `kind`,
`repo_relative_path`, and `span` plus the `CALLS` edge handle, `resolution`, and
`confidence`. The walk is a directed BFS over `resolved` CALLS edges ONLY — the outbound
call direction — so ambiguous/unresolved CALLS edges (issues #152/#134) and every other
label (`REFERENCES`/`MENTIONS`/`IMPORTS`/`IMPLEMENTS`/containment) are excluded; the
always-present disclaimer states this and that the witness is a reachability LEAD, never
proof of runtime control flow, and a `no_path` verdict is not proof of non-reachability.
Direction is honored (`path A B` and `path B A` are distinct queries); `A == B` is a
trivial `path_found` zero-hop path. Selection is deterministic and byte-stable: among
minimum-hop paths, each node keeps the discovery edge with the lexicographically smallest
`(source_record_id, edge_record_id)` pair as its parent pointer, and the witness is
reconstructed by following those pointers. Both endpoints resolve independently; an
ambiguous name exits 1 listing all candidate record IDs (the `endpoint` field names which
side failed), an unknown/stale endpoint exits 2 (`no_match`/`stale_handle`), and both
endpoints resolving with no directed path is an explicit `no_path` verdict at exit 2.
`--at`/`--as-of` trace a single-commit history view (mutually exclusive); with neither, a
scan-history graph/store is read as the UNION of all commit snapshots (no edge tombstones),
so an edge removed at a later commit can still yield `path_found` — pass `--at <HEAD>` for
HEAD-only semantics (matches `deps`/`transitive-callers`/`transitive-callees`). Read-only over
`--graph`/`--data-dir`, redaction-safe (record IDs, names, paths, spans only), with a
`--format text` mode. See `docs/cli/path.md`.

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
`occurrence_source: aggregate_only` — counts are never fabricated. Since schema v3 (issue #364)
each `LogOccurrenceBucket` carries a sorted `occurrence_timestamps` list, so per-window counts
are ENDPOINT-EXACT: only timestamps at or before the endpoint (by parsed UTC instant) are
counted, so a bucket straddling a mid-hour endpoint is sub-divided at the instant instead of
counted whole. A legacy `log:v2:` bucket has empty `occurrence_timestamps` and falls back
per-bucket to the hour-bucket rule (counted whole when `bucket_start <= endpoint`, may over-count
by up to one bucket width; the "fully-before" predicate is deliberately not used — it would
under-count). The `occurrence_count_granularity` marker is therefore PER-RESPONSE and CONDITIONAL:
`endpoint_exact` when every contributing bucket had timestamps (or none contributed), else
`hourly_bucket` when at least one legacy bucket fell back; the disclaimer wording is selected to
match. Per-window bucket counts
and the aggregate `occurrence_count` both SUM across all scanned sources: since issue #361 a
`LogOccurrenceBucket` record ID is source-aware — `(repository/signature/hour/width/source_id)` —
so distinct sources mint DISTINCT bucket IDs whose per-source counts each sum in, while a genuine
rescan of identical bytes mints the SAME bucket ID and is deduped (collapsed) before summing;
concatenating the identical `scan-logs` output therefore no longer double-counts buckets. All timestamp comparisons
(window derivation, classification, bucket cutoffs) are by parsed UTC instant, never raw RFC
3339 string order, because commit committer dates carry local offsets (`%cI`) while scan-logs
times are Z-normalized — a lexical comparison would misclassify across offsets. Since schema v3
(issue #362) every log record persists a retrievable `repository_id`, so `--repo` now FILTERS log
signatures (not just the code side): `RepositoryIndex::owner_of` resolves each signature's
attribution and a signature belonging to a different repository is soundly EXCLUDED — closing the
cross-repository false-regression lead the old disclosure could only warn about. A legacy `log:v2:`
record has an empty `repository_id` and cannot be proven in-repo, so it is conservatively EXCLUDED
under `--repo` (possible under-report, never a cross-repo bleed) and tallied. The former "logs are
never repository-filtered" `repo_scope_caveat` is GONE; a shrunken RESIDUAL `repo_scope_caveat`
(carrying `repo_scope`, `excluded_unattributed_signature_count`, and a fixed `message`) is emitted
ONLY when `--repo` is set AND at least one legacy unattributed signature was actually excluded — a
fully-v3 scoped store carries NO caveat. The old `distinct_repository_count`/`multi_repository_store`
disclosure fields are removed (attribution makes them unnecessary). Because `LogSource` is a non-identity input (a signature's
stable ID is `(repository_id, fingerprint_algorithm, template, severity)` only), a graph
combining multiple `scan-logs` outputs for one repo carries the same signature record ID more
than once; those records are grouped by stable ID and merged BEFORE classifying — earliest
`first_seen`, latest `last_seen` (by instant), distinct buckets summed across the group (deduped by
source-aware bucket record ID, per #361), aggregate `occurrence_count` summed — so exactly one row per signature ID is emitted, never
split across conflicting classes. Exit codes and the error
taxonomy mirror #118 exactly. Rows are regression LEADS, never proof this range caused the
failure; a ceased signature is not proof of a fix; occurrence data only reflects scanned log
sources. Cross-scan signature coalescing now works on BOTH read paths (#363 landed the
adapter-level retention): the embedded (`--data-dir`) lane loads through the log-retained read
surface (`read_all_records_log_retained`), which surfaces every distinct scan OBSERVATION —
each superseded non-temporal `ErrorSignature`/`LogOccurrenceBucket` version that
differing-content `scan-logs` ingests append, while an enrichment-only rewrite (evidence links
added by `resolve-frames`/`link-logs`, log payload unchanged) is retained as a single
observation and never double-counted (retraction boundary honored — a `forget`-retracted
observation re-observed by a LATER `scan-logs` is not resurrected; only the post-retraction
observation reaches the coalesced sum) — so `first_seen`/`last_seen`/occurrence counts are
reconstructed exactly as on the concatenated `--graph` JSONL. Since issue #361 made bucket identity
source-aware, per-window occurrence counts CONVERGE across `--graph` and `--data-dir` (distinct
sources sum on both, rescans collapse on both). When run over `--data-dir` with log records present,
the envelope still carries an `embedded_log_retention_caveat` disclosing the ONE residual divergence,
rooted purely in idempotent-write dedup of byte-identical non-temporal records (not a bucket-identity
gap): a byte-identical re-ingest of the whole `scan-logs` output is deduped to one physical record on
`--data-dir` but summed on `--graph`, so concatenating identical JSONL inflates the `--graph` aggregate
`occurrence_count` while an identical re-scan does not inflate `--data-dir`.
A single ingest is exact either way. Read-only, redaction-safe (no raw log text), deterministic and
byte-identical across runs. See `docs/cli/log-deltas.md`.

`eg query error-context <handle>` resolves ONE `ErrorSignature` and assembles a single
deterministic, trust-separated cross-domain envelope (issue #324) — one cited answer where an
agent otherwise runs four tools. It is a read-time JOIN that mints no edge and adds no node
kind, edge label, or trust class: the `query context` (#38) cross-domain bundle for the code
half (seeded from each resolved frame target via `record_context`), the #322
`FRAME_RESOLVES_TO` / #323 `EMITTED_DURING`+`REFERENCES_TASK` / #320 `AGGREGATES`+`CAPTURED_FROM`
log-edge topology for the runtime half, and the #118 `range_deltas` mechanics for the history
`first_seen_range`. Handle resolution has three precedence-ordered modes (the `log:v<N>:` prefix
is matched version-agnostically, so both v1 fixtures and real v2 handles resolve): exact
`log:v2:<hex>` record ID; a fingerprint/template-hash hex prefix (unique → resolve; ≥2 → `ambiguous` exit 1
with all candidate IDs; a bare hex prefix matching none falls through to symbol-name mode); and
an exact `Symbol` name whose frames resolved to it (a symbol named by MANY signatures returns
ALL of them, exit 0 — not ambiguity). A well-formed `log:v2:` handle that is not an
`ErrorSignature` (absent, or a `LogSource`/bucket ID) is `no_match` (exit 2), never silently
prefix-matched. Sections are trust-separated with a `trust_class` on every row: `signatures`
(`runtime_observation` — identity + `template_excerpt` + occurrence buckets + resolved frames),
`source_facts` (`source_fact`), `observations` (`agent_observation`, `EMITTED_DURING` runs
carrying their closed `correlation_basis` — `content_hash_join`/`temporal_correlation` — with
overlapping runs each kept, no silent winner), `project_state` (`REFERENCES_TASK` targets),
`artifacts`, `verification_evidence` (`EMITTED_DURING` `CommandRun`s carry their basis too), plus
`unresolved` and `excluded`. Runtime rows live ONLY in `signatures` — zero cross-class leakage
into `source_facts`. `first_seen_range` brackets the earliest signature `first_seen` in the
narrowest commit window (base = newest commit at/before it, head = oldest at/after, ties by
ascending SHA) and joins the reused `range_deltas` symbol groups against the frame targets into
`overlapping_symbol_deltas`; a plain `scan` graph reports `history_unavailable`, never a
fabricated window. `--at <commit>` re-resolves frames against that commit view; `--as-of
<instant>` bounds the occurrence-bucket view (both are mutually exclusive → exit 1
`unsupported_combination`). `--supersession exclude` (default) drops superseded/contradicted
agent rows into `excluded`; `include-but-flag` keeps and flags them. `--repo` scopes only the
code side of the history join (log records carry no retrievable repository attribution).
`--protected-store <dir>` matches each signature's `LogSource` `source_artifact_hash` to a
`protected:v1:` handle at READ time (class + byte length only; raw bytes never read); the graph
must carry zero protected handles or the run exits 1 (`protected_handle_in_graph`). Over
`--data-dir` the current-state (no `--at`/`--as-of`) read loads through the log-retained surface
(#363), so multi-scan signature coalescing (earliest `first_seen`, latest `last_seen`, summed
occurrence, all buckets) is reconstructed exactly as on `--graph`; since issue #361 made bucket
identity source-aware, the occurrence-bucket block CONVERGES across `--graph` and `--data-dir`
(distinct sources sum on both, rescans collapse on both). The envelope carries an
`embedded_log_retention_caveat` disclosing the ONE residual divergence, rooted purely in
idempotent-write dedup of byte-identical non-temporal records (not a bucket-identity gap): a
byte-identical re-ingest of the whole output is deduped to one physical record on `--data-dir`
but summed on `--graph`. All
timestamp ordering is by parsed UTC instant, never raw RFC 3339 string order. Rows are
CORRELATION LEADS, never proof of cause: a resolved frame proves the backtrace NAMES a symbol,
an `EMITTED_DURING` edge is a correlation. Read-only, redaction-safe (only the bounded
`template_excerpt` escapes as free text), deterministic and byte-identical across runs.
See `docs/cli/error-context.md`.

`eg query evidence-path <source_id> <target_id>` traces ONE deterministic cross-domain
evidence witness path between two record handles (issue #247), answering "is record A
grounded in record B, and by what chain?" without hand-walking the graph. It is a read-time
traversal that mints no edge and adds no node kind, edge label, or trust class: the walk
runs over the graph's EVIDENCE/PROVENANCE edge subgraph only. Traversal membership is an
exhaustive compile-time partition of every `EdgeLabel` variant (a `match` with NO wildcard
arm, so a new variant fails to compile until classified — the completeness invariant):
TRAVERSED are the cross-domain grounding edges (`HAS_EVIDENCE`, `OBSERVES`, `VALIDATED_BY`,
`PRODUCED_EVIDENCE`, `FRAME_RESOLVES_TO`, `EMITTED_DURING`, `REFERENCES_TASK`,
`CLOSES_ACCEPTANCE_CRITERION`, `OWNED_BY_TASK`, and the rest of the evidence-link/log/project
registry); EXCLUDED are code-graph topology (`CALLS`, `CONTAINS`, `DEFINES`, `IMPORTS`,
`REFERENCES`, `IMPLEMENTS`, `MENTIONS`, `CHANGED_IN`, `PARENT_OF`, drift edges) and
intra-agent-memory scaffolding (`SESSION_OF`, `AUTHORED_BY`) — code-only CALLS/REFERENCES
call paths are the transitive-callers/callees lanes' job, not this one. Both sorted class
lists ride in the envelope so a `no_path` verdict is never presented as proof no grounding
exists. Reachability is UNDIRECTED (a grounding chain legitimately mixes edge directions, so
each edge is usable either way); each hop reports the edge's native `from`/`to` plus a
`traversal_direction` (`forward`/`reverse`). The path is the deterministic shortest path:
shortest hop count, then the lexicographically smallest path compared as the full ordered
sequence of `(neighbor_record_id, edge_record_id)` steps from the source (a difference at the
first step dominates any later step) — cycles terminate via a visited set. A stable edge ID
re-ingested with changed metadata over an append-only `--graph` resolves to its latest write
(mirroring embedded `latest_edge_versions`), so both transports surface identical edge
basis/confidence. Deleted records (tombstoned and
non-temporal) and edges touching a deleted endpoint are excluded — a current-state view, no
`--at`/`--as-of`. Endpoint/exit taxonomy: a witness path (>=1 hop) exits 0; `source_id ==
target_id` is `identical_endpoints` (exit 1); two live but disconnected endpoints yield a
machine-readable `no_path` verdict (exit 1, never a silent empty list); an absent endpoint is
`endpoint_not_found` and a tombstoned endpoint is `endpoint_tombstoned` (both exit 2,
DISTINCT labels; the source's problem is reported first when both are bad). An `EMITTED_DURING`
hop carries its `basis` (`content_hash_join`/`temporal_correlation`) and documented
`confidence` (`1.0`/`0.5`) — a correlation lead, never causation. The lane is repo-agnostic
(endpoints are exact IDs; a chain may cross repositories), so there is no `--repo` flag.
Read-only (the `--data-dir` path reads a throwaway copy), redaction-safe (only IDs, domains,
kinds, edge labels, paths, spans, counts, and basis strings escape — never raw
source/transcript/command/patch text), deterministic and byte-identical across runs. A witness
path proves a live evidence-edge chain connects two records; it is NOT proof the cited code
still matches current source. See `docs/cli/evidence-path.md`.

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

`eg query recency <symbol-scope>` ranks a repository's indexed symbols by LEAST-recent last
change — most dormant first — over a `scan-history` graph/store (issue #219), the
dormancy-triage lane. Each row carries the stable `Symbol` record ID + name (ADR-0004 identity,
never keyed by name so same-name symbols never collapse), the repo-relative path + span at the
last-change commit, the last-change commit SHA + valid time, and a dormancy span (seconds and
whole days) computed as `anchor.valid_time − last_change.valid_time`. A symbol's last change is
the highest-topological-rank commit at which its body differs from its parent snapshot or at
which it was introduced (reusing the #96/#215 lifeline change-detection). Dormancy is measured
against the NEWEST INDEXED COMMIT per repository (highest topological rank, SHA-ascending
tie-break — the same anchor `eg query churn` uses for `last_commit`), NEVER wall-clock "now", so
a symbol changed at the anchor commit has dormancy 0 and the ranking is byte-identical across
replays. Ordering is deterministic: `dormancy_seconds` descending, then last-change commit
topological rank ascending, then repo-relative path ascending, then record ID ascending. Both
endpoints parse as UTC instants, never raw string order. Live (non-tombstoned) symbols only — a
deleted symbol is gone, not dormant. Honesty contract: a current-tree-only `eg scan` graph
carries no `Commit` nodes, so recency is UNAVAILABLE (`no_history`, exit 2) rather than implying
every symbol is brand-new; commits present but no attributable symbols is `no_match` (exit 2).
Works over `--graph` and `--data-dir`, honors `--repo` scoping with per-repository anchors (no
cross-repo bleed), and rejects `--limit` outside 1..=500 (`invalid_limit`, exit 1). Read-only,
deterministic, `--format text` view available. See `docs/cli/recency.md`.

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

`eg query verification-coverage [scope]` partitions the issue #213
externally-reachable public API surface into verification-covered and uncovered
symbols by joining the recorded verification-domain nodes (`Verification`,
`CommandRun`, `TestRun`, `ProofResult`, `CIStatus`, `CommandEvidence`,
`BenchmarkRun`, `CoverageReport`, or any `domain: verification` node) over the
evidence-link edge/citation registry — never a `#[test]`/coverage-tool grep and
never a build or coverage run. A public symbol is COVERED when a verification
node links to it directly (any evidence-link label, `link_level: symbol`) or to
its containing file via `TOUCHED_FILE`/`FAILED_ON` (`link_level: file`), over
both edge directions and both representations (graph edges and `EvidenceLink`s
carried on a node); an agent-memory node linking to it NEVER confers coverage.
This is a capability-degradation lane by default (mirroring `undocumented`'s
`doc_facts_unavailable`): because no trunk writer links a verification node to a
code Symbol/File, a store with no verification records (`no_verification_records`)
or with verification records that never link to code (`no_verification_code_links`)
yields an explicit `verification_facts_unavailable` verdict (exit 0) with EMPTY
buckets — it NEVER floods every symbol into "uncovered". Absence of recorded
evidence is a prioritization signal, never proof that code is untested,
unverified in reality, unsafe, or broken; presence is a recorded link, never
proof of correctness or that a test/proof passed. Covered rows cite each
crediting verification record ID + kind + edge label + link level. The optional
scope handle resolves as record ID, exact symbol name, or segment-aware
repo-relative path prefix (`src/alpha` never matches `src/alphabet`); a scope
matching no in-store code item exits 2 (`scope_not_found` for a path, `no_match`
for a name/id). `--repo` scopes both endpoints (a link counts only within the
scoped repository), `--at <sha>` pins a single-commit surface snapshot, and
`--limit` (default 500, max 1000) truncates each bucket independently with a
`results_truncated` diagnostic. Read-only, allow-list-only output (never raw
payloads), deterministic and byte-identical across runs. See
`docs/cli/verification-coverage.md`.

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
or when any non-code trust-class row lacks a source/evidence/policy handle. Log-domain coverage
(issues #328, #376): the audit drives all three log query workflows that return
`runtime_observation` rows — `log-deltas` (#326), `error-context` (#324), and the subsystem
`log_signatures` section (#325) — gating every returned runtime-observation signature
row on a well-formed `log:v2:` record ID PLUS its `LogSource` provenance (source path +
`source_artifact_hash`, resolved through an at-least-one present `CAPTURED_FROM`/`AGGREGATES`
`LogSource`; the `log:v2:` ID satisfies the template-hash requirement, a disclosed schema
shape). The requirement is class-wide, so ANY other lane that surfaces a log row (e.g. `memory`)
applies it too. `runtime_observation` is its own rate-gated lane at the strictest default
`--min-log-citation 1.0` (validated to [0,1]; out-of-range exits 2), separate from the binary
non-code gate; a below-threshold lane emits `below_log_citation_threshold` naming the SPECIFIC
failing workflow (never a hard-coded `log-deltas`). Resolved-frame and
overlapping-symbol-delta rows are audited as code rows. Standing invariant: an agent-authored
claim is never counted as evidence for itself, and a runtime observation is never counted as
verification. Output is deterministic and redaction-safe — record IDs, handles, hashes,
markers, and counts only, never raw payloads (including log excerpts). See
`docs/cli/citation-audit.md`.

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
record-closure-scoped bundle rather than a control/window-scoped pack.
Issue #340 folds runtime log-graph incident evidence (foundation: umbrella #319,
#320 `ErrorSignature`/fingerprint scan, #322 `FRAME_RESOLVES_TO`, #326
log-deltas) into the CC7.2/CC7.3 sections — pure pack-side population, no new
graph domain/kind/edge/trust class. `error_signatures` rows carry each signature's
`log:v2:` ID, severity, `template_hash`, `frame_chain_hash`, first/last-seen
**clipped to the window**, content-addressed `protected:v1:` exemplar handles
(handle + hash only, never log text, #60 discipline), and each `FRAME_RESOLVES_TO`
join with its `frame_resolution` label propagated verbatim (#152/#134).
`occurrence_buckets` use a **class-specialized interval-intersection** window rule
(a bucket whose hour `[bucket_start, +1h)` intersects `[from, to)` is included
whole, no interpolation — NOT the point predicate), summing occurrences over
in-window buckets only, ordered by `(signature record_id, hour)`.
`remediation_links` is a derived join `ErrorSignature -> FRAME_RESOLVES_TO ->
Symbol -> CHANGED_IN -> Commit` (capability probe: available when all three fact
kinds exist) with commit valid_time >= the signature's window activity, carrying
`frame_resolution` verbatim and citing signature+symbol+commit+verification IDs;
its leads ride the section `log_summary` with zero hashed rows (a remediation
commit may fall outside the window), and `verify` enforces that empty-row bound as
the section's membership exemption. Log rows are trust class `runtime_observation`,
never tallied `source_fact`/`verification_evidence`. Epistemic boundary: occurrence
counts are recorded ingestion of the scanned sources, not guaranteed-complete
telemetry; remediation links are leads, never causal claims. No catalog change
(the CC7.x classes stay optional in `soc2-v1`; a `required` flip is deferred to a
future soc2-v2). See `docs/cli/evidence-pack.md`.

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
dismissed review, reconstructed from `dismissed_review.review_id`, on a
resolve-or-diagnose ladder (mirrors #334's `REVIEWS_COMMIT`): the edge is minted
only when that review is present in this run's reviews; an absent review (GitHub
omitted it — deleted account / very old review) yields a
`github_dismissed_review_absent` `Diagnostic` instead of a dangling edge, with the
transition node still emitted. `review_requested`/`review_request_removed` stand
alone with no edge. Each
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
