# Owning-Cargo-package attribution (`--package`)

Every code fact Egregore extracts records **which Cargo package owns it** —
the package name plus the repo-relative path of the owning `Cargo.toml` —
resolved from the nearest enclosing manifest (issue #117). `eg query symbol`
and `eg query symbols` accept `--package <NAME>` to scope an answer to one
crate.

Before this, a workspace monorepo was a flat pool of source files: every fact
carried a `repo_relative_path`, but nothing named the package. Agents fell back
to path-prefix guessing (`--under`, issue #83), which conflates a directory with
a package, cannot name it, and assumes one package per top-level directory.

## Synopsis

```powershell
# Every code fact in the graph carries its owning package
eg scan . --out graph.jsonl

# Scope one symbol lookup to one crate
eg query symbol handle --graph graph.jsonl --package alpha

# Enumerate one crate's whole symbol surface
eg query symbols "*" --graph graph.jsonl --package alpha

# Compose with repository scoping in a shared multi-repo store
eg query symbols "*" --data-dir .egregore --package util --repo acme/widget
```

## Arguments

| Flag | Meaning |
|---|---|
| `--package <NAME>` | Return only rows attributed to this Cargo package. Matched **exactly** — no case folding, no `-`/`_` normalization. |

The flag is spelled `--package`, **not** `--crate`. `eg query who-imports
--crate <name>` already exists and means something unrelated (rewriting a
leading `crate::` in the query and in import paths so the two spellings unify);
one spelling for two meanings across adjacent lanes would be worse than a
slightly longer flag. `--package` is also Cargo's own spelling.

## Semantics

- **Nearest enclosing manifest wins.** Resolution walks the ancestor
  directories of a file's repo-relative path, nearest first, out to the
  repository root, and resolves at the first `Cargo.toml` it finds. A crate
  vendored inside another crate owns its own subtree; the parent never claims
  it.
- **Segment-aware containment.** `crates/alpha` never claims
  `crates/alphabet/src/x.rs`. Ancestors are derived by splitting on `/`, never
  by string prefix comparison.
- **A virtual manifest is walked past.** A `[workspace]` root with no
  `[package]` table declares no package and cannot own a file, so the walk
  continues outwards — matching Cargo.
- **Every other manifest form stops the walk, fail-closed.** An unparseable,
  unreadable, or unnamed-`[package]` manifest yields its own reason rather than
  letting an ancestor's name be inherited across the broken boundary. Inheriting
  would fabricate a package attribution, which is exactly what this slice
  forbids.
- **The package name is read, never derived.** It comes from `[package].name`
  in the manifest. A directory name is never used, and a Cargo-invalid name
  (`""`, `"bad name"`) is never sanitized into a usable one.
- **The walk is bounded at the repository root.** A `Cargo.toml` in the scan
  root's parent directory is never consulted.
- **Manifests under `target/`, inside a nested Git checkout, or named
  `cargo.toml` (lowercase) are not manifests.** The harvest reuses the same
  exclusions `eg scan` already applies to source discovery.
- **Local and read-only.** No `cargo build`, `cargo check`, `cargo metadata`,
  network call, or remote fetch on any path. The current-tree scan reads
  manifests from the working tree; history replay reads them from Git objects
  via `git ls-tree` / `git show` and never mutates the checkout.
- **Deterministic.** Attribution and ordering are byte-identical across
  repeated runs, independent of record order, and independent of the
  repository's absolute location on disk.

### Absent is not the same as unattributed

| Field state | Meaning |
|---|---|
| `crate_attribution` **absent** | Attribution is **UNKNOWN** — see the two causes below. |
| present, `status: "attributed"` | An owning package was resolved; `package_name` and `manifest_repo_relative_path` are both present. |
| present, `status: "unattributed"` | Attribution **was computed** and there is provably no owning package; `unattributed_reason` names why. |

These are never conflated. Collapsing them would turn an unknown into a
fabricated "proven ownerless" claim. In `--format text`, an absent field prints
**nothing at all**.

An **absent** field has exactly two causes, and they are not distinguishable
from the field alone:

1. The record predates issue #117.
2. The record was minted by a producer **other than** `eg scan`, `eg refresh`,
   or `eg scan-history` — the only three that stamp attribution. `eg
   capture-tests`, `eg resolve-frames`, and `eg import local` each mint
   path-bearing `Diagnostic` records without it, because those diagnostics
   describe an artifact outside the code-graph extraction pipeline.

So a single store, written by a single current binary, can legitimately hold
both attributed and attribution-less path-bearing records. To distinguish the
two causes, read the record's producer envelope (issue #234): the inference
"absent ⇒ predates #117" is sound only for records whose `producer_kind` is one
of `code_graph_extractor`, `history_replay`, or `incremental_cache`.

What the total-function property does guarantee is narrower and still useful:
**within one scan, refresh, or replay**, field presence is a function of node
kind alone — every path-bearing code-graph node carries it and no other node
does — so an absent field never means "this kind happens not to be covered".

### Unattributed reasons (closed set)

| Reason | Meaning |
|---|---|
| `no_enclosing_manifest` | No `Cargo.toml` in any ancestor directory **within the repository**. Typically a stray source file outside every crate — though it also covers a scan rooted inside a crate, whose manifest sits above the scan root. |
| `virtual_manifest_only` | Every enclosing manifest is a virtual workspace root, which declares no package. |
| `unnamed_package` | The nearest manifest declares a `[package]` whose `name` is absent or Cargo-invalid. |
| `unparseable_manifest` | The nearest manifest is not valid TOML. |
| `unusable_manifest` | The nearest manifest parses but is one Cargo refuses to load: it carries neither `[package]` nor `[workspace]`, or it is a virtual (`[workspace]`, no `[package]`) manifest carrying a package-only section (`dependencies`, `dev-dependencies`, `build-dependencies`, their legacy underscore spellings, `features`, `target`, `lib`, `bin`, `bench`, `test`, `example`, `badges`, `lints`, `hints`). That set is pinned to the toolchain this repository targets and can lag a newer Cargo: it is a deny-list because Cargo *tolerates* an unrecognized section in a virtual manifest, so an allow-list would un-attribute every subtree under a manifest carrying a future or tool-specific key. A manifest is treated as a usable virtual root only on POSITIVE confirmation — no `package` key in any shape, a `[workspace]` table, and no forbidden section — because walking past is the one direction that can attribute a subtree to an outer package. Deeper type errors NESTED inside `[workspace]` (beyond a non-table `workspace.package`) are not validated: this resolver confirms a manifest's top-level shape, it does not reimplement Cargo's schema, so a manifest malformed only in a nested workspace field may still be walked past. |
| `manifest_unreadable` | The nearest manifest could not be read, or is not UTF-8. |

The reason carries no payload: a TOML parse-error message can echo manifest body
text, so it is dropped rather than forwarded.

### Which nodes carry it

`File`, `Module`, `Symbol`, `Import`, `Diagnostic`, `PanicRiskSite`,
`DebtMarker`, `UnsafeSite`, `DependencyDeclaration`, and `Change` — every
code-graph node that carries a `repo_relative_path`. The classifier is an exhaustive match with
no wildcard arm, so a future node kind fails to compile until it is deliberately
classified.

`Repository`, `Commit`, and `ScanCoverage` carry no path and are never
attributed; attributing them would claim the root package owns the repository
itself. Edge and tombstone records never carry it.

A manifest's own `File` node self-attributes: the nearest enclosing manifest of
`crates/alpha/Cargo.toml` is itself, so it is attributed to `alpha`.

## Output format

### `--format json`

An attributed row:

```json
"crate_attribution":{"status":"attributed","package_name":"alpha","manifest_repo_relative_path":"crates/alpha/Cargo.toml"}
```

An unattributed row:

```json
"crate_attribution":{"status":"unattributed","unattributed_reason":"virtual_manifest_only"}
```

A legacy row **omits the key entirely** — never `"crate_attribution":null`.

Under `--package`, every row additionally carries
`crate_attribution_disclaimer` with the verbatim containment caveat below.
Unscoped rows carry no disclaimer: their `manifest_repo_relative_path` already
names exactly what the claim rests on.

`eg query file` carries `crate_attribution` on the first row of each DISTINCT
value: it is a file-level fact, but "the file" is not globally unique — a
repo-relative path can exist in several repositories, and over a `scan-history`
graph its owning package can change between commits. Every surviving value is a
fact about the row it rides on, and the common single-package case still emits
exactly one.

### `--format text`

Appended to the row, in the style of the existing `visibility` / `signature` /
`doc` lines:

```text
handle (Symbol) @ crates/alpha/src/lib.rs:1 (extraction: complete)
  package: alpha (crates/alpha/Cargo.toml)
```

```text
generate (Symbol) @ scripts/gen.rs:1 (extraction: complete)
  package: (unattributed: virtual_manifest_only)
```

The text form is a human view and is not a stable contract; parse the JSON.

## Exit codes

| Condition | Exit | Diagnostic |
|---|---|---|
| At least one row after the package filter | 0 | — |
| Package known to the corpus, zero matching rows | 2 | the lane's ordinary `no match found` |
| Package unknown to the corpus | 1 | `{"code":"unknown_package_selector","selector":…,"known_packages":[…]}` |
| No record in the corpus carries attribution at all | 1 | `{"code":"crate_attribution_unavailable","selector":…,"remedy":…}` |
| Package name owned by two or more repositories, no `--repo` | 1 | `{"code":"ambiguous_package_selector","selector":…,"candidates":[…]}` |
| `--package` with `--daemon` | 1 | `{"code":"unsupported_combination","flags":["--package","--daemon"]}` |

A typo is never a silent empty answer: exit 1 with the known packages is
distinguishable from exit 2 "this package owns no matching symbol". An
ambiguous name is never resolved by silently merging two repositories' facts,
which would make the scoped answer's precision claim false.

`known_packages` lists only packages owning at least one **attributed record in
the loaded corpus**. A package whose manifest exists but that owns no indexed
record is not listed and resolves as `unknown_package_selector`.

`--package` always loads the **whole corpus**, opting out of the
[`eg index`](index.md) sidecar fast path exactly as `--repo` does. Both verdicts
above need global knowledge a one-symbol index closure cannot supply: obviously
so for enumerating `known_packages`, and — the trap — for cross-repository
ambiguity, since a package owned by two repositories where only one defines the
queried symbol looks unambiguous inside the narrowed closure. An answer that
changed with the presence of an `.idx` file would break the index's contract of
being a pure access-path optimization.

### A record's attribution is re-checked before it is believed

An attribution read back from a store or a graph is operator-controlled, so the
reader re-derives whether the value is one the resolver could actually have
**produced** before scoping or rendering on it. Three checks, all fail-closed:

1. `status: attributed`, both strings present, no `unattributed_reason` — a
   value carrying both an attribution and a reason is a shape no producer
   writes.
2. The package name satisfies Cargo's manifest-load charset — the same rule that
   gated it at production, shared rather than re-derived.
3. The manifest path has the shape the ancestor walk produces: a relative,
   `/`-separated path with no `.`/`..` segment, no empty segment, no backslash,
   no drive prefix, no control character, whose last segment is `Cargo.toml`.

A value failing any check **owns nothing**: it is not scopable, never appears in
`known_packages`, and renders nothing — indistinguishable downstream from an
absent attribution, and never from a proven-ownerless one. This matters most on
the text path, where the value is interpolated verbatim: without the last two
checks a newline in either field forges an entire additional output line.

## Example

```console
$ eg query symbol handle --graph graph.jsonl --format text
handle (Symbol) @ crates/alpha/src/lib.rs:1 (extraction: complete)
  package: alpha (crates/alpha/Cargo.toml)
handle (Symbol) @ crates/beta/src/main.rs:1 (extraction: complete)
  package: beta (crates/beta/Cargo.toml)

$ eg query symbol handle --graph graph.jsonl --package alpha --format text
handle (Symbol) @ crates/alpha/src/lib.rs:1 (extraction: complete)
  package: alpha (crates/alpha/Cargo.toml)

$ eg query symbol handle --graph graph.jsonl --package alfa
{"code":"unknown_package_selector","known_packages":["alpha","alphabet","beta","inner"],"selector":"alfa"}
$ echo $LASTEXITCODE
1
```

## Epistemic limits

The load-bearing one, stated verbatim wherever this feature is described and
carried as the value of `crate_attribution_disclaimer`:

> attribution is nearest-enclosing-manifest directory containment, never proof
> the file is compiled into that package

Concretely:

- A `[lib] path` / `[[bin]] path` / `[[test]] path` pointing **outside** the
  package directory attributes that file to whatever owns *its* directory, not
  to the declaring package.
- A `#[path = "…"]` module or an `include!()`d file physically outside the
  package directory is not attributed to the including package.
- An orphan `.rs` file that no `mod` declaration reaches is attributed to the
  enclosing package anyway; Cargo compiles it into nothing.
- A non-Rust file (`.py`, `.ts`, `.go`) inside a crate directory **is**
  attributed to that Cargo package, despite Cargo never compiling it. The rule
  is uniform containment; a carve-out would be more surprising than the rule
  plus this limit.
- A file compiled into more than one package receives exactly **one**
  attribution, chosen by directory. This is a stated one-owner model.
- A `workspace.exclude`d package still owns its own directory tree — which
  nearest-enclosing gets right where a workspace-root `cargo metadata` would
  omit it entirely.
- Workspace membership, versions, features, editions, and Cargo target roles
  (`lib`/`bin`/`test`/`bench`/`example`) are **not** captured and **not**
  inferred.
- Symlinked manifests are invisible on both discovery paths (the working-tree
  walk rejects non-regular files; history replay rejects Git mode `120000`),
  consistent with what `eg scan` already indexes.
- A nested checkout committed as plain tracked files (not a submodule gitlink)
  is excluded by `eg scan` but visible to `eg scan-history`, which cannot
  reconstruct nested-worktree sentinels from a tree listing. Submodules are
  excluded on both paths.

## Out of scope (this slice)

- **Cross-crate canonical symbol identity.** ADR-0004's deferral stands: this
  attributes facts to packages, it does not merge or re-key symbols across
  crates. `crate_attribution` is never an identity input, so adding it moves no
  record ID **within a schema version**. Note that this release ALSO bumps
  `SCHEMA_VERSION` 8 → 9 for the new field, and `stable_id` embeds the version in
  every `codegraph:v<N>:` ID — so every code-graph record does get a new ID in
  this release, for that reason rather than this one. See
  [`store-upgrade.md`](store-upgrade.md#upgrading-a-store-across-a-codegraph-schema_version-bump):
  re-ingesting into an existing v8 store adds a second generation rather than
  superseding it, and the remedy is a fresh `--data-dir`.
- **The inter-crate dependency graph**, crates.io resolution, and
  version/feature resolution. `eg query manifest-deps` (#180) already carries
  declared and lockfile-resolved dependency facts.
- **Non-Cargo build systems and non-Rust languages.**
- **Replacing `--under` path-prefix scoping (#83) or the orientation map
  (#95).** Prefix scoping and package scoping coexist; this slice supplies the
  attribution those surfaces can consume.
- **Workspace-membership facts** (`members`, `exclude`, "is this an excluded
  member"). Resolving them needs glob expansion and directory listing, which
  have no Git-blob equivalent — using them would make `eg scan` and
  `eg scan-history` answer differently for the same tree.
- **Cargo target roles.** A virtual manifest has no `[package].name`, so
  name-absence already decides the AC4 case; reimplementing Cargo's target
  auto-discovery would add a fabrication surface for a small minority of files.
- **`--package` on lanes beyond `query symbol` / `query symbols`.** The field
  exists on every path-bearing node, so every other lane's `--package` is now a
  single-join follow-up.
- **`--daemon` and MCP.** The daemon's symbol projection is an independent
  hand-built JSON map that already omits `visibility`, `signature`, and `doc`;
  rather than silently ignore the flag there, it is refused.
- **`eg watch`.** It is the agent-transcript watcher: no code-graph scan, no
  incremental cache, no Git access. There is no hook to add.

## See also

- [`query.md`](query.md) — the `eg query symbol` / `eg query symbols` lanes
- [`manifest-deps.md`](manifest-deps.md) — declared Cargo dependency facts
- [`who-imports.md`](who-imports.md) — note the different meaning of its
  `--crate` flag
- [`../schema/schema-versioning.md`](../schema/schema-versioning.md) — the
  codegraph `8 → 9` bump
- [`store-upgrade.md`](store-upgrade.md) — what a schema-version bump means for
  an existing store
