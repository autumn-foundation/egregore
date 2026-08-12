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
    /// Parsed with no `[package]` table — a virtual workspace root. It declares
    /// no package, so it owns nothing.
    Virtual,
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
    /// path (absolute, `..`-bearing, empty, or backslash-separated) — such a
    /// fact is dropped rather than relocated across a manifest boundary.
    fn directory_key(&self) -> Option<String> {
        let path = self.manifest_repo_relative_path.as_str();
        if path.is_empty()
            || path.starts_with('/')
            || path.contains('\\')
            || path
                .split('/')
                .any(|segment| segment == ".." || segment == ".")
        {
            return None;
        }
        let mut segments: Vec<&str> = path.split('/').collect();
        segments.pop()?;
        Some(segments.join("/"))
    }
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
    /// Facts are sorted by manifest path first, so when two facts claim the
    /// same directory the lexicographically smallest manifest path wins
    /// deterministically. Facts whose path is not a usable repo-relative
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
        | NodeKind::DependencyDeclaration => true,
        // Repository-scoped code-graph records: they describe the repository or
        // its history, not a file, so no package can own them.
        NodeKind::Repository
        | NodeKind::Commit
        | NodeKind::Change
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
    for record in records {
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
    fn duplicate_dir_facts_resolve_to_lexicographically_smallest_manifest() {
        // Two facts claiming the same directory (only reachable via a caller
        // bug or a case-insensitive filesystem); the winner must be stable.
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
