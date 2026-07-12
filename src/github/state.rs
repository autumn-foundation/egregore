//! Idempotency state file (`.github-import-state.json`).
//!
//! Per `docs/schema/import-github.md` §5 the importer persists per-repository
//! `ETags`, update watermarks, a label-list hash, and per-resource content hashes
//! so an unchanged re-import issues only conditional probes and emits zero
//! per-resource records, while a changed re-import emits only the resources that
//! actually changed.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::github::{model, records::CommitIndex};

/// Current idempotency-state schema version.
///
/// Bumped 1 → 2 for issue #333 (Codex P2): the importer began emitting new
/// first-class flat PR `Task` fields (head/base/merge SHAs and refs), but a
/// pre-#333 state file's cached `/pulls` `ETag` would return HTTP 304 and skip the
/// pulls branch, silently suppressing the new contract for unchanged PRs. A
/// version mismatch discards the stale state (see [`State::load_or_fresh`]),
/// forcing exactly ONE full refresh that re-emits the promoted fields; the
/// version-2 state written afterward keeps subsequent unchanged re-imports
/// idempotent (issue #333 AC8). Bump this whenever the emitted per-resource
/// contract changes in a way that a cached conditional probe could hide.
///
/// NOT bumped for the Codex round-6 `pr_merge_artifacts` field: it is
/// `#[serde(default)]`, so a version-2 state file loads with an empty map and is
/// simply treated as "no known prior artifact" (which safely emits no
/// tombstone). A bump would discard every cached `ETag`/hash and force a heavy
/// full refetch for zero benefit — the additive field needs no forced refresh.
///
/// Bumped 2 → 3 for issue #334 (the review-side mirror of #333): the importer
/// began emitting a new first-class `review_commit_sha` `Review` field and
/// `REVIEWS_COMMIT` anchor edges/diagnostics, but a pre-#334 state file's cached
/// review-endpoint `ETags` (`/pulls/comments`, `/pulls/{n}/reviews`) would return
/// HTTP 304 and skip re-emission, silently suppressing the new contract for
/// unchanged reviews.
///
/// Unlike the #333 1 → 2 bump (v1 held no merge artifacts, so a blunt discard was
/// safe), a v2 file DOES carry #333's `pr_merge_artifacts` tracking, so this bump
/// MUST NOT discard the whole file. [`State::load_or_fresh`] therefore *migrates*
/// v2 → v3 (Codex #352 P2/P1). A schema bump is a one-time FULL refresh, so the
/// migration CLEARS EVERY conditional-request `ETag` while preserving every field
/// that provides idempotency and prior-artifact tracking: the per-resource
/// resource hashes, watermarks, the seed-graph fingerprint, and — critically —
/// `pr_merge_artifacts`, so a merge that resolves differently on the first v3 run
/// can still tombstone the stale artifact. The additive `review_commit_artifacts`
/// field is `#[serde(default)]` and defaults to empty.
///
/// Clearing ALL `ETags` (not just the review-endpoint ones) is required, not
/// merely convenient (Codex #352 P1). The per-PR `/pulls/{n}/reviews` fetch in
/// [`crate::github::import::run_import`] is gated behind `if pulls_changed`, which
/// is true only when the `/pulls` LIST endpoint returns 200. Preserving the
/// `/pulls` list `ETag` lets an unchanged PR list return 304 →
/// `pulls_changed == false` → the per-PR review `ETags` (however they were
/// cleared) are never even requested, so existing reviews never re-emit with the
/// #334 anchor. Dropping the `/pulls` list `ETag` too forces the 200 that flips
/// `pulls_changed` true and drives the review refresh. Clearing every other
/// endpoint's `ETag` is safe because record emission is universally gated by the
/// preserved resource hashes: an unchanged issue/PR/label/comment re-fetches (200)
/// but its unchanged hash suppresses re-emission (idempotency, AC8), so a changed
/// seed graph cannot spuriously re-emit `MERGED_AS`. The v2-stored review resource
/// hashes are preserved but harmless: the v3 review change hash folds in the
/// `REVIEWS_COMMIT` marker ([`review_hash`]) — a different formula than the v2
/// `blake3(payload)` — so on the forced 200 refetch no stored review hash can
/// match and every review re-emits with the anchor field exactly once.
///
/// Bumped 3 → 4 for issue #335 (reviewer identity): the importer began minting
/// `ExternalIdentity` nodes and `REVIEWED_BY` / `REQUESTED_REVIEW_FROM` edges
/// for every review author and requested reviewer. Unlike #334, these derive
/// ONLY from GitHub payloads (no seed-graph dependency), and unlike #334 the
/// review change-hash FORMULA is unchanged, so a v3 store's cached review/PR
/// resource hashes would still MATCH on a forced refetch and suppress
/// re-emission — leaving an upgraded store permanently without the new identity
/// facts. A cached endpoint `ETag` (304) hides them even more directly. So the
/// v3 → v4 migration ([`State::load_or_fresh`]) clears EVERY `ETag` AND EVERY
/// per-resource hash, forcing exactly one full refresh that re-emits every
/// issue, PR, and review with its reviewer-identity facts. Everything that
/// provides prior-artifact tracking survives (`pr_merge_artifacts`,
/// `review_commit_artifacts`, watermarks, the seed-graph fingerprint, the label
/// hash) so #333/#334 tombstoning still fires across the upgrade. A pre-#334 v2
/// file migrates the same way: clearing hashes is a superset of #334's
/// ETag-only clear and safe because #334 relied on a hash-formula change v2
/// hashes could not satisfy anyway.
pub const STATE_SCHEMA_VERSION: u32 = 4;

/// Per-endpoint update watermarks (inclusive `>=` selection, §5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Watermarks {
    /// Highest `updated_at` seen across imported issues.
    #[serde(default)]
    pub issues: Option<String>,
    /// Highest `updated_at` seen across imported pull requests.
    #[serde(default)]
    pub pulls: Option<String>,
}

/// On-disk idempotency state, one object per `<owner>/<repo>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// State-file schema version; mismatched files are treated as missing.
    pub schema_version: u32,
    /// `<owner>/<repo>` this state belongs to.
    pub source_repo: String,
    /// API base URL the state was captured against.
    pub api_base_url: String,
    /// Wall-clock of the last completed run (Unix ms).
    pub last_run_at_unix_ms: u128,
    /// `"<endpoint>?page=<n>" -> "<etag>"`.
    #[serde(default)]
    pub etags: BTreeMap<String, String>,
    /// `"<endpoint>" -> next cursor` (reserved; REST uses Link headers).
    #[serde(default)]
    pub cursors: BTreeMap<String, Option<String>>,
    /// Per-endpoint update watermarks.
    #[serde(default)]
    pub last_seen_updated_at: Watermarks,
    /// Hash of the sorted label list (name+color+description).
    #[serde(default)]
    pub label_list_hash: Option<String>,
    /// `"issue:<n>" | "pr:<n>" -> blake3(content hash of key fields)`.
    #[serde(default)]
    pub resource_hashes: BTreeMap<String, String>,
    /// Maps `"pr:<n>"` to the record ID of the last-emitted `MERGED_AS` artifact
    /// (the `MERGED_AS` edge ID on a unique resolution, or the
    /// `github_commit_unresolved` Diagnostic ID otherwise). Issue #333, Codex
    /// round-6.
    ///
    /// The importer is otherwise purely additive: when a PR's merge-resolution
    /// outcome changes on re-import, the new artifact carries a NEW id and the
    /// prior artifact would linger live in a persistent store, so stale and
    /// fresh merge evidence coexist. Persisting the prior artifact's id lets a
    /// changed re-import retract the superseded record via a `Tombstone`
    /// (`deleted_id`) before emitting the current one. The full record id (not a
    /// lossy marker) is stored so a changed `merge_commit_sha` under a
    /// still-unresolved outcome still retracts the diagnostic keyed on the OLD
    /// sha. A `#[serde(default)]` empty map means legacy state loads without a
    /// schema bump (see [`STATE_SCHEMA_VERSION`]): a missing prior is treated as
    /// "no known artifact", which safely emits no tombstone.
    #[serde(default)]
    pub pr_merge_artifacts: BTreeMap<String, String>,
    /// Maps a review resource key (`"pr_review:<n>:<id>"` or
    /// `"pr_review_comment:<id>"`) to the record ID of the last-emitted
    /// `REVIEWS_COMMIT` anchor artifact — the edge ID on a unique resolution, or
    /// the `github_commit_unresolved` / `github_review_unanchored` Diagnostic ID
    /// otherwise (issue #334, the review-side mirror of `pr_merge_artifacts`).
    ///
    /// Persisting the prior artifact id lets a changed re-import retract the
    /// superseded record via a `Tombstone` (`deleted_id`) before emitting the
    /// current one, so a persistent store never surfaces stale + fresh
    /// review-anchor evidence simultaneously. The full record id (not a lossy
    /// marker) is stored so a changed `commit_id` under a still-unresolved
    /// outcome still retracts the diagnostic keyed on the OLD sha. A
    /// `#[serde(default)]` empty map means legacy state loads as "no known
    /// artifact" (safely emits no tombstone).
    #[serde(default)]
    pub review_commit_artifacts: BTreeMap<String, String>,
    /// Maps `"pr:<n>"` to the sorted set of `REQUESTED_REVIEW_FROM` edge record
    /// IDs the PR emitted on the last run — its "prior request set" (issue #335,
    /// Codex P1, the requested-reviewer analog of `pr_merge_artifacts`).
    ///
    /// A PR emits one `REQUESTED_REVIEW_FROM` edge per reviewer currently in its
    /// `requested_reviewers`. That set shrinks whenever a reviewer approves, the
    /// PR merges/closes, or a reviewer is manually removed. The importer is
    /// otherwise purely additive, so without tracking the prior set a removed
    /// reviewer's edge would linger LIVE in a persistent store and downstream
    /// queries would still report the removed reviewer as "requested". Persisting
    /// the prior set lets a changed re-import retract each dropped edge via a
    /// `Tombstone` (`deleted_id == edge_id`) before persisting the new set. Only
    /// the edge is retracted — never the global `ExternalIdentity` node (a login
    /// persists across PRs) and never an immutable `REVIEWED_BY` edge. A
    /// `#[serde(default)]` empty map means legacy state loads without a schema
    /// bump: a missing prior set is "no known edges", which safely emits no
    /// tombstone.
    #[serde(default)]
    pub pr_request_edges: BTreeMap<String, Vec<String>>,
    /// Maps `"pr:<n>"` to the sorted set of `github_team_review_request_unexpanded`
    /// `Diagnostic` record IDs the PR emitted on the last run — its "prior team
    /// set" (issue #335, Codex P2, the requested-team sibling of
    /// `pr_request_edges`).
    ///
    /// A PR emits one `github_team_review_request_unexpanded` diagnostic per team
    /// currently in its `requested_teams`. That set shrinks whenever a team is
    /// removed or replaced. The importer is otherwise purely additive, so without
    /// tracking the prior set a removed team's diagnostic would linger LIVE in a
    /// persistent store and current-state queries would still report the removed
    /// team's review request. Persisting the prior set lets a changed re-import
    /// retract each dropped diagnostic via a `Tombstone`
    /// (`deleted_id == diagnostic_id`) before persisting the new set. Only the
    /// team diagnostic is retracted — never a reviewer edge, an identity node, or
    /// an immutable `REVIEWED_BY` edge. A `#[serde(default)]` empty map means
    /// legacy state loads without a schema bump: a missing prior set is "no known
    /// diagnostics", which safely emits no tombstone.
    #[serde(default)]
    pub pr_team_diagnostics: BTreeMap<String, Vec<String>>,
    /// Fingerprint of the seeded code graph relevant to PR merge-link resolution
    /// (issue #333, Codex round-5). PR merge-link resolution depends on the local
    /// seed graph, which GitHub's `/pulls` `ETag` cannot see; this gates the
    /// `/pulls` conditional request. A pre-fingerprint state file lacks the field
    /// and deserialises to `None` ("unknown"), which never equals any real
    /// fingerprint and so forces exactly one full `/pulls` refetch on the first
    /// upgraded run (fail-safe), after which the real fingerprint is persisted.
    #[serde(default)]
    pub code_graph_fingerprint: Option<String>,
}

impl State {
    /// Builds a fresh empty state for `source_repo` against `api_base_url`.
    #[must_use]
    pub fn fresh(source_repo: &str, api_base_url: &str) -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            source_repo: source_repo.to_owned(),
            api_base_url: api_base_url.to_owned(),
            last_run_at_unix_ms: 0,
            etags: BTreeMap::new(),
            cursors: BTreeMap::new(),
            last_seen_updated_at: Watermarks::default(),
            label_list_hash: None,
            resource_hashes: BTreeMap::new(),
            pr_merge_artifacts: BTreeMap::new(),
            review_commit_artifacts: BTreeMap::new(),
            pr_request_edges: BTreeMap::new(),
            pr_team_diagnostics: BTreeMap::new(),
            code_graph_fingerprint: None,
        }
    }

    /// Loads state from `path`, returning a fresh state when the file is
    /// missing, unreadable, or unparseable (a partial file from a crashed run,
    /// per §5), and migrating older but recoverable schema versions in place.
    ///
    /// The cached state is discarded (fresh-empty) when its `source_repo` or
    /// `api_base_url` does not match the current run: `ETags` and resource
    /// hashes are scoped to one `(api_base, owner/repo)` pair, so reusing the
    /// same `--state-file` across GitHub Enterprise, the default API, or a mock
    /// `--api-base` must start fresh rather than send conditional requests with
    /// another server's `ETags`.
    ///
    /// Schema-version handling (see [`STATE_SCHEMA_VERSION`]):
    /// - `== STATE_SCHEMA_VERSION` → used as-is.
    /// - `== 2` or `== 3` → MIGRATED to v4, not discarded (issue #335, extending
    ///   Codex #352 P2). Discarding would drop #333's `pr_merge_artifacts` and
    ///   #334's `review_commit_artifacts` tracking, so a PR/review whose outcome
    ///   resolves differently on the first v4 run could emit the fresh artifact
    ///   yet never tombstone the stale one. Migration CLEARS EVERY `ETag` AND
    ///   EVERY per-resource hash, forcing a full refresh that re-fetches (200)
    ///   and re-emits every issue, PR, and review with its #335 reviewer-identity
    ///   facts. (The #334 review change-hash formula is unchanged, so a preserved
    ///   hash would MATCH on the refetch and suppress the new edges — hence the
    ///   hashes must be cleared, not preserved.) Everything providing
    ///   prior-artifact tracking survives: `pr_merge_artifacts`,
    ///   `review_commit_artifacts`, watermarks, the seed-graph fingerprint, and
    ///   the label hash.
    /// - anything else (`< 2`, pre-#333 with no artifacts to lose, or an
    ///   unsupported future value) → safe fresh-empty fallback.
    #[must_use]
    pub fn load_or_fresh(path: &Path, source_repo: &str, api_base_url: &str) -> Self {
        let fallback = || Self::fresh(source_repo, api_base_url);
        let Ok(raw) = std::fs::read_to_string(path) else {
            return fallback();
        };
        let Ok(mut s) = serde_json::from_str::<Self>(&raw) else {
            return fallback();
        };
        // `ETags`/hashes are scoped to one `(api_base, owner/repo)` pair; a
        // cross-scope reuse must never send another server's conditional probes.
        if s.source_repo != source_repo || s.api_base_url != api_base_url {
            return fallback();
        }
        if s.schema_version == STATE_SCHEMA_VERSION {
            return s;
        }
        if s.schema_version == 2 || s.schema_version == 3 {
            // Migrate v2/v3 → v4 in place rather than discarding the whole file
            // (issue #335). Discarding would drop #333's `pr_merge_artifacts`
            // and #334's `review_commit_artifacts` tracking, so a PR/review whose
            // outcome resolves differently on the first v4 run could emit the
            // fresh artifact yet never tombstone the stale one. A schema bump is
            // a one-time full refresh: clear EVERY conditional `ETag` AND EVERY
            // per-resource hash, so the first v4 run re-fetches (200) and
            // re-emits every issue, PR, and review with its #335 reviewer-identity
            // facts — the review change-hash formula is unchanged since #334, so
            // preserved hashes would otherwise MATCH and suppress the new edges.
            // Everything providing prior-artifact tracking survives: the merge/
            // review artifact maps, watermarks, the seed-graph fingerprint, and
            // the label hash.
            s.etags.clear();
            s.resource_hashes.clear();
            s.schema_version = STATE_SCHEMA_VERSION;
            return s;
        }
        fallback()
    }

    /// Serialises state to `path` (pretty JSON for operator inspection).
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be written.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(path, json)
    }

    /// Returns `true` when `key`'s stored content hash equals `hash` (i.e. the
    /// resource is unchanged since the last run and must not be re-emitted).
    #[must_use]
    pub fn is_unchanged(&self, key: &str, hash: &str) -> bool {
        self.resource_hashes.get(key).is_some_and(|h| h == hash)
    }

    /// Records `key`'s new content hash.
    pub fn record_hash(&mut self, key: String, hash: String) {
        self.resource_hashes.insert(key, hash);
    }

    /// Returns the record ID of the merge-resolution artifact last emitted for
    /// `key` (`"pr:<n>"`), or `None` when no artifact is tracked (issue #333,
    /// Codex round-6).
    #[must_use]
    pub fn prior_merge_artifact(&self, key: &str) -> Option<&str> {
        self.pr_merge_artifacts.get(key).map(String::as_str)
    }

    /// Records (`Some`) or clears (`None`) the merge-resolution artifact id
    /// currently emitted for `key` (`"pr:<n>"`). `None` means the PR emits no
    /// merge artifact (unmerged, no seed match candidate, or empty SHA), so no
    /// stale record can exist to retract on a later change (issue #333, Codex
    /// round-6).
    pub fn set_merge_artifact(&mut self, key: String, artifact_id: Option<String>) {
        match artifact_id {
            Some(id) => {
                self.pr_merge_artifacts.insert(key, id);
            }
            None => {
                self.pr_merge_artifacts.remove(&key);
            }
        }
    }

    /// Returns the record ID of the review-anchor artifact last emitted for
    /// `key`, or `None` when no artifact is tracked (issue #334).
    #[must_use]
    pub fn prior_review_artifact(&self, key: &str) -> Option<&str> {
        self.review_commit_artifacts.get(key).map(String::as_str)
    }

    /// Records (`Some`) or clears (`None`) the review-anchor artifact id
    /// currently emitted for `key`. `None` means the review emits no anchor
    /// artifact (exempt `issue_comment`, or no seed graph), so no stale record
    /// can exist to retract on a later change (issue #334).
    pub fn set_review_artifact(&mut self, key: String, artifact_id: Option<String>) {
        match artifact_id {
            Some(id) => {
                self.review_commit_artifacts.insert(key, id);
            }
            None => {
                self.review_commit_artifacts.remove(&key);
            }
        }
    }

    /// Returns the `REQUESTED_REVIEW_FROM` edge ids the PR keyed by `key`
    /// (`"pr:<n>"`) emitted last run — its prior request set (issue #335, Codex
    /// P1). An empty slice means no tracked edges (legacy state or a PR that has
    /// never requested a reviewer), which safely emits no tombstone.
    #[must_use]
    pub fn prior_request_edges(&self, key: &str) -> &[String] {
        self.pr_request_edges.get(key).map_or(&[], Vec::as_slice)
    }

    /// Records the current `REQUESTED_REVIEW_FROM` edge ids for `key`
    /// (`"pr:<n>"`). A non-empty set is stored as the new prior set; an EMPTY set
    /// clears the entry (the PR requests no reviewers, so no stale edge can exist
    /// to retract on a later change), keeping the map minimal and deterministic
    /// (issue #335, Codex P1).
    pub fn set_request_edges(&mut self, key: String, edge_ids: Vec<String>) {
        if edge_ids.is_empty() {
            self.pr_request_edges.remove(&key);
        } else {
            self.pr_request_edges.insert(key, edge_ids);
        }
    }

    /// Returns the `github_team_review_request_unexpanded` diagnostic ids the PR
    /// keyed by `key` (`"pr:<n>"`) emitted last run — its prior team set (issue
    /// #335, Codex P2). An empty slice means no tracked diagnostics (legacy state
    /// or a PR that has never requested a team), which safely emits no tombstone.
    #[must_use]
    pub fn prior_team_diagnostics(&self, key: &str) -> &[String] {
        self.pr_team_diagnostics.get(key).map_or(&[], Vec::as_slice)
    }

    /// Records the current `github_team_review_request_unexpanded` diagnostic ids
    /// for `key` (`"pr:<n>"`). A non-empty set is stored as the new prior set; an
    /// EMPTY set clears the entry (the PR requests no teams, so no stale
    /// diagnostic can exist to retract on a later change), keeping the map minimal
    /// and deterministic (issue #335, Codex P2).
    pub fn set_team_diagnostics(&mut self, key: String, diagnostic_ids: Vec<String>) {
        if diagnostic_ids.is_empty() {
            self.pr_team_diagnostics.remove(&key);
        } else {
            self.pr_team_diagnostics.insert(key, diagnostic_ids);
        }
    }
}

/// Computes the content hash of an issue's emission-affecting key fields (§5).
#[must_use]
pub fn issue_hash(issue: &model::Issue) -> String {
    let key = serde_json::json!({
        "number": issue.number,
        "state": issue.state,
        "state_reason": issue.state_reason,
        "title": issue.title,
        "body": issue.body,
        "labels": issue.labels.iter().map(|l| &l.name).collect::<Vec<_>>(),
        "assignees": issue.assignees.iter().map(|u| &u.login).collect::<Vec<_>>(),
        "milestone": issue.milestone.as_ref().map(|m| &m.title),
        "updated_at": issue.updated_at,
        "closed_at": issue.closed_at,
    });
    blake3::hash(serde_json::to_string(&key).unwrap_or_default().as_bytes())
        .to_hex()
        .to_string()
}

/// Computes the content hash of a PR's emission-affecting key fields (§5).
///
/// `merge_link_marker` is the PR's `MERGED_AS` resolution outcome against the
/// current seeded code graph (see [`crate::github::records::merge_resolution_marker`]).
/// It participates in the change hash (issue #333, Codex round-4) because the
/// merge-link output depends on the seed graph while the PR payload does not: a
/// seed graph that newly resolves this PR's `merge_commit_sha` must re-emit the
/// `MERGED_AS` edge even though the payload is unchanged, and an unchanged seed
/// must stay idempotent (AC8). This affects only change detection — never the
/// stable record identity ([`crate::ir::project_stable_id`]).
#[must_use]
pub fn pull_hash(pr: &model::PullRequest, merge_link_marker: &str) -> String {
    let key = serde_json::json!({
        "number": pr.number,
        "state": pr.state,
        "title": pr.title,
        "body": pr.body,
        "labels": pr.labels.iter().map(|l| &l.name).collect::<Vec<_>>(),
        "assignees": pr.assignees.iter().map(|u| &u.login).collect::<Vec<_>>(),
        "milestone": pr.milestone.as_ref().map(|m| &m.title),
        "updated_at": pr.updated_at,
        "closed_at": pr.closed_at,
        "merged_at": pr.merged_at,
        "draft": pr.draft,
        "head_sha": pr.head.as_ref().map(|h| &h.sha),
        // head_ref is now a first-class flat Task field (#333), so a branch
        // rename with every other field unchanged must force re-emission.
        "head_ref": pr.head.as_ref().map(|h| &h.ref_name),
        "base_ref": pr.base.as_ref().map(|b| &b.ref_name),
        // merge_commit_sha is persisted in the task body blob, so it must be in
        // the change hash — GitHub may rewrite it after finalizing a merge while
        // every other field is unchanged.
        "merge_commit_sha": pr.merge_commit_sha,
        // MERGED_AS resolution outcome against the seeded code graph (#333,
        // Codex round-4): a changed seed graph re-emits the merge edge.
        "merge_link": merge_link_marker,
        // Requested reviewers/teams drive REQUESTED_REVIEW_FROM edges and team
        // diagnostics (#335), and the PR author is minted as an ExternalIdentity /
        // Task.author (#335). A reviewer added/removed or an author account rename
        // with every other field unchanged must re-emit the PR's request/author
        // edges, so all three participate in the change hash.
        "requested_reviewers": pr.requested_reviewers.iter().map(|u| &u.login).collect::<Vec<_>>(),
        "requested_teams": pr.requested_teams.iter().map(|t| &t.slug).collect::<Vec<_>>(),
        // The PR author (`pr.user.login`) is minted as an ExternalIdentity and
        // stored as Task.author (#335). An author account rename with every other
        // field unchanged must re-emit the PR so the new author identity is minted
        // and Task.author updated — otherwise the author≠approver segregation-of-
        // duties join keeps the stale identity.
        "author": pr.user.as_ref().map(|u| &u.login),
    });
    blake3::hash(serde_json::to_string(&key).unwrap_or_default().as_bytes())
        .to_hex()
        .to_string()
}

/// Folds a review's `REVIEWS_COMMIT` resolution marker into its raw-payload
/// hash (issue #334, the review-side analog of [`pull_hash`]'s `merge_link`
/// fold).
///
/// `payload_hash` is the review item's raw-JSON content hash; `review_marker`
/// is [`crate::github::records::review_commit_marker`]'s outcome against the
/// seeded code graph. Folding the marker in means a changed seed graph that now
/// resolves (or stops resolving) a review's `commit_id` re-emits the anchor
/// artifact even when the review payload is byte-identical, while an unchanged
/// seed keeps re-imports idempotent.
#[must_use]
pub fn review_hash(payload_hash: &str, review_marker: &str) -> String {
    blake3::hash(format!("{payload_hash}\u{1}{review_marker}").as_bytes())
        .to_hex()
        .to_string()
}

/// Computes the label-list hash (sorted name+color+description), §5.
#[must_use]
pub fn label_list_hash(labels: &[model::Label]) -> String {
    let mut rows: Vec<String> = labels
        .iter()
        .map(|l| {
            format!(
                "{}\u{1}{}\u{1}{}",
                l.name,
                l.color,
                l.description.clone().unwrap_or_default()
            )
        })
        .collect();
    rows.sort();
    blake3::hash(rows.join("\n").as_bytes())
        .to_hex()
        .to_string()
}

/// Computes a stable fingerprint of the seeded code graph relevant to PR
/// merge-link resolution (issue #333, Codex round-5).
///
/// PR `MERGED_AS` resolution matches a PR's `merge_commit_sha` against the
/// `Commit` nodes captured in `commit_index`, so the fingerprint digests the
/// full `commit_sha -> [commit_record_id]` mapping (per-SHA record IDs sorted so
/// an incidental reorder never spuriously flips the fingerprint). GitHub's
/// `/pulls` `ETag` cannot observe this local seed, so [`run_import`] gates the
/// `/pulls` conditional request on this value: a changed fingerprint forces a
/// full 200 refetch that recomputes merge links, while an unchanged fingerprint
/// keeps the 304 fast path (merge links cannot have changed).
///
/// An empty index (no `--code-graph`) is the distinct, stable marker `"none"` so
/// a none→some, some→different, or some→none transition all register as a
/// change. Deterministic and byte-identical across runs for a given seed graph.
///
/// [`run_import`]: crate::github::import::run_import
#[must_use]
pub fn code_graph_fingerprint(commit_index: &CommitIndex) -> String {
    if commit_index.is_empty() {
        return "none".to_owned();
    }
    let mut hasher = blake3::Hasher::new();
    // `commit_index` is a `BTreeMap`, so keys iterate in sorted order already.
    for (sha, ids) in commit_index {
        let mut ids = ids.clone();
        ids.sort();
        hasher.update(sha.as_bytes());
        hasher.update(&[0]);
        for id in &ids {
            hasher.update(id.as_bytes());
            hasher.update(&[0]);
        }
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(n: u64, title: &str) -> model::Issue {
        model::Issue {
            number: n,
            title: title.to_owned(),
            body: None,
            state: "open".to_owned(),
            state_reason: None,
            labels: vec![],
            assignees: vec![],
            user: None,
            milestone: None,
            created_at: String::new(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            closed_at: None,
            html_url: String::new(),
            pull_request: None,
        }
    }

    #[test]
    fn hash_changes_when_title_changes() {
        assert_ne!(issue_hash(&issue(1, "a")), issue_hash(&issue(1, "b")));
        assert_eq!(issue_hash(&issue(1, "a")), issue_hash(&issue(1, "a")));
    }

    #[test]
    fn load_rejects_wrong_schema_version() {
        let dir = std::env::temp_dir().join(format!("egst-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(&path, r#"{"schema_version":99,"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0}"#).unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert!(s.resource_hashes.is_empty());
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_discards_pre_333_state_format_version() {
        // A pre-#333 state file carries state-format version 1 with cached ETags
        // and resource hashes. The upgraded binary (STATE_SCHEMA_VERSION >= 2)
        // must discard it so the next import re-fetches every endpoint and
        // re-emits the newly-promoted flat PR Task fields (issue #333, Codex P2).
        // The literal `1` in the fixture is the pre-#333 state-format version; the
        // current binary's STATE_SCHEMA_VERSION has moved past it, so it is stale.
        let dir = std::env::temp_dir().join(format!("egst-pre333-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"etags":{"/repos/o/r/pulls?state=all&per_page=100?page=1":"\"pulls-333\""},"resource_hashes":{"pr:10":"abc"}}"#,
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert!(
            s.etags.is_empty() && s.resource_hashes.is_empty(),
            "pre-#333 (version 1) state must be discarded so a forced refresh re-emits the new fields"
        );
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_save_load() {
        let dir = std::env::temp_dir().join(format!("egst2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let mut s = State::fresh("o/r", "https://api.github.com");
        s.record_hash("issue:1".to_owned(), "abc".to_owned());
        s.save(&path).unwrap();
        let loaded = State::load_or_fresh(&path, "o/r", "https://api.github.com");
        assert!(loaded.is_unchanged("issue:1", "abc"));
        assert!(!loaded.is_unchanged("issue:1", "def"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_discards_state_from_a_different_api_base() {
        // Reusing the same state file for the same slug but a different API base
        // (e.g. GHE vs github.com vs a mock) must NOT reuse the other server's
        // ETags/hashes.
        let dir = std::env::temp_dir().join(format!("egst3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let mut s = State::fresh("o/r", "https://ghe.example.com/api/v3");
        s.record_hash("issue:1".to_owned(), "abc".to_owned());
        s.save(&path).unwrap();

        // Same slug, different base → fresh (no reuse).
        let other = State::load_or_fresh(&path, "o/r", "https://api.github.com");
        assert!(
            other.resource_hashes.is_empty(),
            "must not reuse cross-base state"
        );
        // Same slug, same base → reused.
        let same = State::load_or_fresh(&path, "o/r", "https://ghe.example.com/api/v3");
        assert!(same.is_unchanged("issue:1", "abc"));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn pull(n: u64) -> model::PullRequest {
        model::PullRequest {
            number: n,
            title: "t".to_owned(),
            body: None,
            state: "closed".to_owned(),
            merged_at: Some("2026-01-01T00:00:00Z".to_owned()),
            draft: false,
            labels: vec![],
            assignees: vec![],
            user: None,
            milestone: None,
            created_at: String::new(),
            updated_at: "2026-01-01T00:00:00Z".to_owned(),
            closed_at: None,
            head: None,
            base: None,
            merge_commit_sha: None,
            requested_reviewers: vec![],
            requested_teams: vec![],
            html_url: String::new(),
        }
    }

    #[test]
    fn code_graph_fingerprint_marks_empty_seed_distinctly_and_is_stable() {
        let empty = CommitIndex::new();
        assert_eq!(
            code_graph_fingerprint(&empty),
            "none",
            "no seed graph is the distinct stable marker"
        );

        let mut a = CommitIndex::new();
        a.insert("sha-aaa".to_owned(), vec!["codegraph:v5:c0".to_owned()]);
        let fp_a = code_graph_fingerprint(&a);
        assert_ne!(fp_a, "none", "a seeded graph is not the empty marker");
        assert_eq!(fp_a, code_graph_fingerprint(&a), "stable across runs");

        // A different SHA set yields a different fingerprint (some→different).
        let mut b = CommitIndex::new();
        b.insert("sha-bbb".to_owned(), vec!["codegraph:v5:c0".to_owned()]);
        assert_ne!(fp_a, code_graph_fingerprint(&b));

        // The SAME SHA resolving to a DIFFERENT record id also changes it, so a
        // re-resolution is caught and forces a `/pulls` refetch.
        let mut c = CommitIndex::new();
        c.insert("sha-aaa".to_owned(), vec!["codegraph:v5:c9".to_owned()]);
        assert_ne!(fp_a, code_graph_fingerprint(&c));
    }

    #[test]
    fn code_graph_fingerprint_ignores_per_sha_id_order() {
        let mut a = CommitIndex::new();
        a.insert("sha".to_owned(), vec!["id-b".to_owned(), "id-a".to_owned()]);
        let mut b = CommitIndex::new();
        b.insert("sha".to_owned(), vec!["id-a".to_owned(), "id-b".to_owned()]);
        assert_eq!(
            code_graph_fingerprint(&a),
            code_graph_fingerprint(&b),
            "per-SHA record id ordering must not affect the fingerprint"
        );
    }

    #[test]
    fn legacy_state_without_fingerprint_loads_as_unknown() {
        // A state file written before the fingerprint field existed (schema
        // version 2, no `code_graph_fingerprint`) must still load rather than
        // panic; the missing field deserialises to `None` ("unknown"), which
        // never equals a real fingerprint and so forces one `/pulls` refetch.
        let dir = std::env::temp_dir().join(format!("egst-legacy-fp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{STATE_SCHEMA_VERSION},"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"etags":{{"/repos/o/r/pulls?state=all&per_page=100?page=1":"\"pulls-1\""}},"resource_hashes":{{"pr:1":"abc"}}}}"#
            ),
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert_eq!(
            s.code_graph_fingerprint, None,
            "missing fingerprint loads as unknown (None), forcing a /pulls refetch"
        );
        // The rest of the version-matched state is preserved (not discarded).
        assert!(s.is_unchanged("pr:1", "abc"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_state_without_pr_merge_artifacts_loads_empty() {
        // A state file written before the round-6 `pr_merge_artifacts` field
        // existed (current schema version, no such key) must still load rather
        // than fail; the missing map deserialises to an empty `BTreeMap`, which
        // reads as "no known prior artifact" and safely emits no tombstone. This
        // is why the field needed no `STATE_SCHEMA_VERSION` bump.
        let dir = std::env::temp_dir().join(format!("egst-legacy-pma-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{STATE_SCHEMA_VERSION},"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"resource_hashes":{{"pr:1":"abc"}}}}"#
            ),
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert!(
            s.pr_merge_artifacts.is_empty(),
            "missing pr_merge_artifacts loads as an empty map"
        );
        assert_eq!(s.prior_merge_artifact("pr:1"), None);
        // The rest of the version-matched state is preserved (not discarded).
        assert!(s.is_unchanged("pr:1", "abc"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_and_read_prior_merge_artifact_roundtrips() {
        let mut s = State::fresh("o/r", "x");
        assert_eq!(s.prior_merge_artifact("pr:7"), None);
        s.set_merge_artifact("pr:7".to_owned(), Some("project:v1:edge-e".to_owned()));
        assert_eq!(s.prior_merge_artifact("pr:7"), Some("project:v1:edge-e"));
        // Clearing (None) removes the entry so a later change sees "no prior".
        s.set_merge_artifact("pr:7".to_owned(), None);
        assert_eq!(s.prior_merge_artifact("pr:7"), None);
    }

    #[test]
    fn pull_hash_changes_when_merge_commit_sha_changes() {
        let mut a = pull(1);
        let mut b = pull(1);
        a.merge_commit_sha = Some("aaaa".to_owned());
        b.merge_commit_sha = Some("bbbb".to_owned());
        assert_ne!(
            pull_hash(&a, "none"),
            pull_hash(&b, "none"),
            "merge_commit_sha must affect the change hash"
        );
    }

    #[test]
    fn pull_hash_changes_when_merge_link_marker_changes() {
        // Issue #333, Codex round-4: an unchanged PR payload against a seed graph
        // that newly resolves its merge_commit_sha must produce a different change
        // hash so the MERGED_AS edge is re-emitted; an unchanged marker stays
        // idempotent.
        let p = pull(1);
        let unseeded = pull_hash(&p, "none");
        let resolved = pull_hash(&p, "resolved:codegraph:v5:commit-0");
        assert_ne!(
            unseeded, resolved,
            "a changed merge-link resolution outcome must change the hash"
        );
        assert_eq!(
            resolved,
            pull_hash(&p, "resolved:codegraph:v5:commit-0"),
            "an unchanged marker keeps the hash stable (AC8)"
        );
    }

    #[test]
    fn review_hash_changes_when_review_marker_changes() {
        // Issue #334: an unchanged review payload against a seed graph that newly
        // resolves its commit_id must produce a different change hash so the
        // REVIEWS_COMMIT anchor is re-emitted; an unchanged marker stays stable.
        let payload = "payload-hash-abc";
        let unseeded = review_hash(payload, "none");
        let resolved = review_hash(payload, "resolved:codegraph:v5:commit-a");
        assert_ne!(
            unseeded, resolved,
            "a changed review-anchor resolution outcome must change the hash"
        );
        assert_eq!(
            resolved,
            review_hash(payload, "resolved:codegraph:v5:commit-a"),
            "an unchanged marker keeps the hash stable (idempotency)"
        );
    }

    #[test]
    fn set_and_read_prior_review_artifact_roundtrips() {
        let mut s = State::fresh("o/r", "x");
        assert_eq!(s.prior_review_artifact("pr_review:3:7"), None);
        s.set_review_artifact(
            "pr_review:3:7".to_owned(),
            Some("project:v1:edge-r".to_owned()),
        );
        assert_eq!(
            s.prior_review_artifact("pr_review:3:7"),
            Some("project:v1:edge-r")
        );
        s.set_review_artifact("pr_review:3:7".to_owned(), None);
        assert_eq!(s.prior_review_artifact("pr_review:3:7"), None);
    }

    #[test]
    fn request_edges_round_trip_and_empty_set_clears_entry() {
        let mut s = State::fresh("o/r", "x");
        assert!(s.prior_request_edges("pr:1").is_empty());
        s.set_request_edges(
            "pr:1".to_owned(),
            vec![
                "project:v1:edge-a".to_owned(),
                "project:v1:edge-b".to_owned(),
            ],
        );
        assert_eq!(
            s.prior_request_edges("pr:1"),
            [
                "project:v1:edge-a".to_owned(),
                "project:v1:edge-b".to_owned()
            ]
        );
        // An empty set clears the entry (no stale edge to retract later).
        s.set_request_edges("pr:1".to_owned(), Vec::new());
        assert!(s.prior_request_edges("pr:1").is_empty());
        assert!(!s.pr_request_edges.contains_key("pr:1"));
    }

    #[test]
    fn legacy_state_without_pr_request_edges_loads_empty() {
        // A version-4 state file lacking `pr_request_edges` (the additive
        // #[serde(default)] field) must load with an empty map rather than fail.
        let dir = std::env::temp_dir().join(format!("egst-legacy-pre-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{STATE_SCHEMA_VERSION},"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"resource_hashes":{{"pr:1":"abc"}}}}"#
            ),
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert!(
            s.pr_request_edges.is_empty(),
            "missing pr_request_edges loads as an empty map"
        );
        assert!(s.prior_request_edges("pr:1").is_empty());
        assert!(s.is_unchanged("pr:1", "abc"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_old_state_retains_pr_request_edges() {
        // Issue #335 (Codex P1): a v2/v3 store carrying a populated
        // `pr_request_edges` map must retain it under v4 so a reviewer removed on
        // the first v4 run is still tombstoned against the prior set.
        let dir = std::env::temp_dir().join(format!("egst-mig-pre-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version":3,"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"pr_request_edges":{"pr:1":["project:v1:edge-erin","project:v1:edge-dave"]}}"#,
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(
            s.prior_request_edges("pr:1"),
            [
                "project:v1:edge-erin".to_owned(),
                "project:v1:edge-dave".to_owned()
            ],
            "migration must retain pr_request_edges so removed reviewers can be tombstoned"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn team_diagnostics_round_trip_and_empty_set_clears_entry() {
        let mut s = State::fresh("o/r", "x");
        assert!(s.prior_team_diagnostics("pr:1").is_empty());
        s.set_team_diagnostics(
            "pr:1".to_owned(),
            vec![
                "project:v1:diag-a".to_owned(),
                "project:v1:diag-b".to_owned(),
            ],
        );
        assert_eq!(
            s.prior_team_diagnostics("pr:1"),
            [
                "project:v1:diag-a".to_owned(),
                "project:v1:diag-b".to_owned()
            ]
        );
        // An empty set clears the entry (no stale diagnostic to retract later).
        s.set_team_diagnostics("pr:1".to_owned(), Vec::new());
        assert!(s.prior_team_diagnostics("pr:1").is_empty());
        assert!(!s.pr_team_diagnostics.contains_key("pr:1"));
    }

    #[test]
    fn legacy_state_without_pr_team_diagnostics_loads_empty() {
        // A version-4 state file lacking `pr_team_diagnostics` (the additive
        // #[serde(default)] field) must load with an empty map rather than fail.
        let dir = std::env::temp_dir().join(format!("egst-legacy-team-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{STATE_SCHEMA_VERSION},"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"resource_hashes":{{"pr:1":"abc"}}}}"#
            ),
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert!(
            s.pr_team_diagnostics.is_empty(),
            "missing pr_team_diagnostics loads as an empty map"
        );
        assert!(s.prior_team_diagnostics("pr:1").is_empty());
        assert!(s.is_unchanged("pr:1", "abc"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_old_state_retains_pr_team_diagnostics() {
        // Issue #335 (Codex P2): a v2/v3 store carrying a populated
        // `pr_team_diagnostics` map must retain it under v4 so a team removed on
        // the first v4 run is still tombstoned against the prior set.
        let dir = std::env::temp_dir().join(format!("egst-mig-team-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version":3,"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"pr_team_diagnostics":{"pr:1":["project:v1:diag-backend","project:v1:diag-frontend"]}}"#,
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(
            s.prior_team_diagnostics("pr:1"),
            [
                "project:v1:diag-backend".to_owned(),
                "project:v1:diag-frontend".to_owned()
            ],
            "migration must retain pr_team_diagnostics so removed teams can be tombstoned"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_state_without_review_commit_artifacts_loads_empty() {
        // A version-3 state file lacking `review_commit_artifacts` (the additive
        // #[serde(default)] field) must load with an empty map rather than fail.
        let dir = std::env::temp_dir().join(format!("egst-legacy-rca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"schema_version":{STATE_SCHEMA_VERSION},"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"resource_hashes":{{"pr_review:3:7":"abc"}}}}"#
            ),
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert!(
            s.review_commit_artifacts.is_empty(),
            "missing review_commit_artifacts loads as an empty map"
        );
        assert_eq!(s.prior_review_artifact("pr_review:3:7"), None);
        assert!(s.is_unchanged("pr_review:3:7", "abc"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_old_state_clears_resource_hashes_for_full_reemit() {
        // Issue #335: a pre-#335 (version-2 or version-3) state file must be
        // MIGRATED, not discarded — discarding drops the merge/review artifact
        // tracking. But because #335's reviewer-identity edges re-use the
        // unchanged #334 review change-hash formula, a preserved resource hash
        // would MATCH on the forced refetch and suppress the new edges, so the
        // migration CLEARS every per-resource hash to force a one-time full
        // re-emit of every issue, PR, and review.
        for prior_version in [2, 3] {
            let dir = std::env::temp_dir().join(format!(
                "egst-mig335-{prior_version}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("state.json");
            std::fs::write(
                &path,
                format!(
                    r#"{{"schema_version":{prior_version},"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"resource_hashes":{{"pr_review:3:7":"abc","pr:10":"deadbeef"}}}}"#
                ),
            )
            .unwrap();
            let s = State::load_or_fresh(&path, "o/r", "x");
            assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
            // Resource hashes are CLEARED so every resource re-emits its #335 facts.
            assert!(
                s.resource_hashes.is_empty(),
                "v{prior_version}→v4 migration must clear resource hashes to force a full re-emit"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn migrate_old_state_retains_pr_merge_artifacts() {
        // Issue #335: a v2/v3 store with a populated `pr_merge_artifacts` map (a
        // PR already carrying a prior MERGED_AS/unresolved artifact id) must
        // retain that map under v4, so `prior_merge_artifact` is still available
        // and the merge-retraction/tombstone path still fires on the first v4 run.
        let dir = std::env::temp_dir().join(format!("egst-mig-pma-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version":3,"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"pr_merge_artifacts":{"pr:10":"project:v1:merged-edge-10","pr:11":"project:v1:unresolved-diag-11"}}"#,
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        assert_eq!(
            s.prior_merge_artifact("pr:10"),
            Some("project:v1:merged-edge-10"),
            "migration must retain pr_merge_artifacts so the stale artifact can be tombstoned"
        );
        assert_eq!(
            s.prior_merge_artifact("pr:11"),
            Some("project:v1:unresolved-diag-11")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn migrate_old_state_clears_etags_and_hashes_but_preserves_watermarks_and_artifacts() {
        // Issue #335: the v2/v3 → v4 migration must clear EVERY ETag AND EVERY
        // per-resource hash, so the first v4 run is a full refresh that both
        // re-fetches (no 304) and re-emits (no unchanged-hash suppression) every
        // resource with its reviewer-identity facts. Everything providing
        // prior-artifact tracking survives: watermarks, artifact maps, the
        // fingerprint, and the label hash.
        let dir = std::env::temp_dir().join(format!("egst-mig-etag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let pulls_list = "/repos/o/r/pulls?state=all&per_page=100?page=1";
        let per_pr_reviews = "/repos/o/r/pulls/12/reviews?per_page=100?page=1";
        let json = serde_json::json!({
            "schema_version": 3,
            "source_repo": "o/r",
            "api_base_url": "x",
            "last_run_at_unix_ms": 0,
            "etags": {
                pulls_list: "\"pulls-list\"",
                per_pr_reviews: "\"reviews-12\"",
            },
            "last_seen_updated_at": { "issues": "2026-01-03T00:00:00Z", "pulls": "2026-01-04T00:00:00Z" },
            "label_list_hash": "labhash",
            "resource_hashes": { "pr:12": "prhash", "issue:1": "ihash" },
            "pr_merge_artifacts": { "pr:12": "project:v1:merged-edge-12" },
            "review_commit_artifacts": { "pr_review:12:5": "project:v1:review-edge-5" },
            "code_graph_fingerprint": "fp-abc"
        });
        std::fs::write(&path, serde_json::to_string(&json).unwrap()).unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        // EVERY ETag AND EVERY resource hash is cleared.
        assert!(
            s.etags.is_empty(),
            "migration must clear every ETag: {:?}",
            s.etags
        );
        assert!(
            s.resource_hashes.is_empty(),
            "migration must clear every resource hash: {:?}",
            s.resource_hashes
        );
        // Everything providing prior-artifact tracking survives.
        assert_eq!(s.label_list_hash.as_deref(), Some("labhash"));
        assert_eq!(
            s.last_seen_updated_at.issues.as_deref(),
            Some("2026-01-03T00:00:00Z")
        );
        assert_eq!(
            s.prior_merge_artifact("pr:12"),
            Some("project:v1:merged-edge-12"),
            "pr_merge_artifacts must survive so a changed merge outcome can tombstone the stale artifact"
        );
        assert_eq!(
            s.prior_review_artifact("pr_review:12:5"),
            Some("project:v1:review-edge-5"),
            "review_commit_artifacts must survive the migration"
        );
        assert_eq!(
            s.code_graph_fingerprint.as_deref(),
            Some("fp-abc"),
            "the seed-graph fingerprint survives the migration"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pull_hash_changes_when_requested_reviewers_change() {
        // Issue #335: a reviewer added/removed with every other field unchanged
        // must change the PR change hash so REQUESTED_REVIEW_FROM edges re-emit.
        let mut a = pull(1);
        let mut b = pull(1);
        a.requested_reviewers = vec![model::User {
            login: "alice".to_owned(),
        }];
        b.requested_reviewers = vec![
            model::User {
                login: "alice".to_owned(),
            },
            model::User {
                login: "bob".to_owned(),
            },
        ];
        assert_ne!(
            pull_hash(&a, "none"),
            pull_hash(&b, "none"),
            "a changed requested-reviewer set must change the hash"
        );
        // Stable when unchanged.
        assert_eq!(
            pull_hash(&a, "none"),
            pull_hash(&pull_with_reviewer(1, "alice"), "none")
        );
    }

    fn pull_with_reviewer(n: u64, login: &str) -> model::PullRequest {
        let mut p = pull(n);
        p.requested_reviewers = vec![model::User {
            login: login.to_owned(),
        }];
        p
    }

    #[test]
    fn pull_hash_changes_when_requested_teams_change() {
        // Issue #335: a requested team added/removed must change the hash so the
        // team diagnostic re-emits.
        let mut a = pull(1);
        let mut b = pull(1);
        a.requested_teams = vec![];
        b.requested_teams = vec![model::Team {
            slug: "backend".to_owned(),
        }];
        assert_ne!(pull_hash(&a, "none"), pull_hash(&b, "none"));
    }

    #[test]
    fn pull_hash_changes_when_author_login_changes() {
        // Issue #335: the PR author (`pr.user.login`) is minted as an
        // ExternalIdentity and stored as Task.author. If the author renames their
        // GitHub account while every other hashed field is unchanged, the PR must
        // re-emit so the new author identity is minted and Task.author updated;
        // otherwise `is_unchanged` suppresses the update and the author≠approver
        // segregation-of-duties join keeps the stale identity.
        let mut a = pull(1);
        let mut b = pull(1);
        a.user = Some(model::User {
            login: "old-login".to_owned(),
        });
        b.user = Some(model::User {
            login: "new-login".to_owned(),
        });
        assert_ne!(
            pull_hash(&a, "none"),
            pull_hash(&b, "none"),
            "a changed PR author login must change the hash"
        );
        // Stable when the author login is unchanged.
        assert_eq!(
            pull_hash(&a, "none"),
            pull_hash(
                &{
                    let mut p = pull(1);
                    p.user = Some(model::User {
                        login: "old-login".to_owned(),
                    });
                    p
                },
                "none"
            )
        );
    }

    #[test]
    fn load_discards_pre_333_version_1_state() {
        // A pre-#333 (version-1) state carried no merge artifacts, so discarding it
        // is still safe and forces the #333 full refresh (unchanged behavior).
        let dir = std::env::temp_dir().join(format!("egst-v1-disc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        std::fs::write(
            &path,
            r#"{"schema_version":1,"source_repo":"o/r","api_base_url":"x","last_run_at_unix_ms":0,"resource_hashes":{"pr:10":"abc"}}"#,
        )
        .unwrap();
        let s = State::load_or_fresh(&path, "o/r", "x");
        assert_eq!(s.schema_version, STATE_SCHEMA_VERSION);
        assert!(
            !s.is_unchanged("pr:10", "abc"),
            "pre-#333 version-1 state is still discarded (no merge artifacts to lose)"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
