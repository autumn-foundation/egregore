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
    path::Path,
};

use crate::{
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

/// The three captured dependency tables in documented output order.
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
    /// `ambiguous_in_lockfile`. Without a usable requirement (absent or
    /// unparseable), a sole locked version resolves directly and several stay
    /// `ambiguous_in_lockfile`. A resolved version is never fabricated.
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
}

/// Parsed dependency surface of one manifest.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ManifestDependencies {
    /// `[package].name`; `None` for a virtual workspace manifest.
    pub package_name: Option<String>,
    /// Declarations in documented order: table order (`normal`, `dev`,
    /// `build`), then crate name, then the declared-as manifest key.
    pub declarations: Vec<DeclaredDependency>,
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
    let package_name = doc
        .get("package")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|package| package.get("name"))
        .and_then(|name| name.as_str())
        .map(str::to_owned);

    let mut declarations = Vec::new();
    for kind in DEPENDENCY_KINDS {
        let Some(table) = doc
            .get(kind.table())
            .and_then(toml_edit::Item::as_table_like)
        else {
            continue;
        };
        let mut entries: Vec<DeclaredDependency> = table
            .iter()
            .map(|(key, item)| declared_dependency(key, item, kind))
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
        declarations,
    })
}

/// Interprets one dependency table entry.
///
/// `serde = "1"` declares requirement `"1"`; `serde = { version = "1", .. }`
/// declares the `version` key; a `package = "real-name"` key renames the
/// entry, so the *crate* name is the `package` value. A declaration without a
/// `version` key (pure `path`/`git`/`workspace = true`) carries no
/// requirement — nothing is fabricated.
fn declared_dependency(
    key: &str,
    item: &toml_edit::Item,
    kind: DependencyKind,
) -> DeclaredDependency {
    let mut name = key.to_owned();
    let mut declared_as = None;
    let mut declared_requirement = None;
    if let Some(requirement) = item.as_str() {
        declared_requirement = Some(requirement.to_owned());
    } else if let Some(spec) = item.as_table_like() {
        if let Some(package) = spec.get("package").and_then(|v| v.as_str()) {
            package.clone_into(&mut name);
            declared_as = Some(key.to_owned());
        }
        if let Some(version) = spec.get("version").and_then(|v| v.as_str()) {
            declared_requirement = Some(version.to_owned());
        }
    }
    DeclaredDependency {
        name,
        declared_as,
        kind,
        declared_requirement,
    }
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
) -> Vec<GraphRecord> {
    let Ok(parsed) = parse_manifest_dependencies(manifest_text) else {
        return vec![unparseable_manifest_diagnostic(
            repository_id,
            manifest_path,
        )];
    };
    let Some(declaring_package) = parsed.package_name else {
        // Virtual workspace manifests cannot declare package dependencies.
        return Vec::new();
    };
    parsed
        .declarations
        .into_iter()
        .map(|declaration| {
            dependency_record(
                repository_id,
                manifest_path,
                &declaring_package,
                &declaration,
                lockfile,
            )
        })
        .collect()
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
        let manifest_records = manifest_dependency_records(
            repository_id,
            &manifest.repo_relative_path,
            &manifest_text,
            &lockfile,
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
    /// A workspace root: `members` / `exclude` globs plus the root package's
    /// in-tree `path = "…"` dependencies (transitively — Cargo treats them as
    /// automatic members) decide coverage.
    Workspace {
        members: Vec<String>,
        exclude: Vec<String>,
        path_members: Vec<String>,
    },
}

/// Per-directory caches for the lockfile walk: what lockfile the directory
/// holds and what its manifest says about workspace membership.
#[derive(Debug, Default)]
struct LockfileWalkCache {
    lockfiles: BTreeMap<String, LockfileStatus>,
    workspaces: BTreeMap<String, WorkspaceFacts>,
}

/// Finds and parses the `Cargo.lock` Cargo would actually use for a manifest,
/// walking from the manifest's directory up to the repository root.
/// Per-directory outcomes are cached so workspace members sharing a root
/// lockfile parse it once.
///
/// Boundary rules (PR #314 review):
///
/// - The manifest's **own** directory's lockfile is always its own.
/// - An **ancestor** directory's lockfile is accepted only when that
///   directory's `Cargo.toml` declares a `[workspace]` whose `members` globs
///   include the manifest's directory and whose `exclude` globs do not — an
///   independent nested crate or an excluded member never resolves from an
///   unrelated ancestor lockfile (`no_lockfile` instead).
/// - A plain package manifest (no `[workspace]`) or a stray lockfile with no
///   manifest beside it is walked past, mirroring Cargo's workspace
///   discovery; an unreadable/unparseable ancestor manifest stops the walk
///   without accepting anything (membership cannot be verified).
/// - A candidate lockfile that exists but cannot be read or parsed stops the
///   walk with [`LockfileStatus::Invalid`] — never a fallback to a higher
///   ancestor.
fn nearest_lockfile(
    repo_root: &Path,
    manifest_repo_relative_path: &str,
    cache: &mut LockfileWalkCache,
) -> LockfileStatus {
    let mut segments: Vec<&str> = manifest_repo_relative_path.split('/').collect();
    // Drop the `Cargo.toml` file name, keeping the containing directory.
    segments.pop();
    let manifest_dir: Vec<&str> = segments.clone();

    // The manifest's own directory: its lockfile is unconditionally its own,
    // and a manifest that itself declares `[workspace]` IS a workspace root —
    // lockfile or not — so the walk never proceeds to an outer workspace
    // (PR #314 review).
    let own = lockfile_in_dir(repo_root, &segments, cache);
    if !matches!(own, LockfileStatus::Absent) {
        return own;
    }
    if matches!(
        workspace_facts_in_dir(repo_root, &segments, cache),
        WorkspaceFacts::Workspace { .. }
    ) {
        return LockfileStatus::Absent;
    }

    loop {
        if segments.pop().is_none() {
            return LockfileStatus::Absent;
        }
        match workspace_facts_in_dir(repo_root, &segments, cache) {
            // The first `[workspace]`-declaring ancestor is the crate's
            // workspace root, whether or not a lockfile sits beside it:
            // membership decides between that root's own lockfile state and
            // a standalone `no_lockfile` — never an outer lockfile.
            WorkspaceFacts::Workspace {
                members,
                exclude,
                path_members,
            } => {
                let rel = manifest_dir[segments.len()..].join("/");
                let is_member = (members.iter().any(|glob| member_glob_match(glob, &rel))
                    || path_members.iter().any(|member| member == &rel))
                    && !exclude.iter().any(|glob| member_glob_match(glob, &rel));
                if is_member {
                    return lockfile_in_dir(repo_root, &segments, cache);
                }
                return LockfileStatus::Absent;
            }
            // Membership cannot be verified: never fabricate a resolution.
            WorkspaceFacts::Unverifiable => return LockfileStatus::Absent,
            // Not a workspace root (plain package, or a stray lockfile with
            // no manifest): Cargo's discovery walks past it — and past any
            // lockfile it holds.
            WorkspaceFacts::NoManifest | WorkspaceFacts::PackageOnly => {}
        }
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
    let mut root_dir = repo_root.to_path_buf();
    for segment in segments {
        root_dir.push(segment);
    }
    let facts = match std::fs::read_to_string(&candidate) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WorkspaceFacts::NoManifest,
        Err(_) => WorkspaceFacts::Unverifiable,
        Ok(text) => parse_workspace_facts(&text, &root_dir),
    };
    cache.workspaces.insert(dir_key, facts.clone());
    facts
}

/// Classifies one manifest body as a workspace root, a plain package, or
/// unverifiable. For a workspace root that is also a package, the in-tree
/// `path = "…"` dependency closure is collected as automatic members
/// (Cargo semantics; PR #314 review).
fn parse_workspace_facts(text: &str, root_dir: &Path) -> WorkspaceFacts {
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return WorkspaceFacts::Unverifiable;
    };
    let Some(workspace) = doc
        .get("workspace")
        .and_then(toml_edit::Item::as_table_like)
    else {
        return WorkspaceFacts::PackageOnly;
    };
    let members = string_array(workspace.get("members"));
    let exclude = string_array(workspace.get("exclude"));
    let path_members = path_dependency_closure(&doc, root_dir, &members, &exclude);
    WorkspaceFacts::Workspace {
        members,
        exclude,
        path_members,
    }
}

/// Collects the transitive in-tree `path = "…"` dependency directories of a
/// workspace, relative to the root. Cargo treats these as automatic
/// workspace members even when `members` does not list them; the closure
/// grows from the root package (when the root manifest is one) AND from
/// every explicit members-glob member — so a virtual root's members
/// contribute their path dependencies too (PR #314 review). Paths escaping
/// the root directory (`..` or an absolute path beyond it) are outside this
/// slice and are skipped; each manifest is parsed with `toml_edit` — never
/// `cargo metadata`.
fn path_dependency_closure(
    root_doc: &toml_edit::DocumentMut,
    root_dir: &Path,
    members: &[String],
    exclude: &[String],
) -> Vec<String> {
    let mut closure: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = Vec::new();
    if root_doc.get("package").is_some() {
        queue.extend(
            manifest_path_dependency_dirs(root_doc)
                .into_iter()
                .filter_map(|path| normalize_path_dep(root_dir, "", &path)),
        );
    }
    if !members.is_empty() {
        for rel in manifest_dirs_under(root_dir) {
            if members.iter().any(|glob| member_glob_match(glob, &rel))
                && !exclude.iter().any(|glob| member_glob_match(glob, &rel))
            {
                queue.push(rel);
            }
        }
    }
    while let Some(rel) = queue.pop() {
        if !closure.insert(rel.clone()) {
            continue;
        }
        let mut manifest = root_dir.to_path_buf();
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
        for path in manifest_path_dependency_dirs(&doc) {
            if let Some(next) = normalize_path_dep(root_dir, &rel, &path) {
                queue.push(next);
            }
        }
    }
    closure.into_iter().collect()
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

/// Normalizes one `path = "…"` dependency value against the workspace root.
///
/// Relative paths join `base` (the declaring manifest's root-relative
/// directory) with `.`/`..` resolution; an **absolute** path is accepted
/// only when it points inside the workspace root (lexical prefix match) and
/// converts to the root-relative member path. Absolute out-of-tree targets
/// and paths escaping the root are skipped — documented out of scope, never
/// an error (PR #314 review).
fn normalize_path_dep(root_dir: &Path, base: &str, raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    if Path::new(&normalized).is_absolute() {
        let rel = Path::new(&normalized).strip_prefix(root_dir).ok()?;
        let mut parts: Vec<&str> = Vec::new();
        for component in rel.components() {
            match component {
                std::path::Component::Normal(part) => parts.push(part.to_str()?),
                std::path::Component::CurDir => {}
                _ => return None,
            }
        }
        if parts.is_empty() {
            return None;
        }
        return Some(parts.join("/"));
    }
    normalize_in_tree_path(base, &normalized)
}

/// Extracts the `path = "…"` values from a manifest's three captured
/// dependency tables.
fn manifest_path_dependency_dirs(doc: &toml_edit::DocumentMut) -> Vec<String> {
    let mut dirs = Vec::new();
    for kind in DEPENDENCY_KINDS {
        let Some(table) = doc
            .get(kind.table())
            .and_then(toml_edit::Item::as_table_like)
        else {
            continue;
        };
        for (_, item) in table.iter() {
            if let Some(spec) = item.as_table_like()
                && let Some(path) = spec.get("path").and_then(|value| value.as_str())
            {
                dirs.push(path.to_owned());
            }
        }
    }
    dirs
}

/// Joins a `/`-separated base (relative to the workspace root; `""` for the
/// root itself) with a manifest-declared relative path, resolving `.` and
/// `..` segments. Returns `None` when the result escapes the root — such a
/// path dependency is outside this slice's membership check.
fn normalize_in_tree_path(base: &str, path: &str) -> Option<String> {
    let mut stack: Vec<&str> = base.split('/').filter(|s| !s.is_empty()).collect();
    let normalized = path.replace('\\', "/");
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        return None;
    }
    Some(stack.join("/"))
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
/// sequence within one path segment, `?` one non-separator character, and a
/// `**` segment spans zero or more whole segments. No external glob crate.
fn member_glob_match(pattern: &str, path: &str) -> bool {
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
                Some((ch, rest)) => s
                    .split_first()
                    .is_some_and(|(sc, s_rest)| sc == ch && inner(rest, s_rest)),
            }
        }
        let (p, s): (Vec<char>, Vec<char>) = (pattern.chars().collect(), segment.chars().collect());
        inner(&p, &s)
    }
    let pattern_segments: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let path_segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segments_match(&pattern_segments, &path_segments)
}

#[cfg(test)]
mod tests {
    use super::*;

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
                },
                DeclaredDependency {
                    name: "serde".to_owned(),
                    declared_as: None,
                    kind: DependencyKind::Normal,
                    declared_requirement: Some("1.0.228".to_owned()),
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
    fn corrupt_lockfile_fails_parse() {
        assert!(LockfileIndex::parse("not [ valid toml").is_none());
    }

    #[test]
    fn invalid_nearest_lockfile_marks_every_fact_lockfile_unreadable() {
        let records = manifest_dependency_records(
            "repo-id",
            "Cargo.toml",
            "[package]\nname = \"pkg\"\n\n[dependencies]\nserde = \"1\"\n",
            &LockfileStatus::Invalid,
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
        );
        assert!(records.is_empty());
    }
}
