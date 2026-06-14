//! Read-only store freshness classification (issue #82).
//!
//! A `scan`/`ingest` store is a snapshot of one working-tree state. Each store
//! stamps a [`SourceSnapshotPayload`](crate::ir::SourceSnapshotPayload) on its
//! `Repository` node recording the HEAD commit and dirty flag it was built from.
//! This module compares that stored snapshot against the *current* working tree
//! and classifies the store as `fresh`, `stale_head`, `stale_dirty`, or
//! `unknown` so an agent never cites file/span handles the live code has already
//! invalidated.
//!
//! The classification is a pure, deterministic function of the stored snapshot
//! and the current working-tree state; the probing of the working tree
//! (`crate::identity::working_tree_snapshot`) and the loading of the store are
//! strictly read-only.
//!
//! Documented in `docs/cli/freshness.md` and `docs/schema/source-snapshot.md`.

use crate::ir::{GraphRecord, NodeKind, SnapshotHead, SourceSnapshotPayload};

/// Freshness of a store relative to the current working tree.
///
/// Codes are stable and machine-readable; see `docs/cli/freshness.md`.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Freshness {
    /// Stored commit equals the current HEAD and the tree is clean.
    Fresh,
    /// The current HEAD differs from the stored commit.
    StaleHead,
    /// HEAD matches but the tree carries uncommitted changes relative to the
    /// stored snapshot (either the live tree is dirty now, or the store itself
    /// was built from a dirty tree that cannot be reproduced).
    StaleDirty,
    /// The store predates snapshot stamping, or no Git context exists on either
    /// the stored side or the current working tree.
    Unknown,
}

impl Freshness {
    /// Returns the stable machine-readable code for this freshness state.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::StaleHead => "stale_head",
            Self::StaleDirty => "stale_dirty",
            Self::Unknown => "unknown",
        }
    }

    /// Returns `true` only for [`Freshness::Fresh`].
    #[must_use]
    pub const fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }
}

/// Classifies store freshness from the stored snapshot and the current working
/// tree's head + dirty state.
///
/// Precedence:
/// 1. No stored snapshot, or either side lacks a committed Git head → `unknown`.
/// 2. Stored commit differs from the current HEAD → `stale_head`.
/// 3. HEAD matches but the live tree is dirty, or the store was built from a
///    dirty tree → `stale_dirty`.
/// 4. Otherwise → `fresh`.
#[must_use]
pub fn classify(
    stored: Option<&SourceSnapshotPayload>,
    current_head: &SnapshotHead,
    current_dirty: bool,
) -> Freshness {
    let Some(stored) = stored else {
        return Freshness::Unknown;
    };
    match (&stored.head, current_head) {
        (SnapshotHead::Commit { sha: stored_sha }, SnapshotHead::Commit { sha: current_sha }) => {
            if stored_sha != current_sha {
                Freshness::StaleHead
            } else if current_dirty || stored.dirty {
                Freshness::StaleDirty
            } else {
                Freshness::Fresh
            }
        }
        // A `no_git` / `unborn_head` head on either side cannot be commit-compared.
        _ => Freshness::Unknown,
    }
}

/// Finds the source snapshot a store recorded for the given repository ID.
///
/// Prefers the snapshot stamped on the `Repository` node whose stable ID matches
/// `repository_id`. As a fallback it returns the sole repository's snapshot, but
/// **only when the store contains exactly one `Repository` node** — never in a
/// multi-repository store, where returning an unrelated repository's snapshot
/// would misclassify a legacy/pre-stamping target against an unrelated checkout.
/// Returns `None` when the matched (or sole) repository carries no snapshot
/// (pre-stamping stores).
#[must_use]
pub fn stored_snapshot<'a>(
    records: &'a [GraphRecord],
    repository_id: &str,
) -> Option<&'a SourceSnapshotPayload> {
    let mut repository_nodes = 0usize;
    let mut matched: Option<&'a SourceSnapshotPayload> = None;
    let mut sole: Option<&'a SourceSnapshotPayload> = None;
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot,
            ..
        } = record
        {
            repository_nodes += 1;
            let snapshot = source_snapshot.as_deref();
            if id == repository_id {
                matched = snapshot;
            }
            sole = snapshot;
        }
    }
    // Exact identity match wins. Otherwise fall back to the sole repository's
    // snapshot only when the store holds exactly one Repository node.
    matched.or_else(|| (repository_nodes == 1).then_some(sole).flatten())
}

#[cfg(test)]
mod tests {
    use super::{Freshness, classify, stored_snapshot};
    use crate::ir::{GraphRecord, NodeKind, SnapshotHead, SourceSnapshotPayload};

    fn snapshot(head: SnapshotHead, dirty: bool) -> SourceSnapshotPayload {
        SourceSnapshotPayload {
            head,
            dirty,
            repository_id: "codegraph:v3:repo".to_owned(),
            scanned_at: "2026-05-19T00:00:00Z".to_owned(),
        }
    }

    fn commit(sha: &str) -> SnapshotHead {
        SnapshotHead::Commit {
            sha: sha.to_owned(),
        }
    }

    #[test]
    fn clean_tree_at_head_is_fresh() {
        let stored = snapshot(commit("abc"), false);
        assert_eq!(
            classify(Some(&stored), &commit("abc"), false),
            Freshness::Fresh
        );
    }

    #[test]
    fn moved_head_is_stale_head() {
        let stored = snapshot(commit("abc"), false);
        assert_eq!(
            classify(Some(&stored), &commit("def"), false),
            Freshness::StaleHead
        );
    }

    #[test]
    fn dirty_tree_at_head_is_stale_dirty() {
        let stored = snapshot(commit("abc"), false);
        assert_eq!(
            classify(Some(&stored), &commit("abc"), true),
            Freshness::StaleDirty
        );
    }

    #[test]
    fn store_built_from_dirty_tree_is_never_fresh() {
        let stored = snapshot(commit("abc"), true);
        assert_eq!(
            classify(Some(&stored), &commit("abc"), false),
            Freshness::StaleDirty
        );
    }

    #[test]
    fn missing_snapshot_is_unknown() {
        assert_eq!(classify(None, &commit("abc"), false), Freshness::Unknown);
    }

    #[test]
    fn no_git_on_either_side_is_unknown() {
        let stored = snapshot(SnapshotHead::NoGit, false);
        assert_eq!(
            classify(Some(&stored), &commit("abc"), false),
            Freshness::Unknown
        );
        let stored = snapshot(commit("abc"), false);
        assert_eq!(
            classify(Some(&stored), &SnapshotHead::NoGit, false),
            Freshness::Unknown
        );
    }

    #[test]
    fn unborn_head_is_unknown() {
        let stored = snapshot(SnapshotHead::UnbornHead, false);
        assert_eq!(
            classify(Some(&stored), &SnapshotHead::UnbornHead, false),
            Freshness::Unknown
        );
    }

    #[test]
    fn codes_are_stable() {
        assert_eq!(Freshness::Fresh.code(), "fresh");
        assert_eq!(Freshness::StaleHead.code(), "stale_head");
        assert_eq!(Freshness::StaleDirty.code(), "stale_dirty");
        assert_eq!(Freshness::Unknown.code(), "unknown");
    }

    fn repo_node(id: &str, snapshot: Option<SourceSnapshotPayload>) -> GraphRecord {
        let node = GraphRecord::node(
            id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(id.to_owned()),
            format!("Repository {id}"),
        );
        match snapshot {
            Some(s) => node.with_source_snapshot(s),
            None => node,
        }
    }

    #[test]
    fn stored_snapshot_matches_by_repository_id() {
        let records = vec![
            repo_node("repo-a", Some(snapshot(commit("aaa"), false))),
            repo_node("repo-b", Some(snapshot(commit("bbb"), false))),
        ];
        let found = stored_snapshot(&records, "repo-b").expect("repo-b snapshot");
        assert_eq!(found.head, commit("bbb"));
    }

    #[test]
    fn stored_snapshot_no_cross_repo_fallback_in_multi_repo_store() {
        // Requested repo is a legacy/pre-stamping node (no snapshot); another repo
        // has one. The unrelated snapshot must NOT be returned.
        let records = vec![
            repo_node("legacy", None),
            repo_node("other", Some(snapshot(commit("ccc"), false))),
        ];
        assert!(stored_snapshot(&records, "legacy").is_none());
    }

    #[test]
    fn stored_snapshot_single_repo_fallback_allows_id_mismatch() {
        // Exactly one Repository node: fall back to its snapshot even if the
        // requested id differs (e.g. identity recomputed differently).
        let records = vec![repo_node("only", Some(snapshot(commit("ddd"), false)))];
        let found = stored_snapshot(&records, "different-id").expect("sole snapshot");
        assert_eq!(found.head, commit("ddd"));
    }

    #[test]
    fn stored_snapshot_single_pre_stamping_repo_is_none() {
        let records = vec![repo_node("only", None)];
        assert!(stored_snapshot(&records, "only").is_none());
    }
}
