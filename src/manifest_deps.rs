//! Cargo manifest dependency-declaration extraction (issue #180).
//!
//! Captures every directly-declared Cargo dependency — `[dependencies]`,
//! `[dev-dependencies]`, and `[build-dependencies]` — from each `Cargo.toml`
//! under the repository root as deterministic, citable `DependencyDeclaration`
//! graph facts, joined with the single resolved version from the nearest
//! `Cargo.lock` when one exists.
//!
//! Strictly local and read-only: manifests and lockfiles are parsed with a
//! real TOML parser (`toml_edit`); `cargo build`/`cargo check`/`cargo
//! metadata` are never invoked and no network access occurs. When no lockfile
//! is present, declarations are still captured and marked with the documented
//! `no_lockfile` resolution; when the nearest `Cargo.lock` exists but cannot
//! be read or parsed, they are marked `lockfile_unreadable` and an ancestor
//! lockfile is never consulted — a resolved version is never fabricated.
//!
//! Out of scope (per issue #180): the transitive dependency tree, feature
//! unification, target-specific (`[target.'cfg(..)'.dependencies]`) and
//! workspace-level (`[workspace.dependencies]`) tables, cross-crate symbol
//! identity, and non-Cargo build systems.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use crate::{
    crate_attribution::{ManifestPackageFact, ManifestParseOutcome},
    error::Result,
    fs::discover_cargo_manifests,
    ir::{DependencyDeclarationPayload, EdgeLabel, GraphRecord, NodeKind, stable_id},
};

/// Dependency table a declaration was written in.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum DependencyKind {
    /// `[dependencies]`.
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
}

impl DependencyKind {
    /// Returns the serialized dependency kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Dev => "dev",
            Self::Build => "build",
        }
    }

    /// Manifest table header for this kind.
    const fn table(self) -> &'static str {
        match self {
            Self::Normal => "dependencies",
            Self::Dev => "dev-dependencies",
            Self::Build => "build-dependencies",
        }
    }
}

/// The loadable form a `Cargo.toml` takes (issue #117).
///
/// A closed set: each variant demands a different answer from the
/// nearest-enclosing-manifest walk.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub enum ManifestShape {
    /// Declares a `[package]` table. Its `name` may still be absent or
    /// Cargo-invalid; see `ManifestDependencies::package_name`.
    Package,
    /// A usable VIRTUAL workspace root: `[workspace]`, no `[package]`, and no
    /// section Cargo forbids beside it. Declares no package, so it owns nothing
    /// and the attribution walk passes it.
    VirtualRoot,
    /// A form Cargo REFUSES to load: neither `[package]` nor `[workspace]`, or a
    /// virtual manifest carrying a package-only section. The boundary exists but
    /// is unusable, so the attribution walk stops rather than crossing it.
    Unusable,
}

/// Sections Cargo forbids in a VIRTUAL manifest (`[workspace]`, no `[package]`).
///
/// Cargo rejects such a manifest with "this virtual manifest specifies a
/// `<section>` section, which is not allowed". Every entry was verified against
/// the toolchain this repository pins (cargo 1.94.1); `[profile]`, `[patch]`,
/// `[replace]`, and `[project]` were verified ACCEPTED and are deliberately
/// absent.
///
/// A DENY-list, deliberately, even though it can lag: Cargo tolerates an
/// unrecognized section in a virtual manifest (verified — an invented
/// `[totally-made-up-section]` loads fine), so an allow-list would classify
/// every manifest carrying a future or tool-specific key as unusable and
/// silently un-attribute its whole subtree. The residual risk is the opposite
/// direction — a section Cargo forbids in a LATER release is walked past here
/// until this list is updated, exactly as `hints` was before it was added.
const VIRTUAL_MANIFEST_FORBIDDEN_SECTIONS: [&str; 15] = [
    "dependencies",
    "dev-dependencies",
    "dev_dependencies",
    "build-dependencies",
    "build_dependencies",
    "features",
    "target",
    "lib",
    "bin",
    "bench",
    "test",
    "example",
    "badges",
    "lints",
    "hints",
];

/// The TOML shape a known `[workspace]` field must have for Cargo to load the
/// manifest at all.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum WorkspaceFieldShape {
    /// An array whose every element is a string (`members`, `exclude`,
    /// `default-members`).
    StringArray,
    /// A plain string (`resolver`).
    Str,
    /// A table (`package`, `dependencies`, `lints`).
    Table,
}

/// Type contract for the known `[workspace]` fields.
///
/// Cargo type-checks each of these while parsing, so a wrong-typed one makes
/// the WHOLE manifest unloadable — not merely that field ignored. That matters
/// here because classifying a manifest as a virtual root is the FAIL-OPEN
/// direction: the walk passes it and the subtree inherits an outer package,
/// when in reality Cargo can load neither the workspace nor anything under it.
///
/// Every row was verified against real `cargo metadata --no-deps
/// --format-version 1` on the pinned toolchain (cargo 1.94.1), which rejects
/// each violation with `invalid type: … expected …`. Two fields are
/// deliberately ABSENT because Cargo accepts any type for them: `metadata`
/// (arbitrary user data), and every unrecognized key (a `future-tool-key`
/// loads fine) — rejecting either would un-attribute a real subtree.
///
/// HONEST BOUND: this checks the TYPE of a known field, not its VALUE. Cargo
/// validates deeper still — `[workspace.package] version = 1` is rejected as
/// "expected semver version", and `resolver = "9"` as an unknown resolver — and
/// re-implementing Cargo's manifest loader is out of scope for a local,
/// deterministic, `cargo`-free resolver. A manifest malformed in one of those
/// deeper ways is still walked past, exactly as the deny-list above can lag a
/// newer Cargo.
const WORKSPACE_FIELD_SHAPES: [(&str, WorkspaceFieldShape); 7] = [
    ("members", WorkspaceFieldShape::StringArray),
    ("exclude", WorkspaceFieldShape::StringArray),
    ("default-members", WorkspaceFieldShape::StringArray),
    ("resolver", WorkspaceFieldShape::Str),
    // The inheritance table: a non-table here is rejected by Cargo just as a
    // non-table top-level `package` is.
    ("package", WorkspaceFieldShape::Table),
    ("dependencies", WorkspaceFieldShape::Table),
    ("lints", WorkspaceFieldShape::Table),
];

/// Whether every KNOWN field of a `[workspace]` table has a type Cargo accepts.
///
/// An absent field is fine (all are optional); an unknown field is fine (Cargo
/// tolerates it). Only a present, known, wrong-typed field is disqualifying.
fn workspace_table_is_well_typed(workspace: &dyn toml_edit::TableLike) -> bool {
    WORKSPACE_FIELD_SHAPES.iter().all(|(field, shape)| {
        workspace.get(field).is_none_or(|item| match shape {
            WorkspaceFieldShape::StringArray => item
                .as_array()
                .is_some_and(|values| values.iter().all(toml_edit::Value::is_str)),
            WorkspaceFieldShape::Str => item.as_str().is_some(),
            WorkspaceFieldShape::Table => item.as_table_like().is_some(),
        })
    })
}

/// The TOML shape a known `[package]` field must have for Cargo to load the
/// manifest.
///
/// Several fields accept the workspace-INHERITANCE table (`version.workspace =
/// true`), which is ubiquitous in real workspaces — a rule that demanded the
/// direct type would un-attribute them wholesale, so every inheritable arm
/// admits a table.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PackageFieldShape {
    /// A string, with no inheritance form (`name`, `links`, `default-run`).
    Str,
    /// A string, or the inheritance table.
    StrOrInherited,
    /// An array whose every element is a string, or the inheritance table.
    StringArrayOrInherited,
    /// A string, a bool, or the inheritance table (`readme = false` is valid).
    StrBoolOrInherited,
    /// A bool, a string array, or the inheritance table (`publish`).
    BoolArrayOrInherited,
    /// A string or a bool, with no inheritance form (`build = false`).
    StrOrBool,
    /// A bool ONLY — no string, no inheritance table. Cargo's automatic-target
    /// switches (`autolib`, `autobins`, …) reject a table with "invalid type:
    /// map, expected a boolean", so they are the one checked family taking no
    /// `x.workspace = true` form.
    Bool,
}

/// Type contract for the known `[package]` fields.
///
/// Cargo type-checks these while parsing, so a wrong-typed one makes the WHOLE
/// manifest unloadable — the package it names does not exist, and attributing a
/// subtree to it would claim ownership by something that cannot be built. The
/// `[workspace]` analog is [`WORKSPACE_FIELD_SHAPES`]; this is the same rule on
/// the other table.
///
/// EVERY arm — accepted and rejected alike — was verified against real `cargo
/// metadata --no-deps --format-version 1` on the pinned toolchain (cargo
/// 1.94.1). The accepted ones matter as much: `readme = false`, `publish =
/// false`, `build = false`, an unknown key, and every `x.workspace = true`
/// inheritance form all load fine, and rejecting any of them would un-attribute
/// a real crate.
///
/// HONEST BOUND, unchanged from the workspace table: this checks the TYPE of a
/// known field, not its VALUE. Cargo also rejects `version = "notsemver"` and
/// `edition = "1066"`, which this resolver does not evaluate — it confirms a
/// manifest's shape rather than reimplementing Cargo's schema.
const PACKAGE_FIELD_SHAPES: [(&str, PackageFieldShape); 26] = [
    ("name", PackageFieldShape::Str),
    // The workspace POINTER (`workspace = "../.."`), naming the root this
    // package belongs to. Distinct from the `x.workspace = true` INHERITANCE
    // form, which appears as a table inside another field; Cargo rejects a
    // non-string here.
    ("workspace", PackageFieldShape::Str),
    ("links", PackageFieldShape::Str),
    ("default-run", PackageFieldShape::Str),
    ("version", PackageFieldShape::StrOrInherited),
    ("edition", PackageFieldShape::StrOrInherited),
    ("rust-version", PackageFieldShape::StrOrInherited),
    ("description", PackageFieldShape::StrOrInherited),
    ("homepage", PackageFieldShape::StrOrInherited),
    ("repository", PackageFieldShape::StrOrInherited),
    ("license", PackageFieldShape::StrOrInherited),
    ("license-file", PackageFieldShape::StrOrInherited),
    ("documentation", PackageFieldShape::StrOrInherited),
    ("readme", PackageFieldShape::StrBoolOrInherited),
    ("authors", PackageFieldShape::StringArrayOrInherited),
    ("keywords", PackageFieldShape::StringArrayOrInherited),
    ("categories", PackageFieldShape::StringArrayOrInherited),
    ("exclude", PackageFieldShape::StringArrayOrInherited),
    ("include", PackageFieldShape::StringArrayOrInherited),
    ("publish", PackageFieldShape::BoolArrayOrInherited),
    ("build", PackageFieldShape::StrOrBool),
    ("autolib", PackageFieldShape::Bool),
    ("autobins", PackageFieldShape::Bool),
    ("autoexamples", PackageFieldShape::Bool),
    ("autotests", PackageFieldShape::Bool),
    ("autobenches", PackageFieldShape::Bool),
];

/// SCOPE, verified rather than assumed: this checks the `[package]` TABLE's own
/// fields. Wrong-typed TOP-LEVEL sections are NOT checked, because Cargo
/// tolerates them — `lib = 1`, `bin = 1`, `features = 1`, `dependencies = 1`,
/// `profile = 1`, `badges = 1`, and `target = 1` beside a valid `[package]` all
/// load cleanly under `cargo metadata --no-deps` AND `cargo build` on the
/// pinned toolchain (1.94.1). Only a well-formed SECTION with a bad inner value
/// (`[lib]` with `name = 1`) is rejected, and that is value-level validation
/// this resolver deliberately does not reimplement. Adding a top-level section
/// check would reject manifests Cargo accepts and un-attribute real crates.
/// Whether every KNOWN field of a `[package]` table has a type Cargo accepts.
///
/// An absent field is fine; an unknown field is fine (Cargo tolerates it, as
/// verified). Only a present, known, wrong-typed field is disqualifying.
fn package_table_is_well_typed(package: &dyn toml_edit::TableLike) -> bool {
    fn is_string_array(item: &toml_edit::Item) -> bool {
        item.as_array()
            .is_some_and(|values| values.iter().all(toml_edit::Value::is_str))
    }
    PACKAGE_FIELD_SHAPES.iter().all(|(field, shape)| {
        package.get(field).is_none_or(|item| {
            // The workspace-inheritance form is a table; accepting any table
            // here is deliberate, since validating its contents is Cargo's job
            // and a false rejection un-attributes a real crate.
            let inherited = item.as_table_like().is_some();
            match shape {
                PackageFieldShape::Str => item.as_str().is_some(),
                PackageFieldShape::StrOrInherited => item.as_str().is_some() || inherited,
                PackageFieldShape::StringArrayOrInherited => is_string_array(item) || inherited,
                PackageFieldShape::StrBoolOrInherited => {
                    item.as_str().is_some() || item.as_bool().is_some() || inherited
                }
                PackageFieldShape::BoolArrayOrInherited => {
                    item.as_bool().is_some() || is_string_array(item) || inherited
                }
                PackageFieldShape::StrOrBool => item.as_str().is_some() || item.as_bool().is_some(),
                PackageFieldShape::Bool => item.as_bool().is_some(),
            }
        })
    })
}

/// The three captured dependency tables in documented output order./// The three captured dependency tables in documented output order.
const DEPENDENCY_KINDS: [DependencyKind; 3] = [
    DependencyKind::Normal,
    DependencyKind::Dev,
    DependencyKind::Build,
];

/// Lockfile resolution outcome for one declared dependency.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LockResolution {
    /// Exactly one version of the crate is listed in the lockfile.
    Locked(String),
    /// No `Cargo.lock` was found for the declaring manifest.
    NoLockfile,
    /// A lockfile exists but does not list the crate.
    NotInLockfile,
    /// The lockfile lists two or more versions of the crate; none is chosen.
    AmbiguousInLockfile,
    /// The nearest `Cargo.lock` exists but could not be read or parsed; an
    /// ancestor lockfile is never consulted in its place (PR #314 review).
    LockfileUnreadable,
    /// The lockfile lists the crate, but a parseable declared requirement is
    /// satisfied by none of the locked versions (e.g. a stale or shared
    /// lockfile holding only `foo 1.0.0` while the manifest declares
    /// `foo = "2"`); the mismatched version is never presented as `locked`
    /// (PR #314 review).
    RequirementUnsatisfiedInLockfile,
}

impl LockResolution {
    /// Returns the documented resolution marker.
    #[must_use]
    pub const fn marker(&self) -> &'static str {
        match self {
            Self::Locked(_) => "locked",
            Self::NoLockfile => "no_lockfile",
            Self::NotInLockfile => "not_in_lockfile",
            Self::AmbiguousInLockfile => "ambiguous_in_lockfile",
            Self::LockfileUnreadable => "lockfile_unreadable",
            Self::RequirementUnsatisfiedInLockfile => "requirement_unsatisfied_in_lockfile",
        }
    }

    /// Returns the resolved version for `locked` outcomes; `None` otherwise.
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Locked(version) => Some(version),
            _ => None,
        }
    }
}

/// Crate-name → locked-versions index parsed from one `Cargo.lock`.
#[derive(Debug, Default, Clone)]
pub struct LockfileIndex {
    versions: BTreeMap<String, BTreeSet<String>>,
}

/// Outcome of locating the nearest `Cargo.lock` for one manifest.
#[derive(Debug, Clone)]
pub enum LockfileStatus {
    /// The nearest lockfile was read and parsed cleanly.
    Found(LockfileIndex),
    /// The nearest `Cargo.lock` exists but could not be read or parsed. The
    /// search stops here: resolving from an unrelated ancestor lockfile would
    /// fabricate versions, so every dependency of the manifest is marked
    /// `lockfile_unreadable` instead (PR #314 review).
    Invalid,
    /// No `Cargo.lock` exists between the manifest and the repository root.
    Absent,
}

impl LockfileIndex {
    /// Parses a `Cargo.lock` body. Returns `None` when the lockfile is not
    /// valid TOML — callers mark the manifest's dependencies
    /// `lockfile_unreadable` rather than guessing versions or falling back
    /// to an ancestor lockfile.
    #[must_use]
    pub fn parse(lockfile_text: &str) -> Option<Self> {
        let doc = lockfile_text.parse::<toml_edit::DocumentMut>().ok()?;
        let mut versions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        if let Some(packages) = doc
            .get("package")
            .and_then(|item| item.as_array_of_tables())
        {
            for package in packages {
                let (Some(name), Some(version)) = (
                    package.get("name").and_then(|v| v.as_str()),
                    package.get("version").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                versions
                    .entry(name.to_owned())
                    .or_default()
                    .insert(version.to_owned());
            }
        }
        Some(Self { versions })
    }

    /// Resolves one crate name against the locked versions, honoring the
    /// declared version requirement on every path (PR #314 review).
    ///
    /// A parseable declared requirement is matched with Cargo semantics
    /// (`semver`, so `"1"` means `^1`) against **all** locked versions of the
    /// crate — including a sole locked version, which a stale or shared
    /// lockfile can leave unsatisfying: exactly one satisfying version is
    /// `locked`; none is `requirement_unsatisfied_in_lockfile`; several stay
    /// `ambiguous_in_lockfile`. Without a requirement (a pure `path`/`git`
    /// declaration), a sole locked version resolves directly and several
    /// stay `ambiguous_in_lockfile`. Extraction rejects unparseable
    /// requirement strings at declaration time (PR #314 review), so the
    /// unparseable arm of this fallback is a defensive boundary for direct
    /// callers, not a path extracted rows can reach. A resolved version is
    /// never fabricated.
    #[must_use]
    pub fn resolve(&self, crate_name: &str, declared_requirement: Option<&str>) -> LockResolution {
        let Some(versions) = self.versions.get(crate_name) else {
            return LockResolution::NotInLockfile;
        };
        let requirement = declared_requirement.and_then(|req| semver::VersionReq::parse(req).ok());
        if let Some(requirement) = requirement {
            // Unparseable locked versions never match (Cargo.lock versions
            // are generated semver, so this is a defensive boundary).
            let mut matches = versions.iter().filter(|version| {
                semver::Version::parse(version).is_ok_and(|parsed| requirement.matches(&parsed))
            });
            return match (matches.next(), matches.next()) {
                (Some(version), None) => LockResolution::Locked(version.clone()),
                (None, _) => LockResolution::RequirementUnsatisfiedInLockfile,
                (Some(_), Some(_)) => LockResolution::AmbiguousInLockfile,
            };
        }
        let mut iter = versions.iter();
        match (iter.next(), iter.next()) {
            (Some(version), None) => LockResolution::Locked(version.clone()),
            _ => LockResolution::AmbiguousInLockfile,
        }
    }
}

/// One directly-declared dependency parsed from a manifest.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeclaredDependency {
    /// Crate name (the `package` rename when present, else the table key).
    pub name: String,
    /// Manifest key the entry was declared under, when it differs from the
    /// crate name (Cargo `package = "…"` rename syntax). `None` for plain
    /// declarations.
    pub declared_as: Option<String>,
    /// Dependency table the declaration was written in.
    pub kind: DependencyKind,
    /// Version requirement string exactly as written; `None` when the
    /// declaration has no `version` key.
    pub declared_requirement: Option<String>,
    /// `true` for a `{ workspace = true }` entry: the real crate name and
    /// requirement live in the workspace root's `[workspace.dependencies]`
    /// table and are resolved at record-building time (PR #314 review).
    pub inherits_workspace: bool,
}

/// One `[workspace.dependencies]` entry's inheritance-relevant surface:
/// the optional `package = "…"` rename and the declared `version`
/// requirement (a plain string entry is its own version requirement).
#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct WorkspaceDepSpec {
    /// Real crate name when the entry renames (`package = "…"`).
    pub package: Option<String>,
    /// Declared version requirement; `None` for path/git-only templates.
    pub version: Option<String>,
    /// `false` when the root entry is present but Cargo-invalid — no usable
    /// string `version`/`path`/`git` source (`serde = {}`, wrong-typed
    /// `version = 1`, or a non-string non-table value). Inheriting members
    /// take the uninterpretable diagnostic path instead of fabricating a
    /// row (PR #314 review); the derived `Default` is deliberately unusable.
    pub usable: bool,
}

/// Parsed dependency surface of one manifest.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ManifestDependencies {
    /// `[package].name`; `None` for a virtual workspace manifest or when
    /// the declared name is empty/whitespace-only (Cargo-invalid, PR #314
    /// review) — such names never attribute declarations.
    pub package_name: Option<String>,
    /// Which of the three loadable/unloadable forms this manifest takes.
    ///
    /// Crate attribution (issue #117) needs more than `package_name`: that field
    /// is `None` for a virtual workspace root, for a `[package]` whose name is
    /// unusable, AND for a manifest Cargo refuses to load outright. Those demand
    /// different answers — walk past the first, stop fail-closed on the others —
    /// so the shape is reported as a closed set rather than reconstructed from
    /// a handful of booleans that could describe impossible combinations.
    pub shape: ManifestShape,
    /// Declarations in documented order: table order (`normal`, `dev`,
    /// `build`), then crate name, then the declared-as manifest key.
    pub declarations: Vec<DeclaredDependency>,
    /// `true` when at least one dependency table entry was neither a
    /// version string nor a dependency table (`serde = true` — a manifest
    /// Cargo rejects). The entry is skipped, never rendered as a row, and
    /// reported as a coverage hole (PR #314 review).
    pub uninterpretable: bool,
}

/// Parses the three captured dependency tables from a manifest body.
///
/// # Errors
///
/// Returns the TOML parse error message when the manifest is not valid TOML.
pub fn parse_manifest_dependencies(
    manifest_text: &str,
) -> std::result::Result<ManifestDependencies, String> {
    let doc = manifest_text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| error.to_string())?;
    let package_table = doc.get("package").and_then(toml_edit::Item::as_table_like);
    // Classifying a manifest as a virtual root is the FAIL-OPEN direction: the
    // attribution walk passes it and can attribute the subtree to an outer
    // package. So it requires POSITIVE confirmation of a loadable virtual root,
    // and every other shape falls through to `Unusable`, which stops the walk.
    //
    // Enumerating the ways a manifest can be malformed is open-ended — a
    // `package` key present but not a table reads as "package-less" to a
    // table-only lookup even though Cargo rejects it — so the default for the
    // dangerous branch is deliberately inverted rather than patched per case.
    //
    // A confirmed virtual root has: no `package` key AT ALL (a present one, in
    // any shape, means the manifest is trying to declare a package), a
    // `workspace` TABLE whose every KNOWN field is well-typed
    // ([`WORKSPACE_FIELD_SHAPES`] — a wrong-typed one makes the whole manifest
    // unloadable, not merely that field ignored), and none of the package-only
    // sections Cargo forbids beside it ("this virtual manifest specifies a
    // `<section>` section, which is not allowed" — each verified against real
    // `cargo metadata`, while `[profile]`, `[patch]`, and `[replace]` were
    // verified accepted).
    // A `[package]` whose known fields are wrong-typed is one Cargo refuses to
    // load, so the package it names does not exist and its subtree must NOT be
    // attributed to it — the same fail-closed treatment a malformed
    // `[workspace]` gets.
    let package_well_typed = package_table.is_some_and(package_table_is_well_typed);
    // `cargo-features` is TOP-LEVEL and applies to BOTH shapes: Cargo requires
    // an array of strings and rejects anything else with "expected a sequence",
    // so a manifest carrying a malformed one is unloadable whether it declares a
    // package or a virtual root. This is the one top-level field checked here —
    // `lib`, `bin`, `features`, `dependencies`, `profile`, `badges`, and
    // `target` are all TOLERATED as scalars by Cargo (verified), so rejecting
    // them would un-attribute crates that build fine. An unknown feature NAME is
    // also a Cargo error, but that is value-level validation this resolver does
    // not cross.
    let cargo_features_well_typed = doc.get("cargo-features").is_none_or(|item| {
        item.as_array()
            .is_some_and(|values| values.iter().all(toml_edit::Value::is_str))
    });
    let shape = if !cargo_features_well_typed {
        ManifestShape::Unusable
    } else if package_table.is_some() {
        if package_well_typed {
            ManifestShape::Package
        } else {
            ManifestShape::Unusable
        }
    } else if doc.get("package").is_none()
        && doc
            .get("workspace")
            .and_then(toml_edit::Item::as_table_like)
            .is_some_and(workspace_table_is_well_typed)
        && !VIRTUAL_MANIFEST_FORBIDDEN_SECTIONS
            .iter()
            .any(|section| doc.get(section).is_some())
    {
        ManifestShape::VirtualRoot
    } else {
        ManifestShape::Unusable
    };
    let package_name = package_table
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        // A name Cargo rejects — empty, or violating the package-name
        // rule (`"bad name"`) — is no usable name: a row attributed to it
        // would be a fabricated fact, so such manifests take the existing
        // unattributable-manifest diagnostic path (PR #314 review).
        .filter(|name| package_name_is_valid(name))
        .map(str::to_owned);

    let mut declarations = Vec::new();
    let mut uninterpretable = false;
    for kind in DEPENDENCY_KINDS {
        let Some(table) = doc
            .get(kind.table())
            .and_then(toml_edit::Item::as_table_like)
        else {
            continue;
        };
        let mut entries: Vec<DeclaredDependency> = table
            .iter()
            .filter_map(|(key, item)| {
                let entry = declared_dependency(key, item, kind);
                if entry.is_none() {
                    // Invalid entry (neither version string nor table):
                    // skipped, reported, never fabricated (PR #314 review).
                    uninterpretable = true;
                }
                entry
            })
            .collect();
        // One fact per declared entry: TOML keys are unique per table, and a
        // `package = "…"` rename legitimately declares a second version of
        // the same crate (PR #314 review) — entries are never collapsed.
        entries.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.declared_as.cmp(&right.declared_as))
        });
        declarations.extend(entries);
    }
    Ok(ManifestDependencies {
        package_name,
        shape,
        declarations,
        uninterpretable,
    })
}

/// Where a dependency table sits, for the context-dependent key rules
/// Cargo enforces (each verified against `cargo metadata`, PR #314 review).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum DependencyTableContext {
    /// A member manifest's dependency table entry. `optional = true` is
    /// legal only where the table kind allows it (normal and build tables;
    /// dev dependencies cannot be optional).
    Member { optional_true_allowed: bool },
    /// A `[workspace.dependencies]` template entry. `optional = true` is
    /// rejected (workspace dependencies cannot be optional); the
    /// `workspace` key is IGNORED by Cargo entirely — any value, even a
    /// wrong-typed one, is tolerated like an unknown key (verified).
    Template,
}

impl DependencyTableContext {
    /// Context for one member dependency table by its kind.
    const fn member(kind: DependencyKind) -> Self {
        Self::Member {
            optional_true_allowed: !matches!(kind, DependencyKind::Dev),
        }
    }
}

/// Validates the KNOWN dependency-table keys' types and allowed values per
/// Cargo's rejection behavior (PR #314 review): the string sources and
/// refinements (`version`/`path`/`git`/`registry`/`branch`/`tag`/`rev`/
/// `package`) must be strings, `optional`/`default-features` (and the
/// deprecated `default_features` spelling) must be booleans, `features`
/// must be an array of strings, and — in member tables — `workspace` may
/// only be the literal `true` (`workspace = false` is Cargo-invalid there),
/// while a template's `workspace` key is ignored by Cargo entirely. Cargo
/// rejects only the VALUE `optional = true` where optionality is
/// disallowed (dev dependencies and workspace templates); `optional =
/// false` is accepted everywhere the key is bool-typed (verified — PR #314
/// review). Unknown keys are tolerated: Cargo warns but loads the
/// manifest.
fn dependency_table_is_well_typed(
    spec: &dyn toml_edit::TableLike,
    context: DependencyTableContext,
) -> bool {
    // Cross-field source rules Cargo enforces (each verified against
    // `cargo metadata`, PR #314 review): `path` and `git` are mutually
    // exclusive, `git` and `registry` are mutually exclusive, and
    // `branch`/`tag`/`rev` require `git` with at most one of the three.
    // `registry` beside `version` or `path` is manifest-valid — Cargo only
    // checks registry *configuration* later — and stays accepted.
    let has = |key: &str| spec.get(key).is_some();
    let git_refs = ["branch", "tag", "rev"]
        .iter()
        .filter(|key| has(key))
        .count();
    if (has("path") && has("git"))
        || (has("git") && has("registry"))
        || (git_refs > 0 && !has("git"))
        || git_refs > 1
    {
        return false;
    }
    spec.iter().all(|(key, item)| match key {
        "version" | "path" | "git" | "registry" | "branch" | "tag" | "rev" | "package" => {
            item.as_str().is_some()
        }
        "workspace" => match context {
            DependencyTableContext::Member { .. } => item.as_bool() == Some(true),
            DependencyTableContext::Template => true,
        },
        "optional" => match item.as_bool() {
            Some(false) => true,
            Some(true) => matches!(
                context,
                DependencyTableContext::Member {
                    optional_true_allowed: true
                }
            ),
            None => false,
        },
        "default-features" | "default_features" => item.as_bool().is_some(),
        "features" => item
            .as_array()
            .is_some_and(|array| array.iter().all(|value| value.as_str().is_some())),
        _ => true,
    })
}

/// Returns whether a declared version requirement string is one Cargo
/// accepts (the `semver::VersionReq` grammar — `"1"`, `"^0.2"`,
/// `">=1, <2"`, `"*"`). An unparseable requirement (`"not a req"`) is a
/// manifest Cargo rejects: the declaring entry takes the uninterpretable
/// diagnostic path instead of being recorded, and never resolves against a
/// lockfile (PR #314 review).
fn requirement_is_parseable(requirement: &str) -> bool {
    semver::VersionReq::parse(requirement).is_ok()
}

/// Cargo's package-name validity rule at manifest load, verified against
/// `cargo metadata` (PR #314 review): the first character must be a Unicode
/// XID start character or `_` (never a digit or `-`), and every following
/// character a Unicode XID continue character or `-`. Non-ASCII letters and
/// uppercase are valid; spaces, `.`, `!`, a leading digit, or a leading `-`
/// are rejection-level errors. Applies to `[package].name`, dependency
/// table keys, and `package = "…"` rename values alike — Cargo rejects all
/// three the same way. Rejection-level rules only; crates.io publish-time
/// restrictions and Cargo warnings are out of scope.
///
/// Crate-visible so the attribution READER
/// ([`crate::ir::CrateAttribution::owning_package`]) gates a package name read
/// back from a store against the same rule that gated it at production —
/// re-deriving the charset there would let the two drift.
pub(crate) fn package_name_is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (unicode_ident::is_xid_start(first) || first == '_')
        && chars.all(|c| unicode_ident::is_xid_continue(c) || c == '-')
}

/// Interprets one dependency table entry.
///
/// `serde = "1"` declares requirement `"1"`; `serde = { version = "1", .. }`
/// declares the `version` key; a `package = "real-name"` key renames the
/// entry, so the *crate* name is the `package` value. A declaration without a
/// `version` key (pure `path`/`git`/`workspace = true`) carries no
/// requirement — nothing is fabricated. An entry that is neither a version
/// string nor a dependency table (`serde = true`), a table without a
/// usable source — a `version`/`path`/`git` string or `workspace = true` —
/// and a table whose KNOWN keys are wrong-typed or carry disallowed values
/// (`version = 1` even beside a valid `path`, `workspace = false`, an
/// `optional = true` dev-dependency, or an unparseable requirement string like
/// `"not a req"`) are manifests Cargo rejects: `None`, no row is
/// fabricated, and the caller reports the coverage hole (PR #314 review).
fn declared_dependency(
    key: &str,
    item: &toml_edit::Item,
    kind: DependencyKind,
) -> Option<DeclaredDependency> {
    let mut name = key.to_owned();
    let mut declared_as = None;
    let mut declared_requirement = None;
    let mut inherits_workspace = false;
    if let Some(requirement) = item.as_str() {
        declared_requirement = Some(requirement.to_owned());
    } else if let Some(spec) = item.as_table_like() {
        // Any known key with a wrong type or disallowed value poisons the
        // whole entry — Cargo rejects the manifest even when another
        // source field is valid (PR #314 review). Dev dependencies cannot
        // be optional (`optional = true`); normal and build dependencies
        // can, and `optional = false` is legal everywhere.
        if !dependency_table_is_well_typed(spec, DependencyTableContext::member(kind)) {
            return None;
        }
        if let Some(package) = spec.get("package").and_then(|v| v.as_str()) {
            package.clone_into(&mut name);
            declared_as = Some(key.to_owned());
        }
        if let Some(version) = spec.get("version").and_then(|v| v.as_str()) {
            declared_requirement = Some(version.to_owned());
        }
        inherits_workspace = spec
            .get("workspace")
            .and_then(toml_edit::Item::as_bool)
            .unwrap_or(false);
        // A dependency table must name a usable source (Cargo's acceptance
        // boundary): a string `version`, `path`, or `git`, or
        // `workspace = true`. `registry`/`rev`/`branch`/`tag` only refine a
        // `git`/`version` source and never stand alone.
        let has_source = |field: &str| spec.get(field).and_then(|v| v.as_str()).is_some();
        if declared_requirement.is_none()
            && !inherits_workspace
            && !has_source("path")
            && !has_source("git")
        {
            return None;
        }
    } else {
        return None;
    }
    // A requirement string Cargo cannot parse is a rejected manifest —
    // never a recorded declaration and never a lockfile resolution
    // (PR #314 review). Covers plain-string entries and table `version`s.
    if let Some(requirement) = declared_requirement.as_deref()
        && !requirement_is_parseable(requirement)
    {
        return None;
    }
    // A dependency NAME Cargo rejects — empty (a quoted empty table key,
    // `"" = "1"`), blank, or violating the package-name rule (`"1foo"`,
    // `package = "foo.bar"`) — never becomes a row: Cargo applies the same
    // validity rule to dependency keys and rename values as to
    // `[package].name` (verified, PR #314 review).
    if !package_name_is_valid(&name) {
        return None;
    }
    Some(DeclaredDependency {
        name,
        declared_as,
        kind,
        declared_requirement,
        inherits_workspace,
    })
}

/// Builds the `DependencyDeclaration` (and manifest `Diagnostic`) records for
/// one manifest body.
///
/// A manifest that fails TOML parsing yields exactly one `Diagnostic` node
/// citing the manifest handle — the scan never fails and never invents facts.
/// A parseable manifest without a `[package]` table (a virtual workspace
/// root) legitimately declares nothing and yields no records.
#[must_use]
pub fn manifest_dependency_records(
    repository_id: &str,
    manifest_path: &str,
    manifest_text: &str,
    lockfile: &LockfileStatus,
    workspace_deps: Option<&BTreeMap<String, WorkspaceDepSpec>>,
) -> Vec<GraphRecord> {
    let Ok(parsed) = parse_manifest_dependencies(manifest_text) else {
        return vec![unparseable_manifest_diagnostic(
            repository_id,
            manifest_path,
        )];
    };
    let Some(declaring_package) = parsed.package_name else {
        if parsed.declarations.is_empty() && !parsed.uninterpretable {
            // A true virtual workspace manifest declares nothing: no rows,
            // no diagnostic.
            return Vec::new();
        }
        // Dependency tables without a usable `[package].name` cannot be
        // attributed to a declaring package: a silent skip would be a
        // coverage hole, so the manifest is reported like the unparseable
        // case, under its own discriminator (PR #314 review).
        return vec![unattributable_manifest_diagnostic(
            repository_id,
            manifest_path,
        )];
    };
    let (declarations, uninheritable, inherited_uninterpretable) =
        resolve_workspace_inheritance(parsed.declarations, workspace_deps);
    let mut records: Vec<GraphRecord> = declarations
        .iter()
        .map(|declaration| {
            dependency_record(
                repository_id,
                manifest_path,
                &declaring_package,
                declaration,
                lockfile,
            )
        })
        .collect();
    if uninheritable {
        records.push(uninheritable_dependency_diagnostic(
            repository_id,
            manifest_path,
        ));
    }
    // Own invalid entries and inherited entries whose root spec exists but
    // is Cargo-invalid share the uninterpretable discriminator (PR #314
    // review): both are declarations that cannot be interpreted, distinct
    // from a root entry that is missing entirely (uninheritable above).
    if parsed.uninterpretable || inherited_uninterpretable {
        records.push(uninterpretable_dependency_diagnostic(
            repository_id,
            manifest_path,
        ));
    }
    records
}

/// Resolves `{ workspace = true }` entries through the workspace root's
/// `[workspace.dependencies]` table (PR #314 review): the real crate name is
/// the root entry's `package` (else the shared key), the requirement is the
/// root entry's `version` (a path/git-only template has none — the existing
/// no-requirement semantics apply), and `declared_as` records the member key
/// when it differs from the real name. A member-local `version` beside
/// `workspace = true` (which Cargo rejects) never overrides the root's.
/// Entries with no resolvable root context (standalone manifest, or a key
/// missing from the root table) are dropped — a key-named row would be a
/// fabrication — and reported via the first returned flag (uninheritable).
/// A root entry that EXISTS but is Cargo-invalid (no usable string
/// `version`/`path`/`git` source) is dropped the same way and reported via
/// the second flag: an unusable spec is an uninterpretable declaration,
/// distinct from a missing one (PR #314 review).
fn resolve_workspace_inheritance(
    declarations: Vec<DeclaredDependency>,
    workspace_deps: Option<&BTreeMap<String, WorkspaceDepSpec>>,
) -> (Vec<DeclaredDependency>, bool, bool) {
    let mut resolved = Vec::with_capacity(declarations.len());
    let mut uninheritable = false;
    let mut uninterpretable = false;
    for mut declaration in declarations {
        if !declaration.inherits_workspace {
            resolved.push(declaration);
            continue;
        }
        // The lookup uses the original MANIFEST KEY: Cargo accepts
        // `alias = { workspace = true, package = "serde" }`, and the parse
        // has already rewritten `name` to the member-side `package` — but
        // the ROOT template alone determines the real crate name, and a
        // member-side `package` beside `workspace = true` is ignored by
        // Cargo entirely (verified against `cargo metadata`; pinned,
        // PR #314 review).
        let key = declaration
            .declared_as
            .clone()
            .unwrap_or_else(|| declaration.name.clone());
        let Some(spec) = workspace_deps.and_then(|deps| deps.get(&key)) else {
            uninheritable = true;
            continue;
        };
        if !spec.usable {
            uninterpretable = true;
            continue;
        }
        let real_name = spec.package.clone().unwrap_or_else(|| key.clone());
        // The root template's rename must itself be a Cargo-valid name
        // (PR #314 review) — an invalid one never fabricates a row.
        if !package_name_is_valid(&real_name) {
            uninterpretable = true;
            continue;
        }
        declaration.declared_as = (real_name != key).then_some(key);
        declaration.name = real_name;
        declaration.declared_requirement.clone_from(&spec.version);
        resolved.push(declaration);
    }
    // Restore the documented per-kind (name, declared_as) order — an
    // inherited rename can change the sort key computed at parse time.
    resolved.sort_by(|left, right| {
        left.kind.cmp(&right.kind).then_with(|| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.declared_as.cmp(&right.declared_as))
        })
    });
    (resolved, uninheritable, uninterpretable)
}

fn dependency_record(
    repository_id: &str,
    manifest_path: &str,
    declaring_package: &str,
    declaration: &DeclaredDependency,
    lockfile: &LockfileStatus,
) -> GraphRecord {
    let resolution = match lockfile {
        LockfileStatus::Found(index) => index.resolve(
            &declaration.name,
            declaration.declared_requirement.as_deref(),
        ),
        LockfileStatus::Invalid => LockResolution::LockfileUnreadable,
        LockfileStatus::Absent => LockResolution::NoLockfile,
    };
    // The declared-as manifest key joins the identity when present so a
    // `package = "…"` rename pair yields two distinct stable IDs; plain
    // declarations keep the historical identity inputs.
    let mut id_parts = vec![
        "node",
        "dependency-declaration",
        repository_id,
        manifest_path,
        declaring_package,
        declaration.kind.as_str(),
        &declaration.name,
    ];
    if let Some(declared_as) = declaration.declared_as.as_deref() {
        id_parts.push(declared_as);
    }
    let id = stable_id(&id_parts);
    let requirement_text = declaration
        .declared_requirement
        .as_deref()
        .unwrap_or("(no version requirement)");
    let resolved_text = match &resolution {
        LockResolution::Locked(version) => format!("locked {version}"),
        other => other.marker().to_owned(),
    };
    let declared_as_text = declaration
        .declared_as
        .as_deref()
        .map(|key| format!(" (declared as {key})"))
        .unwrap_or_default();
    let summary = format!(
        "Cargo dependency {name}{declared_as_text} ({kind}) declared by {declaring_package} in {manifest_path}: requirement {requirement_text}, {resolved_text}",
        name = declaration.name,
        kind = declaration.kind.as_str(),
    );
    GraphRecord::node(
        id,
        NodeKind::DependencyDeclaration,
        Some(manifest_path.to_owned()),
        None,
        Some(declaration.name.clone()),
        summary,
    )
    .with_dependency(DependencyDeclarationPayload {
        declaring_package: declaring_package.to_owned(),
        dependency_kind: declaration.kind.as_str().to_owned(),
        declared_as: declaration.declared_as.clone(),
        declared_requirement: declaration.declared_requirement.clone(),
        resolved_version: resolution.version().map(str::to_owned),
        resolution: resolution.marker().to_owned(),
    })
}

/// Skipped-manifest `symbol_kind` discriminator (PR #314 review).
///
/// Stamped on the `Diagnostic` node emitted for an unreadable/unparseable
/// manifest so query surfaces can recognize skipped-manifest coverage holes
/// without matching summary text.
pub const SKIPPED_MANIFEST_DIAGNOSTIC_KIND: &str = "unparseable_cargo_manifest";

/// Skipped-manifest `symbol_kind` for unresolvable `{ workspace = true }`
/// entries (PR #314 review).
///
/// Stamped when a manifest inherits workspace dependencies but no owning
/// workspace root (or no matching `[workspace.dependencies]` key) exists to
/// resolve them: the affected declarations are skipped — a key-named row
/// would be a fabrication — and the answer stays qualified. Other
/// declarations in the same manifest still extract normally.
pub const UNINHERITABLE_MANIFEST_DIAGNOSTIC_KIND: &str = "uninheritable_cargo_dependency";

/// Skipped-manifest `symbol_kind` for invalid dependency table entries
/// (PR #314 review).
///
/// Stamped when a dependency table entry is neither a version string nor a
/// dependency table (`serde = true` — a manifest Cargo rejects): the entry
/// is skipped — a key-named row with no requirement would be a
/// fabrication — and the answer stays qualified. Valid sibling entries in
/// the same manifest still extract normally.
pub const UNINTERPRETABLE_DEPENDENCY_DIAGNOSTIC_KIND: &str = "uninterpretable_cargo_dependency";

/// Skipped-manifest `symbol_kind` for an unloadable workspace root
/// (PR #314 review).
///
/// Stamped on the ROOT manifest whose member resolution Cargo rejects
/// outright — a member directory without `Cargo.toml`, a missing literal
/// member, or a glob matching nothing ("failed to load manifest for
/// workspace member", verified). No member uses the root's lockfile or
/// `[workspace.dependencies]`; all fall back to honest standalone behavior
/// and every answer stays qualified.
pub const UNLOADABLE_WORKSPACE_DIAGNOSTIC_KIND: &str = "unloadable_cargo_workspace";

/// Skipped-manifest `symbol_kind` for an unattributable manifest (PR #314
/// review).
///
/// Stamped when dependency tables exist but no usable `[package].name` does
/// (a name-less package table, or a virtual manifest wrongly carrying
/// top-level dependencies). Reported like the unparseable case so answers
/// stay qualified, under its own discriminator.
pub const UNATTRIBUTABLE_MANIFEST_DIAGNOSTIC_KIND: &str = "unattributable_cargo_manifest";

/// Attaches a skipped-manifest `Diagnostic` to its repository so a shared
/// multi-repo store can scope and label the coverage hole (PR #314 review).
fn diagnostic_repo_edge(repository_id: &str, diagnostic_id: &str) -> GraphRecord {
    GraphRecord::edge(
        EdgeLabel::Contains,
        repository_id.to_owned(),
        diagnostic_id.to_owned(),
        Some("1.0".to_owned()),
        "Repository contains skipped-manifest diagnostic".to_owned(),
    )
}

fn unparseable_manifest_diagnostic(repository_id: &str, manifest_path: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            "unparseable-cargo-manifest",
            repository_id,
            manifest_path,
        ]),
        NodeKind::Diagnostic,
        Some(manifest_path.to_owned()),
        None,
        Some(manifest_path.to_owned()),
        format!("Unparseable Cargo manifest {manifest_path}: dependency declarations skipped"),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(SKIPPED_MANIFEST_DIAGNOSTIC_KIND.to_owned());
    }
    record
}

/// Diagnostic for a parseable manifest with dependency tables but no usable
/// `[package].name` (PR #314 review): the declarations cannot be attributed
/// to a declaring package, so the manifest is reported as a coverage hole —
/// never a silent skip and never an invented package name.
fn unattributable_manifest_diagnostic(repository_id: &str, manifest_path: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            "unattributable-cargo-manifest",
            repository_id,
            manifest_path,
        ]),
        NodeKind::Diagnostic,
        Some(manifest_path.to_owned()),
        None,
        Some(manifest_path.to_owned()),
        format!(
            "Cargo manifest {manifest_path} declares dependencies without a usable [package].name: declarations skipped"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(UNATTRIBUTABLE_MANIFEST_DIAGNOSTIC_KIND.to_owned());
    }
    record
}

/// Diagnostic for `{ workspace = true }` entries with no resolvable
/// workspace context (PR #314 review): the affected declarations are
/// skipped — never rendered as key-named rows — and the coverage hole is
/// reported so answers stay qualified.
fn uninheritable_dependency_diagnostic(repository_id: &str, manifest_path: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            "uninheritable-cargo-dependency",
            repository_id,
            manifest_path,
        ]),
        NodeKind::Diagnostic,
        Some(manifest_path.to_owned()),
        None,
        Some(manifest_path.to_owned()),
        format!(
            "Cargo manifest {manifest_path} inherits workspace dependencies with no resolvable workspace root: those declarations skipped"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(UNINHERITABLE_MANIFEST_DIAGNOSTIC_KIND.to_owned());
    }
    record
}

/// Diagnostic for dependency table entries that are neither a version
/// string nor a dependency table (PR #314 review): Cargo rejects such
/// manifests, so the entry is skipped — never rendered as a row — and the
/// coverage hole is reported so answers stay qualified.
fn uninterpretable_dependency_diagnostic(repository_id: &str, manifest_path: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            "uninterpretable-cargo-dependency",
            repository_id,
            manifest_path,
        ]),
        NodeKind::Diagnostic,
        Some(manifest_path.to_owned()),
        None,
        Some(manifest_path.to_owned()),
        format!(
            "Cargo manifest {manifest_path} declares dependency entries that are neither a version string nor a dependency table: those declarations skipped"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(UNINTERPRETABLE_DEPENDENCY_DIAGNOSTIC_KIND.to_owned());
    }
    record
}

/// Diagnostic for a `[workspace]` root whose member resolution Cargo
/// rejects (PR #314 review): the whole workspace is unloadable, so no
/// member resolves through it — a coverage hole reported on the root
/// manifest, never a silent skip.
fn unloadable_workspace_diagnostic(repository_id: &str, manifest_path: &str) -> GraphRecord {
    let mut record = GraphRecord::node(
        stable_id(&[
            "node",
            "diagnostic",
            "unloadable-cargo-workspace",
            repository_id,
            manifest_path,
        ]),
        NodeKind::Diagnostic,
        Some(manifest_path.to_owned()),
        None,
        Some(manifest_path.to_owned()),
        format!(
            "Cargo workspace at {manifest_path} is unloadable (a member fails to resolve): no member uses its lockfile or workspace dependencies"
        ),
    );
    if let GraphRecord::Node { symbol_kind, .. } = &mut record {
        *symbol_kind = Some(UNLOADABLE_WORKSPACE_DIAGNOSTIC_KIND.to_owned());
    }
    record
}

/// Harvests one owning-package fact per `Cargo.toml` in the working tree
/// (issue #117).
///
/// The current-tree half of crate attribution: it discovers manifests through
/// the SAME [`discover_cargo_manifests`] walk `eg scan` already uses (so the
/// `target/`, `.git`, and nested-worktree exclusions apply identically), reads
/// each one, and reduces it to a closed [`ManifestParseOutcome`]. The pure
/// resolver in `crate::crate_attribution` then answers per-path from these
/// facts alone.
///
/// Deliberately distinct from [`scan_dependency_records`]: that function mints
/// a `File` node only for a manifest that DECLARES dependencies, so a
/// dependency-free member crate produces no graph record at all — yet it still
/// owns its directory tree. Attribution must see every manifest, not only the
/// dependency-declaring ones.
///
/// Reading, not building: no `cargo` invocation, no network, no lockfile.
///
/// # Errors
///
/// Returns an error only when manifest discovery itself fails. An individual
/// manifest that cannot be read or parsed becomes a fact carrying
/// [`ManifestParseOutcome::Unreadable`] / [`ManifestParseOutcome::Unparseable`],
/// never an aborted scan.
pub fn scan_manifest_package_facts(repo_root: &Path) -> Result<Vec<ManifestPackageFact>> {
    let mut facts = Vec::new();
    for manifest in discover_cargo_manifests(repo_root)? {
        let outcome = std::fs::read_to_string(&manifest.path)
            .map_or(ManifestParseOutcome::Unreadable, |text| {
                manifest_package_outcome(&text)
            });
        facts.push(ManifestPackageFact::new(
            manifest.repo_relative_path.clone(),
            outcome,
        ));
    }
    Ok(facts)
}

/// Reduces one manifest's TEXT to its closed owning-package outcome.
///
/// The single shared reduction: the working-tree harvest above and the
/// history-replay harvest (which reads manifest bytes from Git objects) both
/// call it, so the two paths cannot disagree about what a manifest declares.
#[must_use]
pub fn manifest_package_outcome(manifest_text: &str) -> ManifestParseOutcome {
    match parse_manifest_dependencies(manifest_text) {
        Ok(parsed) => match (parsed.shape, parsed.package_name) {
            (ManifestShape::Package, Some(name)) => ManifestParseOutcome::Package { name },
            (ManifestShape::Package, None) => ManifestParseOutcome::UnnamedPackage,
            (ManifestShape::VirtualRoot, _) => ManifestParseOutcome::Virtual,
            (ManifestShape::Unusable, _) => ManifestParseOutcome::UnusableManifest,
        },
        // The TOML error message is deliberately DROPPED, not carried: it can
        // echo manifest body text, and no output surface may leak it.
        Err(_) => ManifestParseOutcome::Unparseable,
    }
}

/// Scans every `Cargo.toml` under `repo_root` into dependency records.
///
/// Manifests are visited in deterministic repo-relative path order. Each
/// manifest resolves against the nearest `Cargo.lock` walking up from its own
/// directory to the repository root — the standard workspace layout keeps one
/// root lockfile shared by every member crate.
///
/// # Errors
///
/// Returns an error when manifest discovery cannot read the filesystem.
pub fn scan_dependency_records(repo_root: &Path, repository_id: &str) -> Result<Vec<GraphRecord>> {
    let mut records = Vec::new();
    let mut lockfile_cache = LockfileWalkCache::default();
    for manifest in discover_cargo_manifests(repo_root)? {
        let Ok(manifest_text) = std::fs::read_to_string(&manifest.path) else {
            // An unreadable manifest is reported like an unparseable one:
            // a diagnostic fact, never a failed scan.
            let diagnostic =
                unparseable_manifest_diagnostic(repository_id, &manifest.repo_relative_path);
            records.push(diagnostic_repo_edge(repository_id, diagnostic.id()));
            records.push(diagnostic);
            continue;
        };
        let lockfile =
            nearest_lockfile(repo_root, &manifest.repo_relative_path, &mut lockfile_cache);
        let workspace_deps =
            workspace_dep_specs_for(repo_root, &manifest.repo_relative_path, &mut lockfile_cache);
        // An unloadable workspace is a coverage hole reported on its ROOT
        // manifest (PR #314 review): Cargo rejects the whole workspace, so
        // no member resolves through it and every answer stays qualified.
        let mut manifest_dir: Vec<&str> = manifest.repo_relative_path.split('/').collect();
        manifest_dir.pop();
        if matches!(
            workspace_facts_in_dir(repo_root, &manifest_dir, &mut lockfile_cache),
            WorkspaceFacts::UnloadableWorkspace
        ) {
            let diagnostic =
                unloadable_workspace_diagnostic(repository_id, &manifest.repo_relative_path);
            records.push(diagnostic_repo_edge(repository_id, diagnostic.id()));
            records.push(diagnostic);
        }
        let manifest_records = manifest_dependency_records(
            repository_id,
            &manifest.repo_relative_path,
            &manifest_text,
            &lockfile,
            workspace_deps.as_ref(),
        );
        // Repository attribution topology (PR #314 review): a manifest that
        // declares dependencies gets a `File` node plus the exact
        // `Repository —CONTAINS→ File —CONTAINS→ DependencyDeclaration` chain
        // `RepositoryIndex` walks for ownership, so a shared multi-repo store
        // can scope and label every dependency fact.
        let dependency_ids: Vec<String> = manifest_records
            .iter()
            .filter(|record| record.node_kind_name() == Some("DependencyDeclaration"))
            .map(|record| record.id().to_owned())
            .collect();
        // Skipped-manifest Diagnostic records join the repository topology
        // (Repository —CONTAINS→ Diagnostic) so `RepositoryIndex` can own
        // them and `--repo` scoping applies (PR #314 review).
        for record in &manifest_records {
            if record.node_kind_name() == Some("Diagnostic") {
                records.push(diagnostic_repo_edge(repository_id, record.id()));
            }
        }
        if dependency_ids.is_empty() {
            records.extend(manifest_records);
        } else {
            let file_id = stable_id(&["node", "file", repository_id, &manifest.repo_relative_path]);
            // Handle-only summary: the manifest body is never embedded.
            records.push(GraphRecord::node(
                file_id.clone(),
                NodeKind::File,
                Some(manifest.repo_relative_path.clone()),
                None,
                Some(manifest.repo_relative_path.clone()),
                format!("Cargo manifest {}", manifest.repo_relative_path),
            ));
            records.push(GraphRecord::edge(
                EdgeLabel::Contains,
                repository_id.to_owned(),
                file_id.clone(),
                Some("1.0".to_owned()),
                "Repository contains manifest file".to_owned(),
            ));
            records.extend(manifest_records);
            for dependency_id in dependency_ids {
                records.push(GraphRecord::edge(
                    EdgeLabel::Contains,
                    file_id.clone(),
                    dependency_id,
                    Some("1.0".to_owned()),
                    "Manifest file contains dependency declaration".to_owned(),
                ));
            }
        }
    }
    Ok(records)
}

/// What one ancestor directory's `Cargo.toml` says about workspace roots
/// (PR #314 review: the lockfile walk respects workspace boundaries).
#[derive(Debug, Clone)]
enum WorkspaceFacts {
    /// No `Cargo.toml` beside the candidate lockfile: a stray lockfile that
    /// Cargo would never attribute; the walk skips past it.
    NoManifest,
    /// The manifest exists but cannot be read or parsed: membership cannot be
    /// verified, so the walk stops without accepting the lockfile.
    Unverifiable,
    /// A plain package manifest (no `[workspace]` table): its lockfile covers
    /// only itself; Cargo's workspace discovery walks past it.
    PackageOnly,
    /// A `[workspace]` root whose member resolution Cargo rejects outright
    /// ("failed to load manifest for workspace member", verified): a glob or
    /// literal member resolving to a directory without `Cargo.toml`, a
    /// missing literal member, or a glob matching nothing at all. The whole
    /// workspace is unloadable — its lockfile and `[workspace.dependencies]`
    /// are never used by ANY member, which fall back to honest standalone
    /// behavior, and the walk still stops at this `[workspace]` boundary
    /// (PR #314 review).
    UnloadableWorkspace,
    /// A workspace root: `members` / `exclude` globs plus the root package's
    /// in-tree `path = "…"` dependencies (transitively — Cargo treats them as
    /// automatic members) decide coverage, and `dep_specs` carries the
    /// `[workspace.dependencies]` inheritance surface members resolve
    /// `{ workspace = true }` entries against (PR #314 review).
    Workspace {
        members: Vec<String>,
        exclude: Vec<String>,
        path_members: Vec<String>,
        dep_specs: BTreeMap<String, WorkspaceDepSpec>,
    },
}

/// Per-directory caches for the lockfile walk: what lockfile the directory
/// holds and what its manifest says about workspace membership.
#[derive(Debug, Default)]
struct LockfileWalkCache {
    lockfiles: BTreeMap<String, LockfileStatus>,
    workspaces: BTreeMap<String, WorkspaceFacts>,
    /// Per-directory `[package].workspace` pointer strings (PR #314 review).
    pointers: BTreeMap<String, Option<String>>,
}

/// Finds and parses the `Cargo.lock` Cargo would actually use for a manifest,
/// walking from the manifest's directory up to the repository root.
/// Per-directory outcomes are cached so workspace members sharing a root
/// lockfile parse it once.
///
/// Boundary rules (PR #314 review):
///
/// - A manifest that itself declares `[workspace]` IS a workspace root: its
///   own directory's lockfile state (found / invalid / absent) is
///   authoritative — the walk never proceeds to an outer workspace.
/// - A **workspace member** — the first `[workspace]`-declaring ancestor's
///   `members` globs (or automatic path-dependency members) include its
///   directory and `exclude` does not — always resolves through the ROOT's
///   lockfile state. A stale or corrupt `Cargo.lock` beside the member's own
///   manifest is dead state Cargo never reads and is ignored entirely.
/// - Only a **standalone** package — no `[workspace]`-declaring ancestor, or
///   a nested crate the first such ancestor does not own (excluded /
///   non-member) — owns its own-directory lockfile.
/// - A plain package manifest (no `[workspace]`) or a stray lockfile with no
///   manifest beside it is walked past, mirroring Cargo's workspace
///   discovery; an unreadable/unparseable ancestor manifest stops the walk
///   without accepting the ancestor (membership cannot be verified; the
///   manifest's own lockfile state is all that is safe).
/// - Wherever the authoritative lockfile lives, one that exists but cannot
///   be read or parsed is [`LockfileStatus::Invalid`] — never a fallback to
///   a different lockfile.
fn nearest_lockfile(
    repo_root: &Path,
    manifest_repo_relative_path: &str,
    cache: &mut LockfileWalkCache,
) -> LockfileStatus {
    let mut manifest_dir: Vec<&str> = manifest_repo_relative_path.split('/').collect();
    // Drop the `Cargo.toml` file name, keeping the containing directory.
    manifest_dir.pop();
    match owning_workspace_dir(repo_root, manifest_repo_relative_path, cache) {
        // The owning workspace root's lockfile state is authoritative —
        // Cargo ignores a stale/corrupt local Cargo.lock beside a member
        // manifest (PR #314 review).
        Some(root_key) => {
            let segments: Vec<&str> = root_key.split('/').filter(|s| !s.is_empty()).collect();
            lockfile_in_dir(repo_root, &segments, cache)
        }
        // Standalone (no owning workspace, excluded / non-member, or
        // unverifiable ancestry): the manifest's own-directory lockfile
        // state is its own.
        None => lockfile_in_dir(repo_root, &manifest_dir, cache),
    }
}

/// Directory of the workspace root OWNING this manifest, as a `/`-joined
/// repo-relative key (`""` = the repository root): the manifest's own
/// directory when it declares `[workspace]`, else the first
/// `[workspace]`-declaring ancestor whose membership (members/exclude globs
/// plus automatic path-dependency members) includes it. `None` for
/// standalone, excluded / non-member, or unverifiable-ancestry manifests.
fn owning_workspace_dir(
    repo_root: &Path,
    manifest_repo_relative_path: &str,
    cache: &mut LockfileWalkCache,
) -> Option<String> {
    let mut segments: Vec<&str> = manifest_repo_relative_path.split('/').collect();
    segments.pop();
    let manifest_dir: Vec<&str> = segments.clone();

    // A manifest that itself declares `[workspace]` IS a workspace root.
    if matches!(
        workspace_facts_in_dir(repo_root, &manifest_dir, cache),
        WorkspaceFacts::Workspace { .. }
    ) {
        return Some(manifest_dir.join("/"));
    }
    // `package.workspace` explicit pointer (PR #314 review): the manifest
    // names its workspace root directly — Cargo resolves through it even
    // when the member lives OUTSIDE the root's directory tree (the root's
    // `members` can be `../`-relative). A pointer, when present, replaces
    // ancestor discovery entirely. Cargo requires MUTUAL consent: the
    // pointed root's own membership rules must cover the pointing package,
    // else Cargo errors ("believes it's in a workspace when it's not") —
    // a broken pointer (target missing or not a `[workspace]` root), an
    // out-of-repo pointer, or a target that does not include the member
    // means honest standalone behavior.
    if let Some(pointer) = package_workspace_pointer(repo_root, &manifest_dir, cache) {
        let target_key = resolve_pointer_dir(repo_root, &manifest_dir, &pointer)?;
        let target_segments: Vec<&str> = target_key.split('/').filter(|s| !s.is_empty()).collect();
        let WorkspaceFacts::Workspace {
            members,
            exclude,
            path_members,
            ..
        } = workspace_facts_in_dir(repo_root, &target_segments, cache)
        else {
            return None;
        };
        let rel = rel_between(&target_segments, &manifest_dir);
        return workspace_includes(&members, &exclude, &path_members, &rel).then_some(target_key);
    }
    loop {
        segments.pop()?;
        match workspace_facts_in_dir(repo_root, &segments, cache) {
            // The first `[workspace]`-declaring ancestor is the candidate
            // workspace root, whether or not a lockfile sits beside it.
            WorkspaceFacts::Workspace {
                members,
                exclude,
                path_members,
                ..
            } => {
                let rel = manifest_dir[segments.len()..].join("/");
                return workspace_includes(&members, &exclude, &path_members, &rel)
                    .then(|| segments.join("/"));
            }
            // Membership cannot be verified: never claim ownership. An
            // unloadable workspace grants no membership either — Cargo
            // rejects it whole — but its `[workspace]` boundary still stops
            // the walk (PR #314 review).
            WorkspaceFacts::Unverifiable | WorkspaceFacts::UnloadableWorkspace => return None,
            // Not a workspace root (plain package, or a stray lockfile with
            // no manifest): Cargo's discovery walks past it — and past any
            // lockfile it holds.
            WorkspaceFacts::NoManifest | WorkspaceFacts::PackageOnly => {}
        }
    }
}

/// Membership predicate shared by ancestor discovery and pointer targets:
/// `members` globs (including `../`-relative patterns for out-of-tree
/// pointer members) or automatic path-dependency members admit the
/// directory, and `exclude` globs veto it (PR #314 review).
fn workspace_includes(
    members: &[String],
    exclude: &[String],
    path_members: &[String],
    rel: &str,
) -> bool {
    (members.iter().any(|glob| member_glob_match(glob, rel))
        || path_members.iter().any(|member| member == rel))
        && !exclude.iter().any(|glob| member_glob_match(glob, rel))
}

/// Reads (and caches) one directory's `[package].workspace` pointer string
/// (PR #314 review). `None` when the manifest is missing, unparseable, or
/// carries no pointer.
fn package_workspace_pointer(
    repo_root: &Path,
    segments: &[&str],
    cache: &mut LockfileWalkCache,
) -> Option<String> {
    let dir_key = segments.join("/");
    if let Some(cached) = cache.pointers.get(&dir_key) {
        return cached.clone();
    }
    let mut candidate = repo_root.to_path_buf();
    for segment in segments {
        candidate.push(segment);
    }
    candidate.push("Cargo.toml");
    let pointer = std::fs::read_to_string(&candidate)
        .ok()
        .and_then(|text| text.parse::<toml_edit::DocumentMut>().ok())
        .and_then(|doc| {
            doc.get("package")
                .and_then(toml_edit::Item::as_table_like)
                .and_then(|package| package.get("workspace"))
                .and_then(|value| value.as_str())
                .map(str::to_owned)
        });
    cache.pointers.insert(dir_key, pointer.clone());
    pointer
}

/// Resolves a `package.workspace` pointer against the manifest's directory
/// with lexical `.`/`..` handling. An **absolute** pointer is normalized
/// like absolute path dependencies: the repo root is lexically absolutized
/// (relative scan roots covered) and stripped as a prefix. Returns the
/// target directory as a repo-root-relative `/`-joined key (`""` = the
/// repository root itself); `None` when the pointer escapes the
/// repository — such a root is outside this slice and the manifest stays
/// standalone (documented skip).
fn resolve_pointer_dir(repo_root: &Path, manifest_dir: &[&str], pointer: &str) -> Option<String> {
    let normalized = pointer.replace('\\', "/");
    if Path::new(&normalized).is_absolute() {
        let abs_root = absolutize_lexical(repo_root);
        let target = absolutize_lexical(Path::new(&normalized));
        let rel = target.strip_prefix(&abs_root).ok()?;
        let mut parts: Vec<&str> = Vec::new();
        for component in rel.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(part.to_str()?),
                std::path::Component::CurDir => {}
                _ => return None,
            }
        }
        return Some(parts.join("/"));
    }
    let mut stack: Vec<&str> = manifest_dir.to_vec();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    Some(stack.join("/"))
}

/// Lexical relative path from `root` to `dir` (`..` climbs when `dir` lives
/// outside `root`'s tree), used to run a pointer-named root's `exclude`
/// globs against the member's directory.
fn rel_between(root: &[&str], dir: &[&str]) -> String {
    let common = root
        .iter()
        .zip(dir.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut parts: Vec<&str> = vec![".."; root.len() - common];
    parts.extend(&dir[common..]);
    parts.join("/")
}

/// The `[workspace.dependencies]` inheritance surface of the workspace root
/// owning this manifest, when one exists (PR #314 review): what a member's
/// `{ workspace = true }` entries resolve against. `None` for standalone /
/// excluded / unverifiable manifests — their inherited entries are
/// unresolvable and reported as a coverage hole.
fn workspace_dep_specs_for(
    repo_root: &Path,
    manifest_repo_relative_path: &str,
    cache: &mut LockfileWalkCache,
) -> Option<BTreeMap<String, WorkspaceDepSpec>> {
    let root_key = owning_workspace_dir(repo_root, manifest_repo_relative_path, cache)?;
    let segments: Vec<&str> = root_key.split('/').filter(|s| !s.is_empty()).collect();
    match workspace_facts_in_dir(repo_root, &segments, cache) {
        WorkspaceFacts::Workspace { dep_specs, .. } => Some(dep_specs),
        _ => None,
    }
}

/// Reads (and caches) the lockfile state of one directory.
fn lockfile_in_dir(
    repo_root: &Path,
    segments: &[&str],
    cache: &mut LockfileWalkCache,
) -> LockfileStatus {
    let dir_key = segments.join("/");
    if let Some(cached) = cache.lockfiles.get(&dir_key) {
        return cached.clone();
    }
    let mut candidate = repo_root.to_path_buf();
    for segment in segments {
        candidate.push(segment);
    }
    candidate.push("Cargo.lock");
    let status = match std::fs::read_to_string(&candidate) {
        Ok(text) => {
            LockfileIndex::parse(&text).map_or(LockfileStatus::Invalid, LockfileStatus::Found)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LockfileStatus::Absent,
        // The file exists (or its state is unknowable) but cannot be
        // read: it must not be skipped in favor of an ancestor.
        Err(_) => LockfileStatus::Invalid,
    };
    cache.lockfiles.insert(dir_key, status.clone());
    status
}

/// Reads (and caches) what one directory's `Cargo.toml` says about workspace
/// roots.
fn workspace_facts_in_dir(
    repo_root: &Path,
    segments: &[&str],
    cache: &mut LockfileWalkCache,
) -> WorkspaceFacts {
    let dir_key = segments.join("/");
    if let Some(cached) = cache.workspaces.get(&dir_key) {
        return cached.clone();
    }
    let mut candidate = repo_root.to_path_buf();
    for segment in segments {
        candidate.push(segment);
    }
    candidate.push("Cargo.toml");
    let facts = match std::fs::read_to_string(&candidate) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorkspaceFacts::NoManifest,
        Err(_) => WorkspaceFacts::Unverifiable,
        Ok(text) => parse_workspace_facts(&text, repo_root, &dir_key),
    };
    cache.workspaces.insert(dir_key, facts.clone());
    facts
}

/// Classifies one manifest body as a workspace root, a plain package, or
/// unverifiable. For a workspace root that is also a package, the in-tree
/// `path = "…"` dependency closure is collected as automatic members
/// (Cargo semantics; PR #314 review). `dir_key` is the root's repo-relative
/// `/`-joined directory (`""` = the repository root itself), needed so
/// `../`-relative member patterns can resolve without escaping the repo.
fn parse_workspace_facts(text: &str, repo_root: &Path, dir_key: &str) -> WorkspaceFacts {
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return WorkspaceFacts::Unverifiable;
    };
    let Some(workspace) = doc
        .get("workspace")
        .and_then(toml_edit::Item::as_table_like)
    else {
        return WorkspaceFacts::PackageOnly;
    };
    let root_segments: Vec<&str> = dir_key.split('/').filter(|s| !s.is_empty()).collect();
    // Absolute `members`/`exclude` patterns are normalized to the
    // workspace-root-relative form every matcher and seed expects — the
    // same absolutize-and-strip idiom as absolute path deps and pointers;
    // out-of-repo absolute patterns are a documented skip (PR #314 review).
    let normalize = |patterns: Vec<String>| -> Vec<String> {
        patterns
            .into_iter()
            .filter_map(|pattern| normalize_member_pattern(repo_root, &root_segments, pattern))
            .collect()
    };
    let members = normalize(string_array(workspace.get("members")));
    let exclude = normalize(string_array(workspace.get("exclude")));
    // Member resolution hitting a non-package directory, a missing literal
    // member, or a glob matching nothing makes Cargo reject the WHOLE
    // workspace (verified — "failed to load manifest for workspace
    // member"): no member may use its lockfile or inheritance surface
    // (PR #314 review).
    if !members_are_loadable(repo_root, &root_segments, &members, &exclude) {
        return WorkspaceFacts::UnloadableWorkspace;
    }
    let path_members = path_dependency_closure(&doc, repo_root, dir_key, &members, &exclude);
    // `[workspace.dependencies]` inheritance surface: a plain string entry
    // is its own version requirement; a table entry contributes its
    // `package` rename and `version` keys (PR #314 review).
    let dep_specs = workspace
        .get("dependencies")
        .and_then(toml_edit::Item::as_table_like)
        .map(|table| {
            table
                .iter()
                .map(|(key, item)| {
                    // A table entry needs a usable source — a string
                    // `version`/`path`/`git` — to be inheritable; Cargo
                    // rejects `serde = {}` and wrong-typed fields like
                    // `version = 1`, so an invalid entry stays recorded as
                    // unusable and inheriting members take the
                    // uninterpretable diagnostic path (PR #314 review).
                    let spec = item.as_str().map_or_else(
                        || {
                            item.as_table_like()
                                .map_or_else(WorkspaceDepSpec::default, |entry| {
                                    let field = |name: &str| {
                                        entry.get(name).and_then(|v| v.as_str()).map(str::to_owned)
                                    };
                                    let version = field("version");
                                    // Template known keys must be
                                    // well-typed with the template-context
                                    // rules — `optional = true` rejected
                                    // (`false` accepted), the `workspace`
                                    // key ignored by Cargo entirely — and
                                    // a declared version must be a
                                    // requirement Cargo can parse
                                    // (PR #314 review, each verified).
                                    let usable = dependency_table_is_well_typed(
                                        entry,
                                        DependencyTableContext::Template,
                                    ) && version
                                        .as_deref()
                                        .is_none_or(requirement_is_parseable)
                                        && (version.is_some()
                                            || field("path").is_some()
                                            || field("git").is_some());
                                    WorkspaceDepSpec {
                                        package: field("package"),
                                        version,
                                        usable,
                                    }
                                })
                        },
                        |version| WorkspaceDepSpec {
                            package: None,
                            version: Some(version.to_owned()),
                            // A plain-string template is its own version
                            // requirement — usable only when Cargo could
                            // parse it (PR #314 review).
                            usable: requirement_is_parseable(version),
                        },
                    );
                    (key.to_owned(), spec)
                })
                .collect()
        })
        .unwrap_or_default();
    WorkspaceFacts::Workspace {
        members,
        exclude,
        path_members,
        dep_specs,
    }
}

/// Collects the transitive in-tree `path = "…"` dependency directories of a
/// workspace, relative to the root. Cargo treats these as automatic
/// workspace members even when `members` does not list them; the closure
/// grows from the root package (when the root manifest is one) AND from
/// every explicit members-glob member — so a virtual root's members
/// contribute their path dependencies too (PR #314 review). Directories
/// matching an `exclude` glob are pruned at traversal time: they never join
/// and never contribute their own path dependencies, though a dependency
/// independently reachable from a non-excluded seed still joins. Paths
/// escaping the root directory (`..` or an absolute path beyond it) are
/// outside this slice and are skipped; each manifest is parsed with
/// `toml_edit` — never `cargo metadata`.
fn path_dependency_closure(
    root_doc: &toml_edit::DocumentMut,
    repo_root: &Path,
    dir_key: &str,
    members: &[String],
    exclude: &[String],
) -> Vec<String> {
    let root_segments: Vec<&str> = dir_key.split('/').filter(|s| !s.is_empty()).collect();
    let mut root_dir = repo_root.to_path_buf();
    for segment in &root_segments {
        root_dir.push(segment);
    }
    let mut closure: BTreeSet<String> = BTreeSet::new();
    // The scan root may be relative (`eg scan .`); the absolute-path branch
    // of `normalize_path_dep` strips against the lexically absolutized REPO
    // root — an absolute in-repo target may live anywhere in the
    // repository, not just inside the workspace directory (PR #314 review).
    let abs_repo = absolutize_lexical(repo_root);
    // `[workspace.dependencies]` path templates: a member (or the root
    // package) inheriting one via `{ workspace = true }` makes its target
    // an automatic member; the paths are relative to the ROOT (PR #314
    // review). Uninherited templates never seed anything.
    let workspace_paths = workspace_dependency_paths(root_doc);
    let enqueue = |deps: &ManifestPathDeps, base: &str, queue: &mut Vec<String>| {
        for path in &deps.literal {
            if let Some(next) = normalize_path_dep(&abs_repo, &root_segments, base, path) {
                queue.push(next);
            }
        }
        for key in &deps.inherited {
            if let Some(path) = workspace_paths.get(key)
                && let Some(next) = normalize_path_dep(&abs_repo, &root_segments, "", path)
            {
                queue.push(next);
            }
        }
    };
    let mut queue: Vec<String> = Vec::new();
    if root_doc.get("package").is_some() {
        enqueue(&manifest_path_dependency_dirs(root_doc), "", &mut queue);
    }
    // Explicit member seeds (PR #314 review): each pattern is resolved
    // against the root's directory, so `../`-relative members OUTSIDE the
    // root's tree seed the closure too. The pattern's leading literal
    // segments anchor the enumeration directory; `..` resolves against the
    // root's repo-relative position and a pattern escaping the repository
    // is skipped (documented). Candidates keep the pattern's own
    // root-relative form so exclusion checks and membership comparisons
    // stay consistent.
    for pattern in members {
        seed_member_pattern(repo_root, &root_segments, pattern, exclude, &mut queue);
    }
    while let Some(rel) = queue.pop() {
        // `exclude` removes the package AND everything reachable only
        // through it: an excluded directory never joins the closure and
        // never contributes its own path dependencies (PR #314 review).
        // A dependency that is independently reachable from a non-excluded
        // seed still joins through that other path.
        if exclude.iter().any(|glob| member_glob_match(glob, &rel)) {
            continue;
        }
        if !closure.insert(rel.clone()) {
            continue;
        }
        let mut manifest = root_dir.clone();
        for segment in rel.split('/') {
            manifest.push(segment);
        }
        manifest.push("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
            continue;
        };
        enqueue(&manifest_path_dependency_dirs(&doc), &rel, &mut queue);
    }
    closure.into_iter().collect()
}

/// Seeds the closure queue from one `members` pattern (PR #314 review):
/// the pattern's leading literal segments anchor the enumeration
/// directory, `..` resolving against the root's repo-relative position —
/// so `../`-relative members outside the root's tree seed too, while a
/// pattern escaping the repository is skipped (documented).
fn seed_member_pattern(
    repo_root: &Path,
    root_segments: &[&str],
    pattern: &str,
    exclude: &[String],
    queue: &mut Vec<String>,
) {
    let segments: Vec<&str> = pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let split = segments
        .iter()
        .position(|segment| is_glob_segment(segment))
        .unwrap_or(segments.len());
    let (prefix, rest) = segments.split_at(split);
    let mut anchor_repo: Vec<&str> = root_segments.to_vec();
    for segment in prefix {
        if *segment == ".." {
            if anchor_repo.pop().is_none() {
                // The pattern escapes the repository: documented skip.
                return;
            }
        } else {
            anchor_repo.push(segment);
        }
    }
    let prefix_rel = prefix.join("/");
    let mut anchor_path = repo_root.to_path_buf();
    for segment in &anchor_repo {
        anchor_path.push(segment);
    }
    if rest.is_empty() {
        if !prefix_rel.is_empty() && anchor_path.join("Cargo.toml").is_file() {
            push_candidate(&prefix_rel, pattern, exclude, queue);
        }
        return;
    }
    for sub in manifest_dirs_under(&anchor_path) {
        let candidate = if prefix_rel.is_empty() {
            sub
        } else {
            format!("{prefix_rel}/{sub}")
        };
        push_candidate(&candidate, pattern, exclude, queue);
    }
}

/// Queues one concrete member candidate after the full pattern match and
/// the exclusion veto — both run on the candidate's root-relative form.
fn push_candidate(candidate: &str, pattern: &str, exclude: &[String], queue: &mut Vec<String>) {
    if member_glob_match(pattern, candidate)
        && !exclude
            .iter()
            .any(|glob| member_glob_match(glob, candidate))
    {
        queue.push(candidate.to_owned());
    }
}

/// Enumerates every directory under `root_dir` (relative, `/`-separated,
/// sorted) that holds a `Cargo.toml`, skipping `.git` and `target`. Used to
/// expand members globs into concrete seed manifests for the automatic
/// path-dependency closure.
fn manifest_dirs_under(root_dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name == ".git" || name == "target" {
                continue;
            }
            let child_rel = if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{rel}/{name}")
            };
            let child = entry.path();
            if child.join("Cargo.toml").is_file() {
                out.push(child_rel.clone());
            }
            walk(&child, &child_rel, out);
        }
    }
    let mut dirs = Vec::new();
    walk(root_dir, "", &mut dirs);
    dirs.sort();
    dirs
}

/// Lexically absolutizes a workspace-root path against the process working
/// directory: a relative root (`eg scan .`) joins the cwd, then `.`/`..`
/// components resolve in place. Symlinks are deliberately not followed —
/// `fs::canonicalize` would rewrite a symlinked checkout into a physical
/// path the manifest author never wrote — keeping the absolute-path-dep
/// prefix comparison deterministic (PR #314 review).
fn absolutize_lexical(dir: &Path) -> PathBuf {
    let joined = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        // Without a resolvable working directory the relative root cannot
        // be absolutized; keeping it as-is preserves prior behavior
        // (absolute path deps are skipped, never invented).
        std::env::current_dir().map_or_else(|_| dir.to_path_buf(), |cwd| cwd.join(dir))
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if matches!(
                    out.components().next_back(),
                    Some(std::path::Component::Normal(_))
                ) {
                    out.pop();
                } else if !out.has_root() {
                    // A leading `..` on a still-relative path is preserved;
                    // `..` at an absolute root resolves to the root itself.
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Normalizes one `path = "…"` dependency value to the workspace-root-
/// relative member form.
///
/// Relative paths join `base` (the declaring manifest's root-relative
/// directory) with `.`/`..` resolution in the repo-relative canonical
/// space; an **absolute** path is accepted when it points anywhere inside
/// the REPOSITORY (lexical prefix match against `abs_repo`, the
/// [`absolutize_lexical`] form of the repo root, so a relative scan root
/// still recognizes in-repo targets — including targets outside the
/// workspace directory) and is re-expressed root-relative via
/// [`rel_between`]. Absolute out-of-repo targets and relative paths
/// escaping the repository are skipped — documented out of scope, never an
/// error; a path climbing back over the repo tree stays a member
/// (PR #314 review).
fn normalize_path_dep(
    abs_repo: &Path,
    root_segments: &[&str],
    base: &str,
    raw: &str,
) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    if Path::new(&normalized).is_absolute() {
        let rel = absolutize_lexical(Path::new(&normalized));
        let rel = rel.strip_prefix(abs_repo).ok()?;
        let mut parts: Vec<&str> = Vec::new();
        for component in rel.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(part.to_str()?),
                std::path::Component::CurDir => {}
                _ => return None,
            }
        }
        if parts == root_segments {
            // The workspace root is never its own member.
            return None;
        }
        return Some(rel_between(root_segments, &parts));
    }
    normalize_in_tree_path(root_segments, base, &normalized)
}

/// Path-dependency references collected from one manifest for the
/// workspace-membership closure.
#[derive(Debug, Default, Eq, PartialEq)]
struct ManifestPathDeps {
    /// Literal `path = "…"` values, relative to the declaring manifest.
    literal: Vec<String>,
    /// Table keys declared `{ workspace = true }`: resolved against the
    /// workspace root's `[workspace.dependencies]` table, whose `path`
    /// values are relative to the ROOT manifest's directory (PR #314
    /// review — Cargo makes inherited path deps automatic members too).
    inherited: Vec<String>,
}

/// Extracts the path-dependency references from a manifest's dependency
/// tables for the workspace-membership closure: the three plain tables
/// **and** every `[target.<cfg>.dependencies]` / `dev-` / `build-` variant —
/// Cargo treats target-specific and workspace-inherited path dependencies as
/// automatic workspace members too (PR #314 review). This feeds membership
/// only; target-specific dependency ROWS stay out of extraction scope as
/// documented. A Cargo-invalid entry (ill-typed known keys or a cross-field
/// source conflict like `{ path = "…", git = "…" }`) never seeds membership:
/// Cargo rejects the manifest, so trusting its `path` would fabricate a
/// member (PR #314 review). Whether an invalid ROOT manifest should
/// invalidate the entire workspace it declares is a broader open question —
/// this slice only refuses to seed from the invalid entries themselves.
fn manifest_path_dependency_dirs(doc: &toml_edit::DocumentMut) -> ManifestPathDeps {
    fn collect_paths(
        table: &dyn toml_edit::TableLike,
        kind: DependencyKind,
        deps: &mut ManifestPathDeps,
    ) {
        for (key, item) in table.iter() {
            let Some(spec) = item.as_table_like() else {
                continue;
            };
            if !dependency_table_is_well_typed(spec, DependencyTableContext::member(kind)) {
                continue;
            }
            if let Some(path) = spec.get("path").and_then(|value| value.as_str()) {
                deps.literal.push(path.to_owned());
            } else if spec
                .get("workspace")
                .and_then(toml_edit::Item::as_bool)
                .unwrap_or(false)
            {
                deps.inherited.push(key.to_owned());
            }
        }
    }
    let mut deps = ManifestPathDeps::default();
    for kind in DEPENDENCY_KINDS {
        if let Some(table) = doc
            .get(kind.table())
            .and_then(toml_edit::Item::as_table_like)
        {
            collect_paths(table, kind, &mut deps);
        }
    }
    if let Some(targets) = doc.get("target").and_then(toml_edit::Item::as_table_like) {
        for (_, target) in targets.iter() {
            let Some(target) = target.as_table_like() else {
                continue;
            };
            for kind in DEPENDENCY_KINDS {
                if let Some(table) = target
                    .get(kind.table())
                    .and_then(toml_edit::Item::as_table_like)
                {
                    collect_paths(table, kind, &mut deps);
                }
            }
        }
    }
    deps
}

/// Maps `[workspace.dependencies]` keys to their declared `path` values
/// (relative to the workspace root). Entries without a `path` never feed
/// membership, and a path entry no member (nor the root package) ever
/// inherits is only a template — never enqueued from here; inheritance
/// drives membership (PR #314 review, Cargo semantics). A Cargo-invalid
/// template (ill-typed known keys, a cross-field source conflict, or
/// `optional = true` — workspace dependencies cannot be optional) never
/// feeds membership either; the `workspace` key in a template is ignored
/// by Cargo entirely and never disqualifies one (PR #314 review, both
/// verified).
fn workspace_dependency_paths(root_doc: &toml_edit::DocumentMut) -> BTreeMap<String, String> {
    root_doc
        .get("workspace")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml_edit::Item::as_table_like)
        .map(|table| {
            table
                .iter()
                .filter_map(|(key, item)| {
                    let spec = item.as_table_like()?;
                    if !dependency_table_is_well_typed(spec, DependencyTableContext::Template) {
                        return None;
                    }
                    spec.get("path")
                        .and_then(|value| value.as_str())
                        .map(|path| (key.to_owned(), path.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Joins a `/`-separated base (relative to the workspace root; `""` for the
/// root itself) with a manifest-declared relative path by resolving both
/// against the root's REPO-relative position (PR #314 review): an
/// out-of-root member's dependency may legitimately climb back over the
/// repository tree — base `../pkgs/app` + `../../other` is the repo-root
/// sibling `other` — or back inside the root's own tree, and both are
/// members per Cargo. The result is re-expressed root-relative via
/// `rel_between`, so it compares canonically against membership `rel`
/// strings. Returns `None` only when the path escapes the REPOSITORY
/// (documented out of scope) or lands on the root's own directory.
fn normalize_in_tree_path(root_segments: &[&str], base: &str, path: &str) -> Option<String> {
    // Repo-relative position of the declaring manifest's directory: the
    // root's position plus the root-relative base (whose leading `..`
    // segments climb over the root, never past the repository).
    let mut stack: Vec<&str> = root_segments.to_vec();
    let normalized = format!("{base}/{}", path.replace('\\', "/"));
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                // Escaping the repository is a documented skip.
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    if stack == root_segments {
        // The root is never its own member.
        return None;
    }
    Some(rel_between(root_segments, &stack))
}

/// Returns whether one `/`-separated pattern segment carries glob syntax.
fn is_glob_segment(segment: &str) -> bool {
    segment.contains(['*', '?', '['])
}

/// Enumerates every directory under `root_dir` (relative, `/`-separated,
/// sorted), hidden directories included and nothing skipped: Cargo's member
/// globs match hidden directories too (verified against `cargo metadata`),
/// and a matched directory without a manifest is fatal wherever it sits.
fn all_dirs_under(root_dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let child_rel = if rel.is_empty() {
                name.to_owned()
            } else {
                format!("{rel}/{name}")
            };
            let child = dir.join(name);
            out.push(child_rel.clone());
            walk(&child, &child_rel, out);
        }
    }
    let mut out = Vec::new();
    walk(root_dir, "", &mut out);
    out
}

/// Verifies Cargo would load every explicit member (PR #314 review, each
/// rule verified against `cargo metadata`): a literal member must be a
/// directory holding `Cargo.toml`; a glob must match at least one directory
/// (an empty match set falls back to a literal path Cargo then fails to
/// load), and every matched, non-excluded directory must hold `Cargo.toml`.
/// Hidden directories count, loose files never match, and excluded matches
/// are exempt. Out-of-repo patterns were already dropped by normalization
/// (documented out of scope).
fn members_are_loadable(
    repo_root: &Path,
    root_segments: &[&str],
    members: &[String],
    exclude: &[String],
) -> bool {
    let excluded = |candidate: &str| {
        exclude
            .iter()
            .any(|glob| member_glob_match(glob, candidate))
    };
    for pattern in members {
        let segments: Vec<&str> = pattern
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let split = segments
            .iter()
            .position(|segment| is_glob_segment(segment))
            .unwrap_or(segments.len());
        let (prefix, rest) = segments.split_at(split);
        let mut anchor_repo: Vec<&str> = root_segments.to_vec();
        let mut escaped = false;
        for segment in prefix {
            if *segment == ".." {
                if anchor_repo.pop().is_none() {
                    escaped = true;
                    break;
                }
            } else {
                anchor_repo.push(segment);
            }
        }
        if escaped {
            // Out-of-repo: documented out of scope, never checked here.
            continue;
        }
        let mut anchor_path = repo_root.to_path_buf();
        for segment in &anchor_repo {
            anchor_path.push(segment);
        }
        let prefix_rel = prefix.join("/");
        if rest.is_empty() {
            if excluded(&prefix_rel) {
                continue;
            }
            if !anchor_path.join("Cargo.toml").is_file() {
                return false;
            }
            continue;
        }
        let mut matched_any = false;
        for sub in all_dirs_under(&anchor_path) {
            let candidate = if prefix_rel.is_empty() {
                sub.clone()
            } else {
                format!("{prefix_rel}/{sub}")
            };
            if !member_glob_match(pattern, &candidate) {
                continue;
            }
            matched_any = true;
            if excluded(&candidate) {
                continue;
            }
            let mut member_dir = anchor_path.clone();
            for segment in sub.split('/') {
                member_dir.push(segment);
            }
            if !member_dir.join("Cargo.toml").is_file() {
                return false;
            }
        }
        if !matched_any {
            return false;
        }
    }
    true
}

/// Normalizes one `members`/`exclude` pattern to the workspace-root-relative
/// form used for matching and seeding (PR #314 review): a relative pattern
/// passes through unchanged unless its leading literal prefix carries `..`
/// segments (`crates/../app`), which resolve lexically against the root's
/// repo-relative position — the same canonical space as absolute patterns —
/// with glob segments and everything after them carried literally; an
/// absolute in-repo pattern is normalized like absolute path dependencies
/// and pointers — the repo root lexically absolutized and stripped as a
/// prefix (relative scan roots covered), then re-expressed relative to the
/// workspace root via [`rel_between`]. An out-of-repo absolute pattern, or
/// a relative prefix escaping the repository, is `None` — a documented
/// skip, never a match.
fn normalize_member_pattern(
    repo_root: &Path,
    root_segments: &[&str],
    pattern: String,
) -> Option<String> {
    let normalized = pattern.replace('\\', "/");
    if !Path::new(&normalized).is_absolute() {
        let segments: Vec<&str> = normalized
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
            .collect();
        let split = segments
            .iter()
            .position(|segment| is_glob_segment(segment))
            .unwrap_or(segments.len());
        let (prefix, rest) = segments.split_at(split);
        if !prefix.contains(&"..") {
            // Nothing to resolve: the matcher already normalizes plain
            // `.` segments on both sides.
            return Some(pattern);
        }
        let mut resolved: Vec<&str> = root_segments.to_vec();
        for segment in prefix {
            if *segment == ".." {
                // Escaping the repository is a documented skip.
                resolved.pop()?;
            } else {
                resolved.push(segment);
            }
        }
        let prefix_rel = rel_between(root_segments, &resolved);
        let rest_rel = rest.join("/");
        return match (prefix_rel.is_empty(), rest_rel.is_empty()) {
            // The root itself is never a member pattern.
            (true, true) => None,
            (true, false) => Some(rest_rel),
            (false, true) => Some(prefix_rel),
            (false, false) => Some(format!("{prefix_rel}/{rest_rel}")),
        };
    }
    let abs_repo = absolutize_lexical(repo_root);
    let abs_pattern = absolutize_lexical(Path::new(&normalized));
    let rel = abs_pattern.strip_prefix(&abs_repo).ok()?;
    let mut segments: Vec<&str> = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => segments.push(part.to_str()?),
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(rel_between(root_segments, &segments))
}

/// Extracts a TOML string array (`members` / `exclude`) as owned strings with
/// trailing slashes trimmed; non-arrays and non-strings yield nothing.
fn string_array(item: Option<&toml_edit::Item>) -> Vec<String> {
    item.and_then(toml_edit::Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str())
                .map(|value| value.trim_end_matches('/').to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// Minimal deterministic glob match for Cargo workspace `members` / `exclude`
/// entries over a `/`-separated relative directory path: `*` matches any
/// sequence within one path segment, `?` one non-separator character, a
/// `**` segment spans zero or more whole segments, and `[…]` character
/// classes match one character — singles, `a-z` ranges, `[!…]` negation, and
/// a literal `]` as the first member, per the `glob` crate Cargo uses
/// (PR #314 review). An unclosed or empty class (where the `glob` crate
/// errors and Cargo would reject the manifest) degrades to a literal `[` —
/// it never silently matches everything or nothing. No external glob crate.
fn member_glob_match(pattern: &str, path: &str) -> bool {
    /// One parsed `[…]` character class.
    struct CharClass {
        negated: bool,
        singles: Vec<char>,
        ranges: Vec<(char, char)>,
    }
    impl CharClass {
        /// Parses the class body following `[`, returning the class and the
        /// rest of the pattern after `]`; `None` when the class never closes
        /// (including the empty `[]`, whose first `]` is a literal member).
        fn parse(rest: &[char]) -> Option<(Self, &[char])> {
            let mut i = usize::from(rest.first() == Some(&'!'));
            let negated = i == 1;
            let mut singles = Vec::new();
            let mut ranges = Vec::new();
            let mut first = true;
            loop {
                let ch = *rest.get(i)?;
                if ch == ']' && !first {
                    let class = Self {
                        negated,
                        singles,
                        ranges,
                    };
                    return Some((class, &rest[i + 1..]));
                }
                first = false;
                // `a-z` is a range unless the `-` sits at a class edge
                // (then both characters are literal members).
                if rest.get(i + 1) == Some(&'-') && rest.get(i + 2).is_some_and(|c| *c != ']') {
                    ranges.push((ch, *rest.get(i + 2)?));
                    i += 3;
                } else {
                    singles.push(ch);
                    i += 1;
                }
            }
        }

        fn matches(&self, ch: char) -> bool {
            let inside = self.singles.contains(&ch)
                || self.ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&ch));
            inside != self.negated
        }
    }
    fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
        match pattern.split_first() {
            None => path.is_empty(),
            Some((&"**", rest)) => (0..=path.len()).any(|skip| segments_match(rest, &path[skip..])),
            Some((first, rest)) => match path.split_first() {
                Some((segment, path_rest)) => {
                    segment_match(first, segment) && segments_match(rest, path_rest)
                }
                None => false,
            },
        }
    }
    fn segment_match(pattern: &str, segment: &str) -> bool {
        fn inner(p: &[char], s: &[char]) -> bool {
            match p.split_first() {
                None => s.is_empty(),
                Some(('*', rest)) => (0..=s.len()).any(|skip| inner(rest, &s[skip..])),
                Some(('?', rest)) => s
                    .split_first()
                    .is_some_and(|(_, s_rest)| inner(rest, s_rest)),
                Some(('[', rest)) => match CharClass::parse(rest) {
                    Some((class, after)) => s
                        .split_first()
                        .is_some_and(|(sc, s_rest)| class.matches(*sc) && inner(after, s_rest)),
                    // Unclosed class: the `[` is a literal (documented).
                    None => s
                        .split_first()
                        .is_some_and(|(sc, s_rest)| *sc == '[' && inner(rest, s_rest)),
                },
                Some((ch, rest)) => s
                    .split_first()
                    .is_some_and(|(sc, s_rest)| sc == ch && inner(rest, s_rest)),
            }
        }
        let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), segment.chars().collect());
        inner(&p, &s)
    }
    // Cargo accepts `./`-prefixed members/exclude entries; `.` segments are
    // dropped on both sides so `./crates/a` compares equal to the
    // repo-relative directory `crates/a` (PR #314 review).
    let pattern_segments: Vec<&str> = pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let path_segments: Vec<&str> = path
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    segments_match(&pattern_segments, &path_segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Crate attribution (issue #117) needs the manifest's SHAPE, not just its
    /// package name: `package_name` is `None` for a virtual workspace root, for
    /// a `[package]` whose name is unusable, and for a manifest Cargo refuses to
    /// load — three cases the attribution walk must answer differently.
    #[test]
    fn manifest_dependencies_reports_the_manifest_shape() {
        let named = parse_manifest_dependencies("[package]\nname = \"x\"\n").expect("parses");
        assert_eq!(named.shape, ManifestShape::Package);
        assert_eq!(named.package_name.as_deref(), Some("x"));

        let unusable_name =
            parse_manifest_dependencies("[package]\nname = \"bad name\"\n").expect("parses");
        assert_eq!(
            unusable_name.shape,
            ManifestShape::Package,
            "a `[package]` table with an unusable name still declares a package"
        );
        assert_eq!(unusable_name.package_name, None);

        let virtual_root =
            parse_manifest_dependencies("[workspace]\nmembers = []\n").expect("parses");
        assert_eq!(virtual_root.shape, ManifestShape::VirtualRoot);
        assert_eq!(virtual_root.package_name, None);

        // `[profile]` is accepted beside `[workspace]`.
        let with_profile = parse_manifest_dependencies(
            "[workspace]\nmembers = []\n\n[profile.release]\nopt-level = 3\n",
        )
        .expect("parses");
        assert_eq!(with_profile.shape, ManifestShape::VirtualRoot);

        // Neither table: Cargo refuses to load it.
        let neither =
            parse_manifest_dependencies("[dependencies]\nserde = \"1\"\n").expect("parses");
        assert_eq!(neither.shape, ManifestShape::Unusable);

        // `[workspace]` plus a package-only section: also refused. Every entry
        // verified against real `cargo metadata`.
        for section in [
            "[dependencies]\nserde = \"1\"\n",
            "[dev-dependencies]\nserde = \"1\"\n",
            "[build-dependencies]\nserde = \"1\"\n",
            "[features]\ndefault = []\n",
            "[lib]\nname = \"x\"\npath = \"src/lib.rs\"\n",
            "[badges]\nmaintenance = { status = \"active\" }\n",
            "[lints.rust]\nunsafe_code = \"forbid\"\n",
        ] {
            let manifest = format!("[workspace]\nmembers = []\n\n{section}");
            let parsed = parse_manifest_dependencies(&manifest).expect("parses");
            assert_eq!(
                parsed.shape,
                ManifestShape::Unusable,
                "a virtual manifest carrying `{section}` is rejected by Cargo"
            );
        }
    }

    #[test]
    fn string_and_inline_table_requirements_are_captured_as_written() {
        let parsed = parse_manifest_dependencies(
            r#"[package]
name = "pkg"

[dependencies]
serde = "1.0.228"
clap = { version = "4.6.1", features = ["derive"] }
"#,
        )
        .expect("manifest parses");
        assert_eq!(parsed.package_name.as_deref(), Some("pkg"));
        assert_eq!(
            parsed.declarations,
            vec![
                DeclaredDependency {
                    name: "clap".to_owned(),
                    declared_as: None,
                    kind: DependencyKind::Normal,
                    declared_requirement: Some("4.6.1".to_owned()),
                    inherits_workspace: false,
                },
                DeclaredDependency {
                    name: "serde".to_owned(),
                    declared_as: None,
                    kind: DependencyKind::Normal,
                    declared_requirement: Some("1.0.228".to_owned()),
                    inherits_workspace: false,
                },
            ]
        );
    }

    #[test]
    fn package_rename_records_the_real_crate_name() {
        let parsed = parse_manifest_dependencies(
            r#"[package]
name = "pkg"

[dependencies]
alias = { package = "real-crate", version = "2" }
"#,
        )
        .expect("manifest parses");
        assert_eq!(parsed.declarations[0].name, "real-crate");
        assert_eq!(
            parsed.declarations[0].declared_requirement.as_deref(),
            Some("2")
        );
    }

    #[test]
    fn rename_pair_preserves_both_declarations() {
        // Cargo's `package` rename syntax legitimately declares two versions
        // of the same crate (PR #314 review): both entries must survive.
        let parsed = parse_manifest_dependencies(
            r#"[package]
name = "pkg"

[dependencies]
embedded-hal = "0.2"
embedded-hal-1 = { package = "embedded-hal", version = "1" }
"#,
        )
        .expect("manifest parses");
        assert_eq!(
            parsed.declarations.len(),
            2,
            "one fact per declared entry — a rename never collapses a declaration"
        );
        let plain = &parsed.declarations[0];
        assert_eq!(plain.name, "embedded-hal");
        assert_eq!(plain.declared_as, None);
        assert_eq!(plain.declared_requirement.as_deref(), Some("0.2"));
        let renamed = &parsed.declarations[1];
        assert_eq!(renamed.name, "embedded-hal");
        assert_eq!(renamed.declared_as.as_deref(), Some("embedded-hal-1"));
        assert_eq!(renamed.declared_requirement.as_deref(), Some("1"));
    }

    #[test]
    fn rename_pair_resolves_each_entry_against_its_own_requirement() {
        let index = LockfileIndex::parse(
            r#"version = 4

[[package]]
name = "embedded-hal"
version = "0.2.7"

[[package]]
name = "embedded-hal"
version = "1.0.0"
"#,
        )
        .expect("lockfile parses");
        assert_eq!(
            index.resolve("embedded-hal", Some("0.2")),
            LockResolution::Locked("0.2.7".to_owned()),
            "the requirement selects among multiple locked versions"
        );
        assert_eq!(
            index.resolve("embedded-hal", Some("1")),
            LockResolution::Locked("1.0.0".to_owned())
        );
        // No requirement, an unparseable requirement, or a requirement that
        // matches zero or several locked versions never picks one.
        assert_eq!(
            index.resolve("embedded-hal", None),
            LockResolution::AmbiguousInLockfile
        );
        assert_eq!(
            index.resolve("embedded-hal", Some("not a requirement")),
            LockResolution::AmbiguousInLockfile
        );
        assert_eq!(
            index.resolve("embedded-hal", Some(">=0.2")),
            LockResolution::AmbiguousInLockfile,
            "a requirement matching several locked versions stays ambiguous"
        );
        assert_eq!(
            index.resolve("embedded-hal", Some("2")),
            LockResolution::RequirementUnsatisfiedInLockfile,
            "a requirement matching no locked version never guesses"
        );
    }

    #[test]
    fn stale_sole_locked_version_failing_the_requirement_is_unsatisfied() {
        // PR #314 review: a stale/shared lockfile can hold exactly one version
        // of a crate that does not satisfy the declaration being scanned; that
        // version must never be presented as `locked`.
        let index = LockfileIndex::parse(
            r#"version = 4

[[package]]
name = "foo"
version = "1.0.0"
"#,
        )
        .expect("lockfile parses");
        assert_eq!(
            index.resolve("foo", Some("2")),
            LockResolution::RequirementUnsatisfiedInLockfile,
            "the sole locked version fails the declared requirement"
        );
        assert_eq!(
            LockResolution::RequirementUnsatisfiedInLockfile.version(),
            None,
            "an unsatisfied requirement never yields a resolved version"
        );
        // A satisfying requirement, no requirement, or an unparseable
        // requirement still locks the sole version.
        assert_eq!(
            index.resolve("foo", Some("1")),
            LockResolution::Locked("1.0.0".to_owned())
        );
        assert_eq!(
            index.resolve("foo", None),
            LockResolution::Locked("1.0.0".to_owned())
        );
        assert_eq!(
            index.resolve("foo", Some("not a requirement")),
            LockResolution::Locked("1.0.0".to_owned()),
            "an unparseable requirement cannot gate; the sole version locks"
        );
    }

    #[test]
    fn rename_pair_records_carry_distinct_stable_ids() {
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            r#"[package]
name = "pkg"

[dependencies]
embedded-hal = "0.2"
embedded-hal-1 = { package = "embedded-hal", version = "1" }
"#,
            &LockfileStatus::Absent,
            None,
        );
        assert_eq!(records.len(), 2, "both declared entries become facts");
        assert_ne!(
            records[0].id(),
            records[1].id(),
            "the manifest key participates in record identity"
        );
        let aliases: Vec<Option<&str>> = records
            .iter()
            .map(|r| r.dependency().expect("payload").declared_as.as_deref())
            .collect();
        assert_eq!(aliases, vec![None, Some("embedded-hal-1")]);
    }

    #[test]
    fn versionless_declarations_carry_no_requirement() {
        let parsed = parse_manifest_dependencies(
            r#"[package]
name = "pkg"

[dependencies]
local = { path = "../local" }
shared = { workspace = true }
"#,
        )
        .expect("manifest parses");
        for declaration in &parsed.declarations {
            assert_eq!(declaration.declared_requirement, None);
        }
    }

    #[test]
    fn dotted_table_headers_are_captured() {
        let parsed = parse_manifest_dependencies(
            r#"[package]
name = "pkg"

[dependencies.serde]
version = "1.0.228"
features = ["derive"]
"#,
        )
        .expect("manifest parses");
        assert_eq!(parsed.declarations.len(), 1);
        assert_eq!(parsed.declarations[0].name, "serde");
        assert_eq!(
            parsed.declarations[0].declared_requirement.as_deref(),
            Some("1.0.228")
        );
    }

    #[test]
    fn target_specific_and_workspace_tables_are_out_of_scope() {
        let parsed = parse_manifest_dependencies(
            r#"[package]
name = "pkg"

[target.'cfg(unix)'.dependencies]
libc = "0.2"

[workspace.dependencies]
shared = "1"
"#,
        )
        .expect("manifest parses");
        assert!(parsed.declarations.is_empty());
    }

    #[test]
    fn lockfile_resolution_distinguishes_all_marker_outcomes() {
        let index = LockfileIndex::parse(
            r#"version = 4

[[package]]
name = "single"
version = "1.2.3"

[[package]]
name = "doubled"
version = "1.0.0"

[[package]]
name = "doubled"
version = "2.0.0"
"#,
        )
        .expect("lockfile parses");
        assert_eq!(
            index.resolve("single", None),
            LockResolution::Locked("1.2.3".to_owned())
        );
        assert_eq!(
            index.resolve("doubled", None),
            LockResolution::AmbiguousInLockfile
        );
        assert_eq!(index.resolve("absent", None), LockResolution::NotInLockfile);
    }

    #[test]
    fn member_globs_match_cargo_workspace_semantics() {
        // Literal members, single-segment `*`, `?`, and spanning `**`.
        assert!(member_glob_match("crates/member", "crates/member"));
        assert!(member_glob_match("crates/*", "crates/member"));
        assert!(!member_glob_match("crates/*", "crates/member/nested"));
        assert!(member_glob_match("crates/**", "crates/member/nested"));
        assert!(member_glob_match("crates/**", "crates"));
        assert!(member_glob_match("crates/mem?er", "crates/member"));
        assert!(!member_glob_match("crates/*", "vendor/independent"));
        assert!(member_glob_match("**/nested", "a/b/nested"));
        // Trailing-slash normalization happens in `string_array`; the matcher
        // itself ignores empty segments.
        assert!(member_glob_match("crates//member", "crates/member"));
    }

    #[test]
    fn member_globs_support_character_classes() {
        // Plain classes (PR #314 review): `[ab]` matches exactly one of the
        // listed characters, per the `glob` crate Cargo uses.
        assert!(member_glob_match("crates/[ab]", "crates/a"));
        assert!(member_glob_match("crates/[ab]", "crates/b"));
        assert!(!member_glob_match("crates/[ab]", "crates/c"));
        assert!(!member_glob_match("crates/[ab]", "crates/ab"));
        // Ranges.
        assert!(member_glob_match("crates/pkg-[a-c]", "crates/pkg-b"));
        assert!(!member_glob_match("crates/pkg-[a-c]", "crates/pkg-d"));
        // `-` at a class edge is a literal member, not a range.
        assert!(member_glob_match("crates/[-a]", "crates/-"));
        assert!(member_glob_match("crates/[a-]", "crates/-"));
        // Negation uses the glob crate's `[!…]` form.
        assert!(member_glob_match("crates/[!ab]", "crates/c"));
        assert!(!member_glob_match("crates/[!ab]", "crates/a"));
        assert!(member_glob_match("crates/pkg-[!x-z]", "crates/pkg-a"));
        // A `]` as the first class member is a literal.
        assert!(member_glob_match("crates/[]x]", "crates/]"));
        assert!(member_glob_match("crates/[]x]", "crates/x"));
        assert!(!member_glob_match("crates/[]x]", "crates/y"));
        // Classes compose with the other metacharacters.
        assert!(member_glob_match("crates/[ab]*", "crates/alpha"));
        assert!(member_glob_match("**/[ab]", "x/y/a"));
    }

    #[test]
    fn unclosed_character_class_is_a_literal_bracket() {
        // An unclosed (or empty — `[]` never closes a class) bracket falls
        // back to a literal `[` character; it never silently matches
        // everything or nothing (PR #314 review, documented behavior).
        assert!(member_glob_match("crates/[ab", "crates/[ab"));
        assert!(!member_glob_match("crates/[ab", "crates/a"));
        assert!(member_glob_match("crates/[]", "crates/[]"));
        assert!(!member_glob_match("crates/[]", "crates/a"));
    }

    #[test]
    fn leading_dot_glob_segments_are_normalized() {
        // Cargo accepts `./`-prefixed members/exclude entries; `.` segments
        // are dropped before matching so `./crates/a` compares equal to the
        // repo-relative directory `crates/a` (PR #314 review).
        assert!(member_glob_match("./crates/*", "crates/member"));
        assert!(member_glob_match("./crates/a", "crates/a"));
        // Interior `.` segments resolve the same lexical way (pinned).
        assert!(member_glob_match("crates/./a", "crates/a"));
        assert!(!member_glob_match("./crates/*", "vendor/independent"));
    }

    #[test]
    fn corrupt_lockfile_fails_parse() {
        assert!(LockfileIndex::parse("not [ valid toml").is_none());
    }

    #[test]
    fn path_dependency_dirs_include_target_specific_tables() {
        // PR #314 review: target-specific path deps are automatic workspace
        // members, so the membership closure must see their `path` values;
        // `{ workspace = true }` entries are collected as inherited keys to
        // resolve against the root's `[workspace.dependencies]` table.
        let doc: toml_edit::DocumentMut = r#"[package]
name = "app"

[dependencies]
plain = { path = "plain-dir" }
shared = { workspace = true }

[target.'cfg(unix)'.dependencies]
unixdep = { path = "unix-dir" }
tshared = { workspace = true }

[target.'cfg(windows)'.build-dependencies]
windep = { path = "win-dir" }
"#
        .parse()
        .expect("manifest parses");
        assert_eq!(
            manifest_path_dependency_dirs(&doc),
            ManifestPathDeps {
                literal: vec![
                    "plain-dir".to_owned(),
                    "unix-dir".to_owned(),
                    "win-dir".to_owned()
                ],
                inherited: vec!["shared".to_owned(), "tshared".to_owned()],
            }
        );
    }

    #[test]
    fn workspace_dependency_paths_map_only_path_entries() {
        let doc: toml_edit::DocumentMut = r#"[workspace]
members = ["app"]

[workspace.dependencies]
helper = { path = "helper" }
serde = "1"
tokio = { version = "1", features = ["full"] }
"#
        .parse()
        .expect("manifest parses");
        let paths = workspace_dependency_paths(&doc);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths.get("helper").map(String::as_str), Some("helper"));
    }

    #[test]
    fn absolutize_lexical_resolves_relative_roots_against_the_cwd() {
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(absolutize_lexical(Path::new(".")), cwd);
        assert_eq!(
            absolutize_lexical(Path::new("./sub/../sub")),
            cwd.join("sub"),
            "`.`/`..` components resolve lexically after the cwd join"
        );
        let absolute = cwd.join("a").join(".").join("b").join("..").join("c");
        assert_eq!(
            absolutize_lexical(&absolute),
            cwd.join("a").join("c"),
            "an already-absolute path is normalized in place"
        );
    }

    #[test]
    fn invalid_nearest_lockfile_marks_every_fact_lockfile_unreadable() {
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            "[package]\nname = \"pkg\"\n\n[dependencies]\nserde = \"1\"\n",
            &LockfileStatus::Invalid,
            None,
        );
        assert_eq!(records.len(), 1);
        let payload = records[0].dependency().expect("dependency payload");
        assert_eq!(payload.resolution, "lockfile_unreadable");
        assert_eq!(
            payload.resolved_version, None,
            "an invalid nearest lockfile must never yield a resolved version"
        );
        assert_eq!(
            payload.declared_requirement.as_deref(),
            Some("1"),
            "the declared requirement is still captured as written"
        );
    }

    #[test]
    fn malformed_manifest_yields_one_diagnostic_record() {
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            "[package\nbroken",
            &LockfileStatus::Absent,
            None,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].node_kind_name(), Some("Diagnostic"));
    }

    #[test]
    fn nameless_manifest_with_dependencies_yields_a_diagnostic() {
        // PR #314 review: dependency tables without a usable [package].name
        // cannot be attributed to a declaring package — a silent skip would
        // be a coverage hole, so a distinct skipped-manifest Diagnostic is
        // emitted instead of nothing.
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            "[package]\nversion = \"0.1.0\"\n\n[dependencies]\nserde = \"1\"\n",
            &LockfileStatus::Absent,
            None,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].node_kind_name(), Some("Diagnostic"));
        let GraphRecord::Node { symbol_kind, .. } = &records[0] else {
            panic!("diagnostic must be a node");
        };
        assert_eq!(
            symbol_kind.as_deref(),
            Some(UNATTRIBUTABLE_MANIFEST_DIAGNOSTIC_KIND)
        );
    }

    #[test]
    fn nameless_virtual_manifest_with_dependencies_yields_a_diagnostic() {
        // A virtual manifest wrongly carrying top-level dependency tables is
        // the same unattributable coverage hole.
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            "[workspace]\nmembers = [\"app\"]\n\n[dependencies]\nserde = \"1\"\n",
            &LockfileStatus::Absent,
            None,
        );
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].node_kind_name(), Some("Diagnostic"));
    }

    #[test]
    fn virtual_workspace_manifest_yields_no_records() {
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            "[workspace]\nmembers = [\"a\"]\n",
            &LockfileStatus::Absent,
            None,
        );
        assert!(records.is_empty());
    }
}
