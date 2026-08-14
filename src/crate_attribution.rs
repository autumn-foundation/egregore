//! Owning-Cargo-package attribution for code-graph facts (issue #117).
//!
//! # SPEC
//!
//! Egregore scans a repository as a flat pool of source files: every code fact
//! carries a `repo_relative_path`, but nothing records *which Cargo package
//! owns it*. In a workspace monorepo that makes the natural subsystem boundary
//! of a Rust project — the Cargo package — the one boundary the graph cannot
//! express, forcing agents back onto path-prefix guessing (#83) that conflates
//! a directory with a package and cannot name it.
//!
//! This module is the **pure** resolver behind that attribution. It takes a set
//! of [`ManifestPackageFact`]s — one per `Cargo.toml` discovered in a tree —
//! and answers, for any repo-relative path, which package owns it.
//!
//! ## The rule
//!
//! **Nearest enclosing manifest wins.** The walk visits ancestor directories of
//! the queried path from its own directory outwards to the repository root, and
//! resolves at the first ancestor holding a manifest fact:
//!
//! | Fact at ancestor      | Result                              | Walk       |
//! |-----------------------|-------------------------------------|------------|
//! | a usable package name | attributed to it                    | **stop**   |
//! | `[package]`, no usable name | `unnamed_package`             | **stop**   |
//! | TOML parse failure    | `unparseable_manifest`              | **stop**   |
//! | no `[package]` and no `[workspace]` | `unusable_manifest`   | **stop**   |
//! | unreadable / non-UTF-8 | `manifest_unreadable`              | **stop**   |
//! | virtual (`[workspace]`, no `[package]`) | —                 | *continue* |
//! | none                  | —                                   | *continue* |
//!
//! A virtual manifest declares no package and cannot own a file, so the walk
//! passes it and keeps looking outwards — matching Cargo. Every other manifest
//! form **stops** the walk: inheriting an ancestor's name across an unreadable
//! or nameless boundary would fabricate a package attribution, which the issue
//! forbids outright.
//!
//! Reaching the root with no stop yields [`CrateAttributionReason::VirtualManifestOnly`]
//! when at least one virtual manifest was walked past, and
//! [`CrateAttributionReason::NoEnclosingManifest`] otherwise.
//!
//! ## Purity
//!
//! This module performs **no I/O**: no filesystem access, no process spawning,
//! no clock. Its only inputs are already-harvested facts and `&str` paths.
//! That is what lets the current-tree scan (which reads manifests from the
//! working tree) and history replay (which reads them from Git objects) share
//! one implementation and provably agree — the two harvest sites differ, the
//! rule does not.
//!
//! ## Epistemic limit
//!
//! Attribution is nearest-enclosing-manifest directory containment, never proof
//! the file is compiled into that package.

use std::collections::BTreeMap;

use crate::ir::{CrateAttribution, CrateAttributionReason, GraphRecord, NodeKind};

/// What one discovered `Cargo.toml` says about the package it declares.
///
/// A closed vocabulary: every outcome is either a usable package name or a
/// named, actionable reason there is none. There is deliberately no "maybe"
/// state — an unknown manifest form would have to fall into one of the
/// fail-closed variants rather than silently behave like an absent manifest.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub enum ManifestParseOutcome {
    /// Parsed, carries a `[package]` table, and its `name` is Cargo-valid.
    Package {
        /// The declared package name, exactly as written.
        name: String,
    },
    /// Parsed and carries a `[package]` table, but its `name` is absent or
    /// Cargo-invalid (empty, whitespace-bearing, or otherwise rejected). The
    /// manifest declares a package; Egregore just cannot name it.
    UnnamedPackage,
    /// Parsed with a `[workspace]` table and no `[package]` — a virtual
    /// workspace root. It declares no package, so it owns nothing, and the walk
    /// passes it.
    Virtual,
    /// Parsed, but carries NEITHER `[package]` nor `[workspace]` — a form Cargo
    /// refuses to load. The boundary exists but is unusable, so the walk stops
    /// rather than attributing the subtree to an outer package.
    UnusableManifest,
    /// The manifest is not valid TOML.
    Unparseable,
    /// The manifest could not be read, or is not valid UTF-8.
    Unreadable,
}

/// One discovered `Cargo.toml` and what it declares.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct ManifestPackageFact {
    /// Repo-relative, `/`-separated path of the manifest, with no leading `/`
    /// and no `..` component.
    pub manifest_repo_relative_path: String,
    /// What parsing the manifest yielded.
    pub outcome: ManifestParseOutcome,
}

impl ManifestPackageFact {
    /// Builds a fact from a manifest path and its outcome.
    #[must_use]
    pub fn new(
        manifest_repo_relative_path: impl Into<String>,
        outcome: ManifestParseOutcome,
    ) -> Self {
        Self {
            manifest_repo_relative_path: manifest_repo_relative_path.into(),
            outcome,
        }
    }

    /// The manifest's parent directory as an index key: `""` for a repo-root
    /// manifest, else the `/`-joined directory path with no trailing slash.
    ///
    /// Returns `None` when the path is not a usable repo-relative manifest
    /// path — such a fact is dropped rather than relocated across a manifest
    /// boundary.
    fn directory_key(&self) -> Option<String> {
        let path = self.manifest_repo_relative_path.as_str();
        if !manifest_path_is_repo_relative(path) {
            return None;
        }
        let mut segments: Vec<&str> = path.split('/').collect();
        segments.pop()?;
        Some(segments.join("/"))
    }
}

/// The file name every Cargo manifest carries, case-sensitively.
///
/// Cargo itself is case-sensitive here: a `cargo.toml` is not a manifest, which
/// is why the scan's manifest discovery matches this exact name.
const MANIFEST_FILE_NAME: &str = "Cargo.toml";

/// Whether `path` has the shape of a repo-relative Cargo manifest path.
///
/// This is the shape the ancestor walk can PRODUCE: a non-empty, `/`-separated,
/// relative path whose last segment is `Cargo.toml`, with no `.`/`..` segment,
/// no empty interior segment, no leading `/`, and no Windows drive prefix.
///
/// # What is deliberately NOT disqualifying
///
/// The predicate tests whether the ancestor walk could have PRODUCED the value,
/// and on Unix a filename may contain almost any byte. `normalize_path` joins
/// `Path::components()` with `/`, and the scan's NUL-delimited `ls-files` /
/// `ls-tree` listings are never C-quoted, so all of these survive into a real
/// graph path and must keep owning their subtrees:
///
/// - a **backslash** (`crates/odd\dir/Cargo.toml`) — an ordinary character here,
///   not a separator;
/// - a **colon** outside the drive shape (`vendor:patched/Cargo.toml`);
/// - a **control character**, tab or newline included (`od\td/Cargo.toml`).
///
/// Rejecting any of them dropped the manifest fact from the index, and the
/// subtree then inherited an OUTER package — a fabricated attribution, the one
/// outcome this feature exists to prevent. Text-output safety is a rendering
/// concern, discharged where the value is printed (`sanitized_handle`), not by
/// refusing to record a fact about a directory that really exists.
///
/// Windows-shaped forgeries are still rejected, by rules that already govern
/// them: a backslash-SEPARATED path (`crates\x\Cargo.toml`) has one `/`-segment
/// which is not `Cargo.toml`, and a drive prefix is matched as the exact
/// letter-plus-colon first segment.
///
/// RESIDUAL, stated rather than hidden: a Unix directory named exactly like a
/// drive letter (`C:/crates/Cargo.toml`) is rejected though the walk could
/// reach it. That shape is the canonical absolute-path forgery and a
/// vanishingly unlikely real directory name, so the guard is kept and the
/// false negative accepted.
///
/// One rule, two callers, deliberately: the producer drops a fact it cannot
/// place ([`ManifestPackageFact::directory_key`]), and the reader refuses to
/// present a claim resting on a citation of a shape the producer could never
/// have written ([`crate::ir::CrateAttribution::owning_package`]). A value read
/// back from a store or a hand-edited graph is operator-controlled, so the
/// reader re-checking is what keeps "what a record claims" separate from "what
/// the resolver proved".
#[must_use]
pub fn manifest_path_is_repo_relative(path: &str) -> bool {
    // A manifest path is a repo-relative path that additionally CITES a
    // manifest: its last segment must be exactly `Cargo.toml`, because the
    // value's job is to name the manifest the attribution rests on.
    is_repo_relative_path(path) && path.split('/').next_back() == Some(MANIFEST_FILE_NAME)
}

/// Whether `path` has the shape [`crate::normalize_path`] can PRODUCE.
///
/// That is: a non-empty, `/`-separated, relative path with no `.`/`..` segment,
/// no empty segment, no leading `/`, no Windows drive prefix, and no NUL.
///
/// `normalize_path` keeps only `Component::Normal` and joins with `/`, so every
/// one of those shapes is unreachable from any producer. The rule is shared
/// rather than restated: [`manifest_path_is_repo_relative`] adds the
/// manifest-name requirement on top, and [`manifest_encloses`] applies it to the
/// RECORD path — where a `..` segment reads as an ordinary ancestor, so an
/// escaping path could otherwise be "enclosed" by a directory it escapes out of.
///
/// See [`manifest_path_is_repo_relative`] for what is deliberately NOT
/// disqualifying (backslashes, non-drive colons, control characters other than
/// NUL) and why.
#[must_use]
pub fn is_repo_relative_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }
    // NUL is the ONE byte a POSIX filename cannot contain — it terminates the
    // C string the kernel is handed, and it is the DELIMITER of the `-z` git
    // listings the harvest reads, so no discovery path can deliver one. A tab
    // or a newline names a real directory and is accepted (see above); a NUL
    // provably names nothing, and accepting it would let a crafted record claim
    // scopable ownership and hand back a citation that sanitization rewrites
    // into a path that never existed.
    if path.contains('\0') {
        return false;
    }
    let mut segments = path.split('/');
    // A Windows drive-absolute path (`C:/crates/Cargo.toml`) is not
    // repo-relative. Matched as the EXACT drive shape — one ASCII letter and a
    // colon, as the whole first segment — because a colon is an ordinary
    // character in a POSIX directory name: `vendor:patched/Cargo.toml` is a
    // manifest the walk really reaches, and rejecting it would un-attribute its
    // subtree.
    if segments.next().is_some_and(|first| {
        let mut chars = first.chars();
        matches!(
            (chars.next(), chars.next(), chars.next()),
            (Some(letter), Some(':'), None) if letter.is_ascii_alphabetic()
        )
    }) {
        return false;
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return false;
    }
    true
}

/// A deterministic map from directory to the manifest that sits in it.
///
/// Built once per scanned tree (or per replayed commit) and then queried per
/// record. Construction is order-independent: the same fact multiset always
/// yields the same index.
#[derive(Debug, Clone, Default)]
pub struct CrateAttributionIndex {
    by_dir: BTreeMap<String, ManifestParseOutcome>,
    manifest_by_dir: BTreeMap<String, String>,
}

impl CrateAttributionIndex {
    /// Builds the index from harvested manifest facts.
    ///
    /// Facts are sorted before insertion and the first fact for a directory
    /// wins, so construction is order-independent. Both harvests accept only
    /// the basename `Cargo.toml`, so two facts can claim one directory only by
    /// carrying the IDENTICAL path — in which case the derived `Ord` breaks the
    /// tie on [`ManifestParseOutcome`] rather than on the path. Either way the
    /// winner is deterministic. Facts whose path is not a usable repo-relative
    /// manifest path are dropped.
    #[must_use]
    pub fn from_facts(mut facts: Vec<ManifestPackageFact>) -> Self {
        facts.sort();
        let mut by_dir = BTreeMap::new();
        let mut manifest_by_dir = BTreeMap::new();
        for fact in facts {
            let Some(dir) = fact.directory_key() else {
                continue;
            };
            if by_dir.contains_key(&dir) {
                continue;
            }
            manifest_by_dir.insert(dir.clone(), fact.manifest_repo_relative_path.clone());
            by_dir.insert(dir, fact.outcome);
        }
        Self {
            by_dir,
            manifest_by_dir,
        }
    }

    /// Resolves the owning package of one repo-relative path.
    ///
    /// See the module docs for the full rule table. The walk is bounded by the
    /// repository root because the index only ever holds in-repo manifests.
    #[must_use]
    pub fn attribution_for(&self, repo_relative_path: &str) -> CrateAttribution {
        let mut saw_virtual = false;
        for dir in ancestor_dirs(repo_relative_path) {
            let Some(outcome) = self.by_dir.get(dir.as_str()) else {
                continue;
            };
            let manifest = self
                .manifest_by_dir
                .get(dir.as_str())
                .cloned()
                .unwrap_or_default();
            match outcome {
                ManifestParseOutcome::Package { name } => {
                    return CrateAttribution::attributed(name.clone(), manifest);
                }
                ManifestParseOutcome::UnnamedPackage => {
                    return CrateAttribution::unattributed(CrateAttributionReason::UnnamedPackage);
                }
                ManifestParseOutcome::Unparseable => {
                    return CrateAttribution::unattributed(
                        CrateAttributionReason::UnparseableManifest,
                    );
                }
                ManifestParseOutcome::UnusableManifest => {
                    return CrateAttribution::unattributed(
                        CrateAttributionReason::UnusableManifest,
                    );
                }
                ManifestParseOutcome::Unreadable => {
                    return CrateAttribution::unattributed(
                        CrateAttributionReason::ManifestUnreadable,
                    );
                }
                // A virtual manifest declares no package: walk past it, but
                // remember that we did, so the terminal reason distinguishes
                // "under a virtual workspace root" from "no manifest at all".
                ManifestParseOutcome::Virtual => saw_virtual = true,
            }
        }
        CrateAttribution::unattributed(if saw_virtual {
            CrateAttributionReason::VirtualManifestOnly
        } else {
            CrateAttributionReason::NoEnclosingManifest
        })
    }

    /// Every package name this index can attribute to, sorted and deduplicated.
    #[must_use]
    pub fn package_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .by_dir
            .values()
            .filter_map(|outcome| match outcome {
                ManifestParseOutcome::Package { name } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// `true` when the index holds no manifest facts at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_dir.is_empty()
    }
}

/// The ancestor directories of `repo_relative_path`, nearest first, ending at
/// the repository root (`""`).
///
/// Derived by splitting on `/` and dropping segments, so containment is
/// segment-aware by construction: `crates/foo` can never claim
/// `crates/foobar/src/x.rs`, which a `str::starts_with` comparison would.
fn ancestor_dirs(repo_relative_path: &str) -> Vec<String> {
    let mut segments: Vec<&str> = repo_relative_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    // Drop the file component itself; what remains is its own directory.
    segments.pop();
    let mut dirs = Vec::with_capacity(segments.len() + 1);
    while !segments.is_empty() {
        dirs.push(segments.join("/"));
        segments.pop();
    }
    dirs.push(String::new());
    dirs
}

/// `true` when `manifest_repo_relative_path` could be the manifest the walk
/// resolved for a record at `record_repo_relative_path`.
///
/// Attribution comes from the NEAREST ENCLOSING manifest, so the cited
/// manifest's directory is always an ancestor of — or equal to — the record's
/// own directory. A record at `crates/beta/src/lib.rs` citing
/// `crates/alpha/Cargo.toml` is a shape no walk produces.
///
/// Reuses [`ancestor_dirs`], the SAME function the walk enumerates, so this
/// check cannot drift from the rule it verifies; segment-awareness comes free
/// with it (`crates/alpha` never encloses `crates/alphabet/x.rs`).
///
/// Two correct shapes that look unusual both pass: a manifest's own `File` node
/// cites ITSELF (ancestry by equality), and a repo-root manifest (directory
/// `""`) encloses every path in the repository.
#[must_use]
pub fn manifest_encloses(
    manifest_repo_relative_path: &str,
    record_repo_relative_path: &str,
) -> bool {
    // The RECORD path is operator-controlled too, and ancestry is meaningless
    // over a path no producer emits: `ancestor_dirs` always ends at the repo
    // root, so a root manifest would "enclose" `../outside.rs`, and a `..`
    // segment reads as an ordinary ancestor, so `crates/a/../../outside.rs`
    // would be enclosed by `crates/a/Cargo.toml` on its way out.
    if !is_repo_relative_path(record_repo_relative_path) {
        return false;
    }
    let mut manifest_segments: Vec<&str> = manifest_repo_relative_path
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    // Drop the `Cargo.toml` component; what remains is the package directory.
    manifest_segments.pop();
    let manifest_dir = manifest_segments.join("/");
    ancestor_dirs(record_repo_relative_path).contains(&manifest_dir)
}

/// `true` when nodes of `kind` carry crate attribution.
///
/// An **exhaustive match with no wildcard arm** (the #247 completeness
/// invariant): a new [`NodeKind`] fails to compile until it is deliberately
/// classified, so attribution presence can never drift into a partial function
/// by accident. That totality is load-bearing — it is what makes an absent
/// field mean "produced before issue #117" rather than "this kind happens not
/// to be covered".
#[must_use]
pub const fn carries_crate_attribution(kind: NodeKind) -> bool {
    match kind {
        // Path-bearing code-graph facts: the subject of the attribution.
        NodeKind::File
        | NodeKind::Module
        | NodeKind::Symbol
        | NodeKind::Import
        | NodeKind::Diagnostic
        | NodeKind::PanicRiskSite
        | NodeKind::DebtMarker
        | NodeKind::UnsafeSite
        | NodeKind::DependencyDeclaration
        // A `Change` records that one path changed in one commit, so it names a
        // file and its owning package is meaningful at that commit.
        | NodeKind::Change => true,
        // Repository- and commit-scoped records: they describe the repository or
        // a commit as a whole, carry no path, and no package can own them.
        NodeKind::Repository
        | NodeKind::Commit
        | NodeKind::ScanCoverage
        // Every non-code-graph domain.
        | NodeKind::SemanticDrift
        | NodeKind::EmbeddingModel
        | NodeKind::EmbeddingVector
        | NodeKind::Agent
        | NodeKind::AgentSession
        | NodeKind::Observation
        | NodeKind::Task
        | NodeKind::AcceptanceCriterion
        | NodeKind::ExternalLink
        | NodeKind::Product
        | NodeKind::Project
        | NodeKind::Plan
        | NodeKind::GitHubIssue
        | NodeKind::PR
        | NodeKind::Review
        | NodeKind::ExternalIdentity
        | NodeKind::ReviewStateTransition
        | NodeKind::LocalTask
        | NodeKind::Artifact
        | NodeKind::Verification
        | NodeKind::CommandEvidence
        | NodeKind::AgentRun
        | NodeKind::AgentTurn
        | NodeKind::ToolCall
        | NodeKind::CommandRun
        | NodeKind::FileEdit
        | NodeKind::PatchArtifact
        | NodeKind::Failure
        | NodeKind::Decision
        | NodeKind::TestRun
        | NodeKind::CIStatus
        | NodeKind::BenchmarkRun
        | NodeKind::CoverageReport
        | NodeKind::ProofResult
        | NodeKind::PromoteCandidate
        | NodeKind::PromotionPrompt
        | NodeKind::PromotionDecision
        | NodeKind::Preference
        | NodeKind::WorkflowRule
        | NodeKind::NamingDecision
        | NodeKind::Constraint
        | NodeKind::CostUsage
        | NodeKind::Retraction
        | NodeKind::LogSource
        | NodeKind::ErrorSignature
        | NodeKind::LogEvent
        | NodeKind::LogOccurrenceBucket => false,
    }
}

/// Stamps crate attribution onto every path-bearing code-graph node in
/// `records`.
///
/// A **post-extraction rewrite pass**, mirroring
/// `languages::cross_file::apply_out_of_line_test_scope`: attribution is never
/// threaded through the language extractors and is never an input to
/// `stable_id`, so record IDs are unchanged by this pass.
///
/// Callers on the history path MUST pass only the slice belonging to one
/// commit. A symbol's stable ID carries no commit component, so applying one
/// commit's index across the whole graph would stamp every historical version
/// of a record with the wrong tree's manifests.
pub fn apply_crate_attribution(records: &mut [GraphRecord], index: &CrateAttributionIndex) {
    apply_crate_attribution_where(records, index, |_| true);
}

/// [`apply_crate_attribution`], restricted to the records a predicate selects.
///
/// History replay needs this because not every record in a commit's slice
/// describes that commit's tree. A `Change` recording a DELETION names a path
/// that is no longer there, so resolving it against the post-commit tree finds
/// whatever manifest the walk reaches next — the enclosing package for a lone
/// file deletion, but an OUTER package when the commit removes a manifest and
/// its sources together. Re-resolving just those records against the parent
/// tree keeps one rule and one implementation, applied to the tree each record
/// actually describes.
pub fn apply_crate_attribution_where(
    records: &mut [GraphRecord],
    index: &CrateAttributionIndex,
    mut selects: impl FnMut(&GraphRecord) -> bool,
) {
    for record in records {
        if !selects(record) {
            continue;
        }
        let GraphRecord::Node {
            kind,
            repo_relative_path,
            crate_attribution,
            ..
        } = record
        else {
            continue;
        };
        if !carries_crate_attribution(*kind) {
            continue;
        }
        // A node with no path cannot be located in the manifest tree; leaving
        // the field absent is honest, and substituting `""` would silently
        // claim the root package owns it.
        let Some(path) = repo_relative_path.as_deref() else {
            continue;
        };
        *crate_attribution = Some(index.attribution_for(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CrateAttributionStatus;

    fn package(path: &str, name: &str) -> ManifestPackageFact {
        ManifestPackageFact::new(
            path,
            ManifestParseOutcome::Package {
                name: name.to_owned(),
            },
        )
    }

    fn virtual_manifest(path: &str) -> ManifestPackageFact {
        ManifestPackageFact::new(path, ManifestParseOutcome::Virtual)
    }

    fn resolved(index: &CrateAttributionIndex, path: &str) -> Option<String> {
        index.attribution_for(path).package_name
    }

    fn reason(index: &CrateAttributionIndex, path: &str) -> Option<CrateAttributionReason> {
        index.attribution_for(path).unattributed_reason
    }

    #[test]
    fn nearest_manifest_wins_over_ancestor() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("crates/outer/Cargo.toml", "outer"),
            package("crates/outer/vendor/inner/Cargo.toml", "inner"),
        ]);
        assert_eq!(
            resolved(&index, "crates/outer/vendor/inner/src/lib.rs").as_deref(),
            Some("inner")
        );
        assert_eq!(
            resolved(&index, "crates/outer/src/lib.rs").as_deref(),
            Some("outer")
        );
        // NEGATIVE: nothing under the nested crate may be claimed by the parent.
        for path in [
            "crates/outer/vendor/inner/src/lib.rs",
            "crates/outer/vendor/inner/src/deep/mod.rs",
            "crates/outer/vendor/inner/build.rs",
        ] {
            assert_ne!(
                resolved(&index, path).as_deref(),
                Some("outer"),
                "{path} must not be claimed by the parent crate"
            );
        }
    }

    #[test]
    fn sibling_prefix_never_bleeds() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("crates/foo/Cargo.toml", "foo"),
            package("crates/foobar/Cargo.toml", "foobar"),
        ]);
        assert_eq!(
            resolved(&index, "crates/foobar/src/lib.rs").as_deref(),
            Some("foobar")
        );
        assert_eq!(
            resolved(&index, "crates/foo/src/lib.rs").as_deref(),
            Some("foo")
        );
        // NEGATIVE: zero `crates/foobar/**` paths resolve to `foo`.
        let bleeds = [
            "crates/foobar/src/lib.rs",
            "crates/foobar/src/a/b.rs",
            "crates/foobar/tests/t.rs",
        ]
        .into_iter()
        .filter(|path| resolved(&index, path).as_deref() == Some("foo"))
        .count();
        assert_eq!(bleeds, 0, "sibling prefix bleed detected");
    }

    /// A FILE whose name string-extends a manifest DIRECTORY name must not be
    /// claimed by that manifest.
    ///
    /// Distinct from `sibling_prefix_never_bleeds`, which pits two sibling
    /// directories against each other: there the correct walk resolves at the
    /// deeper manifest and never reaches a lenient comparison. Here NO manifest
    /// encloses the file, so the walk falls through to the terminal reason —
    /// which is exactly where a `str::starts_with` fallback would fire and
    /// fabricate an owner.
    #[test]
    fn a_file_extending_a_manifest_dir_name_is_not_claimed_by_it() {
        let index = CrateAttributionIndex::from_facts(vec![
            virtual_manifest("Cargo.toml"),
            package("crates/foo/Cargo.toml", "foo"),
        ]);
        for path in [
            "crates/foobar.rs",
            "crates/foo.rs",
            "crates/foo_helper.rs",
            "crates/foobar/src/lib.rs",
        ] {
            assert_eq!(
                resolved(&index, path),
                None,
                "{path} is enclosed by no package and must not be claimed by `foo`"
            );
            assert_eq!(
                reason(&index, path),
                Some(CrateAttributionReason::VirtualManifestOnly),
                "{path}"
            );
        }
        // Anti-vacuity: the genuinely enclosed file still resolves.
        assert_eq!(
            resolved(&index, "crates/foo/src/lib.rs").as_deref(),
            Some("foo")
        );
    }

    #[test]
    fn virtual_manifest_is_walked_past_to_ancestor_package() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("Cargo.toml", "root"),
            virtual_manifest("crates/Cargo.toml"),
            package("crates/a/Cargo.toml", "a"),
        ]);
        assert_eq!(resolved(&index, "crates/a/src/x.rs").as_deref(), Some("a"));
        // A file directly under the virtual manifest falls through to the root
        // package — the virtual manifest never stops the walk.
        assert_eq!(resolved(&index, "crates/stray.rs").as_deref(), Some("root"));
    }

    #[test]
    fn stray_under_virtual_root_only_is_virtual_manifest_only() {
        let index = CrateAttributionIndex::from_facts(vec![
            virtual_manifest("Cargo.toml"),
            package("crates/a/Cargo.toml", "a"),
        ]);
        let attribution = index.attribution_for("scripts/gen.rs");
        assert_eq!(attribution.status, CrateAttributionStatus::Unattributed);
        assert_eq!(
            attribution.unattributed_reason,
            Some(CrateAttributionReason::VirtualManifestOnly)
        );
        assert!(attribution.package_name.is_none());
        assert!(attribution.manifest_repo_relative_path.is_none());
    }

    #[test]
    fn stray_with_no_manifest_anywhere_is_no_enclosing_manifest() {
        let index = CrateAttributionIndex::from_facts(Vec::new());
        assert_eq!(
            reason(&index, "scripts/gen.rs"),
            Some(CrateAttributionReason::NoEnclosingManifest)
        );
    }

    #[test]
    fn unparseable_ancestor_stops_walk_fail_closed() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("Cargo.toml", "root"),
            ManifestPackageFact::new("crates/foo/Cargo.toml", ManifestParseOutcome::Unparseable),
        ]);
        assert_eq!(
            reason(&index, "crates/foo/src/lib.rs"),
            Some(CrateAttributionReason::UnparseableManifest)
        );
        // NEGATIVE: never inherits the ancestor's name across the broken boundary.
        assert_eq!(resolved(&index, "crates/foo/src/lib.rs"), None);
    }

    #[test]
    fn unreadable_ancestor_stops_walk_fail_closed() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("Cargo.toml", "root"),
            ManifestPackageFact::new("crates/foo/Cargo.toml", ManifestParseOutcome::Unreadable),
        ]);
        assert_eq!(
            reason(&index, "crates/foo/src/lib.rs"),
            Some(CrateAttributionReason::ManifestUnreadable)
        );
        assert_eq!(resolved(&index, "crates/foo/src/lib.rs"), None);
    }

    /// A manifest carrying neither `[package]` nor `[workspace]` is one Cargo
    /// refuses to load, so it STOPS the walk rather than being treated as a
    /// virtual root.
    #[test]
    fn unusable_manifest_stops_the_walk_unlike_a_real_virtual_root() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("Cargo.toml", "root"),
            ManifestPackageFact::new("nested/Cargo.toml", ManifestParseOutcome::UnusableManifest),
            virtual_manifest("group/Cargo.toml"),
        ]);
        assert_eq!(
            reason(&index, "nested/src/lib.rs"),
            Some(CrateAttributionReason::UnusableManifest)
        );
        assert_eq!(
            resolved(&index, "nested/src/lib.rs"),
            None,
            "an unusable boundary must never inherit the ancestor's package"
        );
        // A REAL virtual root still walks past to the ancestor package.
        assert_eq!(resolved(&index, "group/loose.rs").as_deref(), Some("root"));
    }

    #[test]
    fn unnamed_package_is_distinguished_from_virtual() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("Cargo.toml", "root"),
            ManifestPackageFact::new(
                "crates/named/Cargo.toml",
                ManifestParseOutcome::UnnamedPackage,
            ),
            virtual_manifest("crates/virt/Cargo.toml"),
        ]);
        // An unnamed `[package]` STOPS the walk: it declares a package we
        // cannot name, so inheriting `root` would be a fabricated fact.
        assert_eq!(
            reason(&index, "crates/named/src/lib.rs"),
            Some(CrateAttributionReason::UnnamedPackage)
        );
        // A virtual manifest declares no package: walk past it to `root`.
        assert_eq!(
            resolved(&index, "crates/virt/src/lib.rs").as_deref(),
            Some("root")
        );
    }

    #[test]
    fn package_name_never_derived_from_directory_name() {
        let index = CrateAttributionIndex::from_facts(vec![package(
            "crates/widget-dir/Cargo.toml",
            "totally-different",
        )]);
        assert_eq!(
            resolved(&index, "crates/widget-dir/src/lib.rs").as_deref(),
            Some("totally-different")
        );
        assert!(
            !index.package_names().contains(&"widget-dir"),
            "a directory name must never surface as a package name"
        );
    }

    #[test]
    fn repo_root_package_attributes_root_and_nested_files() {
        let index = CrateAttributionIndex::from_facts(vec![package("Cargo.toml", "r")]);
        for path in ["src/lib.rs", "build.rs", "src/a/b/c.rs", "tests/t.rs"] {
            assert_eq!(
                resolved(&index, path).as_deref(),
                Some("r"),
                "{path} must be owned by the root package"
            );
        }
    }

    #[test]
    fn attributed_carries_both_name_and_manifest_path() {
        let index =
            CrateAttributionIndex::from_facts(vec![package("crates/foo/Cargo.toml", "foo")]);
        let attribution = index.attribution_for("crates/foo/src/lib.rs");
        assert_eq!(attribution.status, CrateAttributionStatus::Attributed);
        assert_eq!(attribution.package_name.as_deref(), Some("foo"));
        assert_eq!(
            attribution.manifest_repo_relative_path.as_deref(),
            Some("crates/foo/Cargo.toml")
        );
        assert!(attribution.unattributed_reason.is_none());
    }

    #[test]
    fn manifest_self_attributes_to_its_own_package() {
        let index =
            CrateAttributionIndex::from_facts(vec![package("crates/foo/Cargo.toml", "foo")]);
        // The nearest enclosing manifest of a manifest is itself.
        assert_eq!(
            resolved(&index, "crates/foo/Cargo.toml").as_deref(),
            Some("foo")
        );
    }

    #[test]
    fn index_is_identical_from_shuffled_fact_order() {
        let facts = vec![
            package("Cargo.toml", "root"),
            virtual_manifest("crates/Cargo.toml"),
            package("crates/a/Cargo.toml", "a"),
            package("crates/ab/Cargo.toml", "ab"),
            ManifestPackageFact::new("crates/bad/Cargo.toml", ManifestParseOutcome::Unparseable),
        ];
        let probes = [
            "crates/a/src/lib.rs",
            "crates/ab/src/lib.rs",
            "crates/bad/src/lib.rs",
            "crates/loose.rs",
            "src/main.rs",
            "README.md",
        ];
        let mut permutations = vec![facts.clone()];
        let mut rotated = facts.clone();
        rotated.rotate_left(2);
        permutations.push(rotated);
        let mut reversed = facts;
        reversed.reverse();
        permutations.push(reversed);

        let baseline: Vec<CrateAttribution> = {
            let index = CrateAttributionIndex::from_facts(permutations[0].clone());
            probes.iter().map(|p| index.attribution_for(p)).collect()
        };
        for permutation in permutations {
            let index = CrateAttributionIndex::from_facts(permutation);
            let observed: Vec<CrateAttribution> =
                probes.iter().map(|p| index.attribution_for(p)).collect();
            assert_eq!(observed, baseline, "index must be order-independent");
        }
    }

    #[test]
    fn duplicate_dir_facts_resolve_deterministically() {
        // Two facts claiming the same directory (only reachable via a caller
        // bug or a case-insensitive filesystem). Because both harvests accept
        // only the basename `Cargo.toml`, such facts necessarily share a path,
        // so the derived `Ord` decides on outcome — the point is that the
        // winner is STABLE, whichever component breaks the tie.
        let facts = vec![
            package("crates/foo/Cargo.toml", "second"),
            ManifestPackageFact::new(
                "crates/foo/Cargo.toml",
                ManifestParseOutcome::Package {
                    name: "first".to_owned(),
                },
            ),
        ];
        for _ in 0..5 {
            let index = CrateAttributionIndex::from_facts(facts.clone());
            assert_eq!(
                resolved(&index, "crates/foo/src/lib.rs").as_deref(),
                Some("first"),
                "duplicate directory facts must resolve deterministically"
            );
        }
    }

    #[test]
    fn unusable_manifest_paths_are_dropped_not_relocated() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("Cargo.toml", "root"),
            // Escaping, absolute, and backslash-separated paths are dropped
            // rather than reinterpreted across a manifest boundary.
            package("../outside/Cargo.toml", "outside"),
            package("/abs/Cargo.toml", "abs"),
            package(r"crates\win\Cargo.toml", "win"),
        ]);
        let names = index.package_names();
        assert_eq!(names, vec!["root"]);
        assert_eq!(resolved(&index, "src/lib.rs").as_deref(), Some("root"));
    }

    #[test]
    fn attributed_iff_package_name_and_manifest_present() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("crates/a/Cargo.toml", "a"),
            virtual_manifest("Cargo.toml"),
            ManifestPackageFact::new("crates/bad/Cargo.toml", ManifestParseOutcome::Unparseable),
            ManifestPackageFact::new("crates/nn/Cargo.toml", ManifestParseOutcome::UnnamedPackage),
            ManifestPackageFact::new("crates/ur/Cargo.toml", ManifestParseOutcome::Unreadable),
        ]);
        for path in [
            "crates/a/src/lib.rs",
            "crates/bad/src/lib.rs",
            "crates/nn/src/lib.rs",
            "crates/ur/src/lib.rs",
            "loose.rs",
        ] {
            let attribution = index.attribution_for(path);
            match attribution.status {
                CrateAttributionStatus::Attributed => {
                    assert!(attribution.package_name.is_some(), "{path}");
                    assert!(attribution.manifest_repo_relative_path.is_some(), "{path}");
                    assert!(attribution.unattributed_reason.is_none(), "{path}");
                }
                CrateAttributionStatus::Unattributed => {
                    assert!(attribution.package_name.is_none(), "{path}");
                    assert!(attribution.manifest_repo_relative_path.is_none(), "{path}");
                    assert!(attribution.unattributed_reason.is_some(), "{path}");
                }
            }
        }
    }

    #[test]
    fn package_names_are_sorted_and_deduplicated() {
        let index = CrateAttributionIndex::from_facts(vec![
            package("crates/z/Cargo.toml", "zeta"),
            package("crates/a/Cargo.toml", "alpha"),
            package("crates/dup/Cargo.toml", "alpha"),
            virtual_manifest("Cargo.toml"),
        ]);
        assert_eq!(index.package_names(), vec!["alpha", "zeta"]);
    }

    #[test]
    fn adding_a_deeper_manifest_only_narrows_attribution() {
        // Property: a manifest strictly deeper than the current winner changes
        // the answer only for paths under it, and only to that manifest.
        let base = vec![package("crates/a/Cargo.toml", "a")];
        let deeper = {
            let mut facts = base.clone();
            facts.push(package("crates/a/sub/Cargo.toml", "sub"));
            facts
        };
        let base_index = CrateAttributionIndex::from_facts(base);
        let deeper_index = CrateAttributionIndex::from_facts(deeper);
        for path in ["crates/a/src/lib.rs", "crates/a/x.rs", "other/y.rs"] {
            assert_eq!(
                base_index.attribution_for(path),
                deeper_index.attribution_for(path),
                "{path} is outside the added manifest's subtree"
            );
        }
        assert_eq!(
            resolved(&deeper_index, "crates/a/sub/src/lib.rs").as_deref(),
            Some("sub")
        );
    }

    #[test]
    fn adding_a_shallower_manifest_never_changes_a_resolved_answer() {
        let base = vec![package("crates/a/Cargo.toml", "a")];
        let shallower = {
            let mut facts = base.clone();
            facts.push(package("Cargo.toml", "root"));
            facts
        };
        let base_index = CrateAttributionIndex::from_facts(base);
        let shallower_index = CrateAttributionIndex::from_facts(shallower);
        for path in ["crates/a/src/lib.rs", "crates/a/deep/x.rs"] {
            assert_eq!(
                base_index.attribution_for(path),
                shallower_index.attribution_for(path),
                "{path} already resolved at a deeper manifest"
            );
        }
    }

    /// The shape rule is shared by the producer (which drops a fact it cannot
    /// place) and the reader (which refuses a claim resting on a citation the
    /// walk could not have written), so it is pinned directly.
    #[test]
    fn manifest_path_shape_admits_only_walkable_citations() {
        for accepted in [
            "Cargo.toml",
            "crates/alpha/Cargo.toml",
            "crates/alpha/vendor/inner/Cargo.toml",
            // Legal in a POSIX directory name and reachable by the walk, so it
            // must not be a false negative: only a FIRST-segment colon is a
            // Windows drive prefix.
            "crates/a:b/Cargo.toml",
            "crates/with space/Cargo.toml",
            "crates/café/Cargo.toml",
            "crates/..hidden/Cargo.toml",
            // A backslash is an ordinary filename character on Unix, so this
            // is a manifest the walk really reaches — rejecting it would
            // un-attribute a real subtree.
            "crates/odd\\dir/Cargo.toml",
            // So are control characters. Perverse directory names, but legal
            // ones the NUL-delimited git listings deliver intact; the text
            // render sanitizes them on the way out (`sanitized_handle`) rather
            // than the index refusing to hold the fact.
            "crates/od\td/Cargo.toml",
            "crates/od\nd/Cargo.toml",
            "crates/\u{1b}[31m/Cargo.toml",
        ] {
            assert!(
                manifest_path_is_repo_relative(accepted),
                "`{accepted}` is a path the walk produces"
            );
        }
        for (label, rejected) in [
            ("empty", ""),
            ("absolute", "/etc/Cargo.toml"),
            ("escaping", "../outside/Cargo.toml"),
            ("interior escape", "crates/../../Cargo.toml"),
            ("dot segment", "crates/./Cargo.toml"),
            ("empty interior segment", "crates//Cargo.toml"),
            ("trailing slash", "crates/x/Cargo.toml/"),
            ("windows drive", "C:/crates/Cargo.toml"),
            (
                "windows drive, backslash-separated",
                "C:\\crates\\Cargo.toml",
            ),
            // Backslash-SEPARATED, so its only `/`-segment is the whole string
            // and the manifest-name rule rejects it — no blanket backslash ban
            // needed, and none is applied.
            ("backslash-separated", "crates\\x\\Cargo.toml"),
            ("backslash-separated, single dir", "crates\\Cargo.toml"),
            // A control character does not disqualify a path (see the accepted
            // list); these two are rejected because the LAST SEGMENT is then no
            // longer exactly `Cargo.toml`, so the value cites no manifest.
            (
                "newline in the manifest name",
                "crates/x/Cargo.toml\nforged",
            ),
            // NUL is the exception among control characters: POSIX forbids it
            // in a filename and it delimits the `-z` listings, so the walk can
            // never produce one. Both positions are rejected — the second by
            // the NUL rule ALONE, since its last segment is a valid manifest
            // name and its first carries no drive prefix.
            ("nul in the manifest name", "crates/x/\u{0}Cargo.toml"),
            ("nul in a directory segment", "crates/\u{0}x/Cargo.toml"),
            ("nul alone", "crates/x\u{0}/Cargo.toml"),
            ("not a manifest", "crates/x/src/lib.rs"),
            ("case-shifted", "crates/x/cargo.toml"),
            ("manifest-suffixed name", "crates/x/NotCargo.toml"),
            ("bare directory", "crates/x"),
        ] {
            assert!(
                !manifest_path_is_repo_relative(rejected),
                "`{label}` is not a path the walk produces"
            );
        }
    }

    /// Every manifest path the resolver itself indexes must satisfy the shape
    /// rule the reader enforces — otherwise the reader would reject a value the
    /// producer legitimately wrote, silently dropping real attribution.
    #[test]
    fn every_indexed_manifest_path_satisfies_the_reader_shape_rule() {
        let facts = vec![
            ManifestPackageFact::new(
                "Cargo.toml",
                ManifestParseOutcome::Package {
                    name: "root".to_owned(),
                },
            ),
            ManifestPackageFact::new(
                "crates/alpha/Cargo.toml",
                ManifestParseOutcome::Package {
                    name: "alpha".to_owned(),
                },
            ),
            ManifestPackageFact::new("crates/beta/Cargo.toml", ManifestParseOutcome::Virtual),
        ];
        for fact in &facts {
            assert!(
                manifest_path_is_repo_relative(&fact.manifest_repo_relative_path),
                "indexed `{}` must satisfy the reader's shape rule",
                fact.manifest_repo_relative_path
            );
            assert!(
                fact.directory_key().is_some(),
                "indexed `{}` must be placeable",
                fact.manifest_repo_relative_path
            );
        }
        let index = CrateAttributionIndex::from_facts(facts);
        let attribution = index.attribution_for("crates/alpha/src/lib.rs");
        let (name, manifest) = attribution
            .owning_package()
            .expect("a resolver-produced attribution must survive the reader's checks");
        assert_eq!(name, "alpha");
        assert_eq!(manifest, "crates/alpha/Cargo.toml");
    }

    /// A record path the producer could never emit must not be "enclosed" by
    /// anything.
    ///
    /// `ancestor_dirs` always ends at the repo root, so a root manifest encloses
    /// literally any string — including one that escapes the repository. And a
    /// `..` segment appears as an ordinary ancestor, so an escaping path can
    /// contain the cited directory on its way out. `normalize_path` keeps only
    /// `Component::Normal`, so none of these shapes can be produced.
    #[test]
    fn a_malformed_record_path_is_enclosed_by_nothing() {
        for (label, manifest, record) in [
            (
                "root manifest, escaping record",
                "Cargo.toml",
                "../outside.rs",
            ),
            (
                "escape through the cited directory",
                "crates/a/Cargo.toml",
                "crates/a/../../outside.rs",
            ),
            ("absolute record path", "Cargo.toml", "/etc/passwd"),
            ("empty interior segment", "Cargo.toml", "crates//x.rs"),
            ("dot segment", "crates/a/Cargo.toml", "crates/a/./x.rs"),
            ("empty record path", "Cargo.toml", ""),
            ("nul in the record path", "Cargo.toml", "crates/\u{0}x.rs"),
            (
                "windows drive record path",
                "Cargo.toml",
                "C:/crates/a/x.rs",
            ),
        ] {
            assert!(
                !manifest_encloses(manifest, record),
                "`{label}`: `{record}` is not a path the producer emits"
            );
        }
    }

    /// The record-path check must not reject what the producer really emits.
    #[test]
    fn a_normal_record_path_is_still_enclosed() {
        for (manifest, record) in [
            ("Cargo.toml", "src/lib.rs"),
            ("Cargo.toml", "Cargo.toml"),
            ("crates/a/Cargo.toml", "crates/a/src/lib.rs"),
            ("crates/a/Cargo.toml", "crates/a/Cargo.toml"),
            // Legal Unix filename bytes (issues #489 rounds 12-14).
            ("crates/odd\\dir/Cargo.toml", "crates/odd\\dir/src/lib.rs"),
            ("crates/od\td/Cargo.toml", "crates/od\td/src/lib.rs"),
            ("vendor:patched/Cargo.toml", "vendor:patched/src/lib.rs"),
        ] {
            assert!(
                manifest_encloses(manifest, record),
                "`{manifest}` encloses `{record}`"
            );
        }
        // Segment-aware in the record direction too.
        assert!(!manifest_encloses(
            "crates/alpha/Cargo.toml",
            "crates/alphabet/src/x.rs"
        ));
    }

    #[test]
    fn ancestor_dirs_are_segment_aware_and_root_terminated() {
        assert_eq!(
            ancestor_dirs("crates/foo/src/lib.rs"),
            vec![
                "crates/foo/src".to_owned(),
                "crates/foo".to_owned(),
                "crates".to_owned(),
                String::new(),
            ]
        );
        assert_eq!(ancestor_dirs("lib.rs"), vec![String::new()]);
        assert_eq!(ancestor_dirs(""), vec![String::new()]);
    }

    #[test]
    fn resolver_module_performs_no_io() {
        // The purity contract is what lets the current-tree and Git-object
        // harvests share one rule and provably agree. Pin it against the
        // module's own source text.
        let source = include_str!("crate_attribution.rs");
        // Only inspect code above the test module, which legitimately mentions
        // these tokens in prose.
        let code = source
            .split("\n#[cfg(test)]\nmod tests {")
            .next()
            .expect("module source should split at its test module");
        for forbidden in [
            "std::fs",
            "std::process",
            "Command::new",
            "read_dir",
            "SystemTime",
            "Utc::now",
        ] {
            assert!(
                !code.contains(forbidden),
                "the resolver must stay pure: found `{forbidden}`"
            );
        }
    }
}
