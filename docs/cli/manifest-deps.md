# eg query manifest-deps

List every **directly-declared Cargo dependency** captured at scan time as a
deterministic, citable graph fact — crate name, dependency kind, the declared
version requirement exactly as written, the resolved version from the nearest
`Cargo.lock`, the declaring package, and the repo-relative manifest handle
(issue #180). Local-first, no network, no build.

> **Parse-derived declaration facts, never usage or build proof.** Rows say
> what the manifests declare and what the lockfile resolved — never that the
> dependency is actually used in code, that the build succeeds, or that the
> requirement is satisfiable. `cargo metadata`, `cargo tree`, `cargo build`,
> and `cargo check` are never invoked.

The pre-edit question this answers offline is *"do we already depend on crate
X, and which version is locked?"* — without the agent opening and parsing the
manifests itself (the per-answer token cost issue #84 targets).

## Synopsis

```text
eg query manifest-deps --graph <PATH>    [--name <CRATE>] [--repo <SELECTOR>] [--format json|text]
eg query manifest-deps --data-dir <DIR>  [--name <CRATE>] [--repo <SELECTOR>] [--format json|text]
```

Reads from either a JSONL file (`--graph`) or an embedded AletheiaDB store
(`--data-dir`). `--name <CRATE>` answers the direct lookup, returning only
declarations of that exact **real** crate name — both entries of a
`package = "…"` rename pair match, since both declare the same crate; the
local alias key is reported per row in `declared_as` but is not a lookup
key. In a shared multi-repo store every row
carries its owning repository's display label (`repository`), and
`--repo <SELECTOR>` restricts the surface to one repository; an unknown or
ambiguous selector is rejected with a machine-readable stderr diagnostic
(exit 1), never resolved implicitly.

| Condition | Exit | Output |
|-----------|------|--------|
| Declarations listed — including an **empty** surface or a `--name` miss | `0` | Deps JSON on stdout, `ok:true` |
| Unreadable / missing graph input | non-zero | Error message on stderr |

An empty surface or a name miss is a **real, successful, explicit answer**:
exit 0 with `ok:true`, an empty `declarations` array, and a stable diagnostic
(`empty_dependency_surface` / `no_match_for_name`) — never a silent empty
payload, never a synthesized row.

**A definitive answer requires full manifest coverage.** When the scan skipped
an unreadable or unparseable `Cargo.toml`, a parseable one whose dependency
tables carry no usable `[package].name` (a name-less package table, an
empty or whitespace-only name — which Cargo rejects — or a
virtual manifest wrongly declaring top-level dependencies — stamped
`symbol_kind: unattributable_cargo_manifest`; its declarations cannot be
attributed to a declaring package), `{ workspace = true }` entries with no
resolvable workspace root (stamped `symbol_kind:
uninheritable_cargo_dependency`; the manifest's other declarations still
extract), or dependency entries Cargo rejects — neither a version string
nor a dependency table (`serde = true`), a table without a usable
source (a string `version`/`path`/`git` or `workspace = true`; empty
tables count — `registry`/`rev`/`branch`/`tag` only refine a source and
never stand alone), or a table whose KNOWN keys are wrong-typed or carry
disallowed values, which poisons the whole entry even beside a valid
source: `version`/`path`/`git`/`registry`/`branch`/`tag`/`rev`/`package`
must be strings, `optional`/`default-features` (and the deprecated
`default_features`) booleans, `features` an array of strings,
`workspace` only the literal `true` (`workspace = false` is
Cargo-invalid), and `optional` is illegal in `[dev-dependencies]` (dev
deps cannot be optional; normal and build deps can) — unknown keys stay
tolerated, since Cargo warns but loads. Cross-field source rules Cargo
enforces are checked too (each verified against `cargo metadata`):
`path` and `git` are mutually exclusive, `git` and `registry` are
mutually exclusive, and `branch`/`tag`/`rev` require `git` with at most
one of the three — while `registry` beside `version` or `path` stays
accepted, since Cargo only checks registry *configuration* later. A
version requirement string
Cargo cannot parse (`serde = "not a req"`, plain-string or table
`version`) is the same class: rejected at declaration time, never
resolved against a lockfile — as is an empty or whitespace-only
dependency NAME (a quoted empty table key `"" = "1"`, or a blank
`package` value; the trimming boundary matches the package-name
check). The same class covers a
`{ workspace = true }` entry whose
`[workspace.dependencies]` root spec exists but is itself invalid: no
usable string `version`/`path`/`git`, an unparseable template `version`,
ill-typed known keys, or the
member-only keys Cargo disallows in templates (`optional`, `workspace`) —
stamped `symbol_kind: uninterpretable_cargo_dependency`,
the invalid entry skipped — never a key-named row with no requirement —
while valid sibling entries, including git-only declarations, still
extract — every response — hit, miss, and empty
surface — carries one
`skipped_manifest` diagnostic per skipped manifest, citing the repo-relative
manifest handle (`detail`) and the `Diagnostic` record ID (`record_id`) in
deterministic path order. A `no_match_for_name` accompanied by
`skipped_manifest` is **not** a definitive "we don't depend on X": the skipped
manifest's declarations are unknown. Exit codes are unchanged. Each diagnostic
is attributed to its repository (`Repository —CONTAINS→ Diagnostic` topology)
and carries the owning repository label; under `--repo`, coverage holes owned
by **other** repositories are dropped, while unattributable legacy diagnostics
are always included — their absent `repository` field is the marker — because
hiding a possible coverage hole would be worse than over-reporting one.

## How the facts are captured

`eg scan` (and `eg scan`'s library entry points) discovers every `Cargo.toml`
under the repository root — honoring the same Git-scope and ignore rules as
source discovery — and parses it with a real TOML parser (`toml_edit`), never
a regex or token grep. Each entry in `[dependencies]`, `[dev-dependencies]`,
and `[build-dependencies]` becomes one `DependencyDeclaration` node per
declared entry (manifest key):

- **`name`** — the crate name. A `package = "real-name"` rename records the
  *real* crate name, since that is what the lockfile lists and what "do we
  depend on X?" means; the manifest key it was declared under is kept in
  **`declared_as`** (absent for plain declarations). A rename pair that
  declares two versions of the same crate (e.g. `embedded-hal = "0.2"` plus
  `embedded-hal-1 = { package = "embedded-hal", version = "1" }`) yields two
  facts with distinct stable IDs — declarations are never collapsed.
- **`dependency_kind`** — `normal`, `dev`, or `build`, from the table the
  declaration was written in.
- **`declared_requirement`** — the `version` string exactly as written
  (`"1.0.228"`, `"1"`). Absent when the declaration carries no `version` key
  (pure `path`/`git` dependencies) — never fabricated. A
  `{ workspace = true }` entry resolves through the owning workspace root's
  `[workspace.dependencies]` table, looked up by the entry's MANIFEST KEY:
  the real crate name is the root entry's
  `package` (else the shared key, with `declared_as` recording the member key
  when it differs), the requirement is the root entry's `version` (absent for
  a path/git-only template), a member-local `version` beside
  `workspace = true` — which Cargo rejects — never overrides the root's,
  and a member-side `package` beside `workspace = true` (which Cargo
  accepts and ignores — verified against `cargo metadata`) never
  overrides the root template either. An
  inherited entry with no resolvable workspace root (or a key missing from
  the root table) yields **no row** — a key-named row would be a
  fabrication — plus a `skipped_manifest` qualification
  (`uninheritable_cargo_dependency`); a root spec that exists but is itself
  Cargo-invalid (no usable string `version`/`path`/`git`, e.g. `serde = {}`
  or wrong-typed `version = 1`, ill-typed known keys, or the member-only
  keys Cargo disallows in `[workspace.dependencies]` — `optional` and
  `workspace` itself) also yields no row, qualified as
  `uninterpretable_cargo_dependency`, while a valid path-only root spec —
  or one carrying `features`/`default-features`, which are allowed
  there — still inherits.
- **`resolved_version`** / **`resolution`** — the manifest resolves against
  the `Cargo.lock` Cargo would actually use: the lockfile of the crate's
  **workspace root** for a member, or its own directory's lockfile for a
  standalone package or a manifest that itself declares `[workspace]`. The
  walk stops at the first `[workspace]`-declaring manifest — the manifest's
  own, or the first ancestor's — whether or not a lockfile sits beside it: a
  lockfile-less workspace root means `no_lockfile`, never an outer lockfile
  even when an outer workspace's globs would match. A **member** always
  resolves through the root's lockfile state — a stale or corrupt
  `Cargo.lock` beside a member's own manifest is dead state Cargo never
  reads and is ignored. Membership at an ancestor root means
  the `members` globs include the crate (and `exclude` does not; glob syntax
  covers `*`, `?`, spanning `**`, and `[…]` character classes with ranges and
  `[!…]` negation, per the `glob` crate Cargo uses — an unclosed class, which
  Cargo would reject, degrades to a literal `[`; an absolute in-repo
  `members`/`exclude` pattern is normalized like absolute path deps — repo
  root lexically absolutized and stripped, then re-expressed
  workspace-root-relative — so it matches, excludes, and seeds the closure,
  while an out-of-repo absolute pattern is a documented skip; a relative
  pattern whose literal prefix carries `.`/`..` segments
  (`crates/../app`) is normalized in the same repo-relative space, glob
  segments carried literally, with a repo-escaping prefix skipped) **or** the
  workspace reaches it through in-tree `path = "…"` dependencies
  (transitively — Cargo's automatic members; the closure grows from the root
  package and from every glob member, so a virtual root's members contribute
  their path dependencies too, while an excluded directory is pruned at
  traversal time — neither it nor packages reachable only through it join).
  A Cargo-invalid dependency entry (ill-typed known keys, or a cross-field
  source conflict like `{ path = "…", git = "…" }`) never seeds
  membership — its `path` is not trusted, in member tables or workspace
  templates alike. Whether an invalid ROOT manifest should invalidate the
  entire workspace it declares is an open question beyond this slice; only
  the invalid entries themselves are refused.
  Explicit `members` patterns resolve against the root's own directory, so a
  `../`-relative member outside the root's tree — literal
  (`members = ["../pkgs/app"]`) or glob (`../pkgs/*`, enumerated under the
  resolved parent directory) — seeds the closure and contributes its own
  path dependencies like any other member; a pattern escaping the
  repository is skipped.
  Target-specific tables (`[target.<cfg>.dependencies]` and the
  `dev-`/`build-` variants) feed this membership closure too, even though
  target-specific dependency rows stay out of extraction scope; so do
  workspace-inherited entries — a `{ workspace = true }` declaration
  resolves against the root's `[workspace.dependencies]` `path` (relative
  to the root), while a template no member ever inherits never becomes a
  member.
  Absolute `path` values pointing anywhere inside the
  REPOSITORY — including outside the workspace directory — are normalized
  to root-relative member paths (the repo root is lexically absolutized and
  stripped, so `eg scan .` with a relative repo path recognizes them too;
  symlinks are never resolved). Non-members and excluded
  members are standalone: their own directory's lockfile state applies —
  never a fabricated `locked` from an unrelated
  ancestor. Plain package manifests and stray lockfiles without a manifest
  are walked past, mirroring Cargo's workspace discovery; an unreadable
  ancestor manifest stops the walk without accepting the ancestor.
  A `package.workspace` explicit pointer names the owning workspace root
  directly and replaces ancestor discovery entirely: a relative pointer
  resolves lexically against the manifest's directory and an absolute one
  is normalized like absolute path dependencies (the repo root lexically
  absolutized and stripped — relative scan roots covered; an out-of-repo
  pointer is a documented skip), the target must declare `[workspace]`, and
  membership is MUTUAL per Cargo — the pointed root's own membership rules
  (`members` globs including `../`-relative patterns, automatic
  path-dependency members, `exclude`) must cover the pointing package.
  When they do, the root's lockfile state and `[workspace.dependencies]`
  inheritance apply — so a member OUTSIDE the
  root's directory tree (root `members = ["../pkgs/a"]`) still resolves
  through the root's lockfile. A broken pointer (target missing or not a
  workspace root) or a non-covering target means honest standalone
  behavior.
  Relative path dependencies resolve against the declaring manifest's
  REPO-relative position, so an out-of-root member's dependency may climb
  back over the repository tree (or back inside the root's own tree) and
  stay a member; only paths escaping the *repository* — relative or
  absolute — are not interpreted in this slice.
  The declared requirement gates **every** path with Cargo semantics
  (`"1"` means `^1`), including a sole locked version that a stale or shared
  lockfile can leave unsatisfying: exactly one satisfying version is
  `locked`; none is `requirement_unsatisfied_in_lockfile`; several satisfying
  (or an absent requirement over several versions) stay
  `ambiguous_in_lockfile`. Without a requirement (a pure `path`/`git`
  declaration), a sole locked version resolves directly. A requirement
  string Cargo cannot parse never reaches resolution at all: the entry is
  rejected at declaration time as `uninterpretable_cargo_dependency` (see
  above). A resolved version is never guessed. The
  `resolution` marker is drawn from a closed set:

  | Marker | Meaning | `resolved_version` |
  |--------|---------|--------------------|
  | `locked` | exactly one version of the crate is in the lockfile | present |
  | `no_lockfile` | no `Cargo.lock` found for this manifest | absent |
  | `not_in_lockfile` | lockfile exists but does not list the crate | absent |
  | `ambiguous_in_lockfile` | several locked versions and the declared requirement (absent, or satisfied by two+) cannot select exactly one | absent — never a guess |
  | `requirement_unsatisfied_in_lockfile` | the lockfile lists the crate but a parseable declared requirement is satisfied by **none** of the locked versions (stale/shared lockfile) | absent — the mismatched version is never presented as resolved |
  | `lockfile_unreadable` | the **nearest** `Cargo.lock` exists but could not be read or parsed | absent — an ancestor lockfile is never consulted in its place |

- **`declaring_package`** — `[package].name` of the owning manifest.
- **manifest handle** — the node's `repo_relative_path` is the owning
  `Cargo.toml`, and every row carries a stable `codegraph:v<N>:` record ID.
- **repository attribution** — each declaring manifest gets a `File` node
  joined by the standard containment chain
  (`Repository —CONTAINS→ File —CONTAINS→ DependencyDeclaration`), the exact
  topology `RepositoryIndex` walks, so dependency facts are scoped and labeled
  per repository in a shared multi-repo store. The manifest `File` summary is
  handle-only — the manifest body is never embedded.

A manifest that fails TOML parsing yields one `Diagnostic` node citing the
manifest handle (stamped `symbol_kind: unparseable_cargo_manifest` so query
surfaces can recognize the coverage hole); the scan never fails and never
invents facts. A virtual
workspace root (no `[package]`) legitimately declares nothing. The nearest
`Cargo.lock` that exists is authoritative: when it cannot be read or parsed,
the upward search stops and the manifest's dependencies are marked
`lockfile_unreadable` — versions are never taken from an unrelated ancestor
lockfile.

## Scope and bounds

- **Directly-declared dependencies only.** The transitive dependency tree,
  feature unification, and per-feature resolution from `Cargo.lock` are out
  of scope; only the declared entry and its single locked version are
  recorded.
- **Captured tables:** `[dependencies]`, `[dev-dependencies]`,
  `[build-dependencies]`. Target-specific tables
  (`[target.'cfg(..)'.dependencies]`) and workspace-level
  (`[workspace.dependencies]`) tables are out of scope for this slice.
- No cross-crate symbol identity: an external call/import site is never
  resolved to a symbol inside the external crate.
- No crates.io lookups, advisory/yank/license metadata, or version-bump
  impact analysis.
- Cargo only; non-Cargo build systems and non-Rust languages are out of
  scope, as is history-backed (as-of) dependency reconstruction.
- **Refreshing after manifest edits.** `eg refresh` / incremental scans do
  not yet re-derive dependency facts, and re-ingesting a fresh scan into an
  *existing* embedded store never retires a **removed** dependency: `ingest`
  writes only the records present in the JSONL and does not tombstone absent
  IDs, so `query manifest-deps --data-dir` would keep reporting the removed
  crate as declared. After an edit that removes or renames a dependency,
  rebuild the store — ingest the fresh scan into a **new** `--data-dir` — or
  query the fresh `graph.jsonl` directly, which always reflects the current
  tree. Tombstoning absent dependency IDs on ingest/refresh is a follow-on
  slice. When a tombstone **is** present (an append-style graph or a store
  where the record was retracted), the query honors it: a tombstoned
  dependency row is never returned as live, and a tombstoned
  skipped-manifest diagnostic stops qualifying answers — the current-state
  contract every other query path follows.

## Output

Deterministic and byte-identical across runs. Stable ordering: repository
label, then manifest path, then declaring package, then dependency kind
(`normal` < `dev` < `build`), then crate name. Redaction-safe: names, version strings, markers, handles,
and counts only — raw manifest body text is never emitted.

### `--format json` (default, the agent contract)

```json
{
  "ok": true,
  "query": "manifest-deps",
  "count": 2,
  "disclaimer": "Declared-dependency facts parsed from Cargo manifests and the nearest Cargo.lock; never proof the dependency is used in code, builds, or resolves.",
  "declarations": [
    {
      "record_id": "codegraph:v5:…",
      "repository": "acme/widget",
      "name": "serde",
      "dependency_kind": "normal",
      "declared_requirement": "1.0.228",
      "resolved_version": "1.0.228",
      "resolution": "locked",
      "declaring_package": "pkg-a",
      "manifest_path": "crates/pkg-a/Cargo.toml",
      "schema_version": 5
    }
  ],
  "diagnostics": []
}
```

`name` echoes the `--name` filter when one was given; `repo_scope` echoes the
resolved `--repo` selector. `repository` is omitted when a record cannot be
attributed (e.g. a legacy graph without a `Repository` node). `declared_requirement`
and `resolved_version` are omitted when absent — readers must treat a missing
key as "not declared / not resolved", never as an empty string.

### `--format text`

One line per declaration:

```text
2 dependency declaration(s)
pkg-a normal serde requirement=1.0.228 resolved=1.0.228 manifest=crates/pkg-a/Cargo.toml (codegraph:v5:…)
pkg-a dev tempfile requirement=3 resolved=3.27.0 manifest=crates/pkg-a/Cargo.toml (codegraph:v5:…)
```

Unresolved rows print the marker in the `resolved=` column
(`resolved=no_lockfile`).

## Workflow

```powershell
cargo run -- scan . --out graph.jsonl
cargo run -- query manifest-deps --graph graph.jsonl           # full surface
cargo run -- query manifest-deps --graph graph.jsonl --name serde  # direct lookup

cargo run -- ingest graph.jsonl --adapter embedded --data-dir .egregore
cargo run -- query manifest-deps --data-dir .egregore --name serde # store-backed
```

A store-backed answer is only as fresh as the last ingest, and re-ingesting
into an existing store adds and updates declarations but does not retire
removed ones (see *Scope and bounds*). When manifests have changed — and
always after a dependency removal — prefer the `--graph` form on a fresh
scan, or rebuild the store into a new `--data-dir`.

## Schema

`DependencyDeclaration` nodes live in the code-graph domain under the current
`SCHEMA_VERSION`. The node kind and its `dependency` payload
(`declaring_package`, `dependency_kind`, `declared_requirement`,
`resolved_version`, `resolution`) are additive per
`docs/schema/schema-versioning.md §2`: old readers may ignore the new kind
and field; new readers tolerate their absence. The facts are deterministic
code-graph records — no agent-authored trust class, no network importer.
