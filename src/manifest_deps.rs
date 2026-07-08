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

fn unparseable_manifest_diagnostic(repository_id: &str, manifest_path: &str) -> GraphRecord {
    GraphRecord::node(
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
    )
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
    let mut lockfile_cache: BTreeMap<String, LockfileStatus> = BTreeMap::new();
    for manifest in discover_cargo_manifests(repo_root)? {
        let Ok(manifest_text) = std::fs::read_to_string(&manifest.path) else {
            // An unreadable manifest is reported like an unparseable one:
            // a diagnostic fact, never a failed scan.
            records.push(unparseable_manifest_diagnostic(
                repository_id,
                &manifest.repo_relative_path,
            ));
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

/// Finds and parses the nearest `Cargo.lock`, walking from the manifest's
/// directory up to the repository root. Per-directory outcomes are cached so
/// workspace members sharing a root lockfile parse it once.
///
/// The nearest `Cargo.lock` that *exists* is authoritative: when it fails to
/// read or parse, the walk stops with [`LockfileStatus::Invalid`] instead of
/// consulting an ancestor lockfile whose versions would be unrelated
/// fabrications (PR #314 review).
fn nearest_lockfile(
    repo_root: &Path,
    manifest_repo_relative_path: &str,
    cache: &mut BTreeMap<String, LockfileStatus>,
) -> LockfileStatus {
    let mut segments: Vec<&str> = manifest_repo_relative_path.split('/').collect();
    // Drop the `Cargo.toml` file name, keeping the containing directory.
    segments.pop();
    loop {
        let dir_key = segments.join("/");
        let status = cache.get(&dir_key).cloned().unwrap_or_else(|| {
            let mut candidate = repo_root.to_path_buf();
            for segment in &segments {
                candidate.push(segment);
            }
            candidate.push("Cargo.lock");
            let status = match std::fs::read_to_string(&candidate) {
                Ok(text) => LockfileIndex::parse(&text)
                    .map_or(LockfileStatus::Invalid, LockfileStatus::Found),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    LockfileStatus::Absent
                }
                // The file exists (or its state is unknowable) but cannot be
                // read: it must not be skipped in favor of an ancestor.
                Err(_) => LockfileStatus::Invalid,
            };
            cache.insert(dir_key, status.clone());
            status
        });
        if !matches!(status, LockfileStatus::Absent) {
            return status;
        }
        if segments.pop().is_none() {
            return LockfileStatus::Absent;
        }
    }
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
