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
  (pure `path`/`git`/`workspace = true` dependencies) — never fabricated.
- **`resolved_version`** / **`resolution`** — the manifest resolves against
  the nearest `Cargo.lock`, walking up from the manifest's directory to the
  repository root (the standard workspace layout keeps one root lockfile).
  A parseable declared requirement gates **every** path with Cargo semantics
  (`"1"` means `^1`), including a sole locked version that a stale or shared
  lockfile can leave unsatisfying: exactly one satisfying version is
  `locked`; none is `requirement_unsatisfied_in_lockfile`; several satisfying
  (or an absent/unparseable requirement over several versions) stay
  `ambiguous_in_lockfile`. Without a usable requirement, a sole locked
  version resolves directly. A resolved version is never guessed. The
  `resolution` marker is drawn from a closed set:

  | Marker | Meaning | `resolved_version` |
  |--------|---------|--------------------|
  | `locked` | exactly one version of the crate is in the lockfile | present |
  | `no_lockfile` | no `Cargo.lock` found for this manifest | absent |
  | `not_in_lockfile` | lockfile exists but does not list the crate | absent |
  | `ambiguous_in_lockfile` | several locked versions and the declared requirement (absent, unparseable, or satisfied by two+) cannot select exactly one | absent — never a guess |
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
manifest handle; the scan never fails and never invents facts. A virtual
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
  slice.

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
