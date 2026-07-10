//! Idempotency state file (`.github-import-state.json`).
//!
//! Per `docs/schema/import-github.md` §5 the importer persists per-repository
//! `ETags`, update watermarks, a label-list hash, and per-resource content hashes
//! so an unchanged re-import issues only conditional probes and emits zero
//! per-resource records, while a changed re-import emits only the resources that
//! actually changed.

use std::{collections::BTreeMap, path::Path};

use serde::{Deserialize, Serialize};

use crate::github::model;

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
pub const STATE_SCHEMA_VERSION: u32 = 2;

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
        }
    }

    /// Loads state from `path`, returning a fresh state when the file is
    /// missing, unreadable, unparseable, or carries an unsupported
    /// `schema_version` (a partial file from a crashed run, per §5).
    ///
    /// The cached state is also discarded when its `source_repo` or
    /// `api_base_url` does not match the current run: `ETags` and resource
    /// hashes are scoped to one `(api_base, owner/repo)` pair, so reusing the
    /// same `--state-file` across GitHub Enterprise, the default API, or a mock
    /// `--api-base` must start fresh rather than send conditional requests with
    /// another server's `ETags`.
    #[must_use]
    pub fn load_or_fresh(path: &Path, source_repo: &str, api_base_url: &str) -> Self {
        let fallback = || Self::fresh(source_repo, api_base_url);
        let Ok(raw) = std::fs::read_to_string(path) else {
            return fallback();
        };
        match serde_json::from_str::<Self>(&raw) {
            Ok(s)
                if s.schema_version == STATE_SCHEMA_VERSION
                    && s.source_repo == source_repo
                    && s.api_base_url == api_base_url =>
            {
                s
            }
            _ => fallback(),
        }
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
    });
    blake3::hash(serde_json::to_string(&key).unwrap_or_default().as_bytes())
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
            html_url: String::new(),
        }
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
}
