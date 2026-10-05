use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::liveness::Liveness;
use crate::ir::{
    EdgeLabel, GraphRecord, IdentitySource, NodeKind, RepositoryIdentityPayload, SnapshotHead,
    SourceSnapshotPayload, parse_codegraph_id,
};

/// Why a repository selector failed to resolve.
///
/// Both variants carry stable machine-readable codes so callers can emit them
/// verbatim as diagnostics: `unknown_repository_selector` and
/// `ambiguous_repository_selector`. Ambiguity is never resolved implicitly.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RepositorySelectorError {
    /// No repository in the store matches the selector.
    Unknown {
        /// The selector as supplied by the caller.
        selector: String,
    },
    /// More than one repository matches the selector.
    Ambiguous {
        /// The selector as supplied by the caller.
        selector: String,
        /// Stable repository record IDs of every match, sorted ascending.
        candidates: Vec<String>,
    },
}

impl RepositorySelectorError {
    /// Returns the stable machine-readable diagnostic code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unknown { .. } => "unknown_repository_selector",
            Self::Ambiguous { .. } => "ambiguous_repository_selector",
        }
    }
}

/// One repository known to a [`RepositoryIndex`].
#[derive(Debug, Clone, Eq, PartialEq)]
struct RepositoryEntry {
    /// Human-usable display handle (e.g. `owner/name` for remote-derived
    /// identities, the basename otherwise).
    display: String,
    /// Every identity-payload-derived handle this repository answers to.
    selectors: BTreeSet<String>,
    /// The identity payload from the latest `Repository` node write (issue
    /// #193): carries `identity_source` for the catalog.
    identity: Option<RepositoryIdentityPayload>,
    /// The source snapshot from the latest `Repository` node write (issue
    /// #193): carries the HEAD commit handle for the catalog recency hint.
    source_snapshot: Option<SourceSnapshotPayload>,
    /// The `ingested_at` timestamp from the latest `Repository` node write
    /// (issue #193): the recency-hint fallback when no commit handle exists.
    ingested_at: Option<String>,
}

/// What kind of recency hint a [`RepositoryCatalogEntry`] carries (issue #193).
#[derive(Debug, Clone, Copy, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecencyKind {
    /// The repository node's source snapshot recorded a HEAD commit SHA at
    /// scan time.
    Commit,
    /// No commit handle is recorded; the hint is the node's `ingested_at`
    /// timestamp instead.
    IngestTimestamp,
}

impl RecencyKind {
    /// Closed snake_case vocabulary for operator-facing output.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::IngestTimestamp => "ingest_timestamp",
        }
    }
}

/// One row of the repository catalog (issue #193): everything an agent or
/// operator needs to scope a #67 repository-scoped query without guessing.
///
/// Entries are ordered by ascending stable repository record ID, carry no
/// timestamps minted at catalog time, and serialize deterministically, so
/// re-running the catalog against an unchanged store is byte-identical.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct RepositoryCatalogEntry {
    /// Stable repository record ID. Always accepted verbatim by
    /// [`RepositoryIndex::resolve_selector`].
    pub repository_id: String,
    /// How the stable ID was derived (`remote` | `local_root_commit` |
    /// `local_path` | `operator_override`); `None` for legacy repository
    /// nodes that carry no identity payload.
    pub identity_source: Option<IdentitySource>,
    /// The #67-accepted scope selector, verbatim: the human-usable handle
    /// (e.g. remote `owner/name` or the operator override), or the stable
    /// record ID itself when the human handle collides with another
    /// repository's (see `selector_collision`).
    pub selector: String,
    /// `true` when the human-usable handle is shared with another repository.
    /// The emitted `selector` is then the distinct stable record ID and
    /// `colliding_selector` names the shared handle, so the collision is
    /// surfaced rather than silently de-duplicated.
    pub selector_collision: bool,
    /// The shared human-usable handle; present only when `selector_collision`.
    pub colliding_selector: Option<String>,
    /// Number of node records attributed to this repository (all domains;
    /// includes the repository node itself).
    pub node_count: usize,
    /// What the recency hint is: a HEAD commit SHA or an ingest timestamp.
    pub recency_kind: Option<RecencyKind>,
    /// The commit SHA or RFC 3339 ingest timestamp; `None` when neither is
    /// recorded on the repository node.
    pub recency_handle: Option<String>,
}

/// Maps code-graph records to their owning repository and resolves
/// human-usable repository selectors.
///
/// Ownership follows the deterministic containment topology emitted by the
/// scanner: `Repository` —CONTAINS→ `File` —DEFINES/CONTAINS/IMPORTS→ nested
/// modules, imports, and symbols. `SemanticDrift` nodes are attributed to the
/// repository of their `DRIFTS_FROM` target (falling back to the metadata's
/// `target_record_id`/`prior_record_id`).
///
/// Records that cannot be attributed (e.g. legacy fixtures without a
/// `Repository` node) simply have no owner; callers must keep that visible
/// rather than guessing.
#[derive(Debug, Default)]
pub struct RepositoryIndex {
    /// Node record ID → owning repository record ID.
    owner: BTreeMap<String, String>,
    /// Repository record ID → identity handles.
    repos: BTreeMap<String, RepositoryEntry>,
    /// Repository record ID → highest-version repository record ID.
    highest_version: BTreeMap<String, String>,
}

impl RepositoryIndex {
    /// Builds the index from a record slice.
    #[must_use]
    pub fn build(records: &[GraphRecord]) -> Self {
        // Tombstoned repositories (e.g. an identity change in an incremental
        // scan) are not part of the current state: they must neither resolve
        // as selectors nor make a live repository's selector ambiguous. Over an
        // append-only `--graph`, a Repository node re-ingested AFTER its own
        // tombstone is live again; the shared gate reports a tombstone active
        // only when it is the id's most recent write, matching the embedded
        // current-state read so `--graph` and `--data-dir` agree (issue #432).
        let liveness = Liveness::new(records);

        // Coalesce Repository nodes to the latest write per id (issue #432):
        // over an append-only `--graph` a repository revived AFTER its own
        // tombstone — or re-scanned from a different checkout under a remote
        // identity whose id is stable but whose `canonical_path`/selectors
        // differ — has several physical writes whose version-varying
        // identity/selector fields differ. The embedded `--data-dir` read
        // exposes only the latest write, so building the entry from the FIRST
        // write (the prior `or_insert_with`) would resolve on STALE selectors
        // over `--graph` while `--data-dir` resolves on the latest. Selecting
        // the latest write per id keeps the two transports in agreement.
        let latest_repo: BTreeMap<&str, usize> = records
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match r {
                GraphRecord::Node {
                    id,
                    kind: NodeKind::Repository,
                    ..
                } => Some((id.as_str(), i)),
                _ => None,
            })
            .collect();

        let mut repos: BTreeMap<String, RepositoryEntry> = BTreeMap::new();
        for (record_index, record) in records.iter().enumerate() {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                name,
                repository_identity,
                source_snapshot,
                ingested_at,
                ..
            } = record
            else {
                continue;
            };
            if latest_repo.get(id.as_str()) != Some(&record_index) {
                continue;
            }
            if liveness.deleted(id.as_str()) {
                continue;
            }
            let mut selectors: BTreeSet<String> = BTreeSet::new();
            let mut display = name.clone();
            if let Some(payload) = repository_identity.as_deref() {
                selectors.insert(payload.basename.clone());
                for handle in [
                    payload.remote_url.as_deref(),
                    payload.root_commit_sha.as_deref(),
                    payload.canonical_path.as_deref(),
                ]
                .into_iter()
                .flatten()
                {
                    selectors.insert(handle.to_owned());
                }
                if display.is_none() {
                    display = Some(payload.basename.clone());
                }
            }
            if let Some(display_name) = &display {
                selectors.insert(display_name.clone());
            }
            // Remote-backed identities store the remote path (`owner/name`)
            // as both basename and display name; the human-usable final path
            // segment (`name`) must resolve as a selector too.
            let shorts: Vec<String> = selectors
                .iter()
                .filter_map(|s| s.rsplit('/').next())
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            selectors.extend(shorts);
            repos.entry(id.clone()).or_insert_with(|| RepositoryEntry {
                display: display.unwrap_or_else(|| id.clone()),
                selectors,
                identity: repository_identity.as_deref().cloned(),
                source_snapshot: source_snapshot.as_deref().cloned(),
                ingested_at: ingested_at.clone(),
            });
        }

        // Containment adjacency over the deterministic code-graph topology.
        //
        // A tombstoned or superseded containment edge must not still attribute
        // ownership over an append-only `--graph`: an embedded `--data-dir`
        // current-state read never sees a retracted/stale edge, so including
        // one here would let the two transports disagree on which repository
        // owns a node for the exact same current state (mirrors the same
        // `deleted`/`is_latest_edge_version` gate `Adjacency::build` in
        // `query::sessions` already applies to its own edge classes).
        //
        // Node kind by record ID, so the walk can tell the two `IMPORTS`
        // shapes apart. A node's kind never changes across versions; the first
        // write wins.
        let mut node_kind: BTreeMap<&str, NodeKind> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Node { id, kind, .. } = record {
                node_kind.entry(id.as_str()).or_insert(*kind);
            }
        }
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (position, record) in records.iter().enumerate() {
            let GraphRecord::Edge {
                id,
                label,
                source,
                target,
                ..
            } = record
            else {
                continue;
            };
            // Ownership follows containment topology only. An `IMPORTS` edge
            // is containment in its extractor shape (`File —IMPORTS→ Import`,
            // the import declaration owned by its file); the issue-#444
            // target edge (`File —IMPORTS→ Module|File`) is a dependency edge
            // into another file's tree — following it would reattribute the
            // imported module (and everything below it) to the importing
            // repository. An unresolvable target kind reads as non-containment
            // (fail closed).
            let is_containment = match label {
                EdgeLabel::Contains | EdgeLabel::Defines => true,
                EdgeLabel::Imports => node_kind
                    .get(target.as_str())
                    .is_some_and(|kind| *kind == NodeKind::Import),
                _ => false,
            };
            if !is_containment {
                continue;
            }
            if liveness.deleted(id.as_str()) || !liveness.is_latest_edge_version(id, position) {
                continue;
            }
            adjacency.entry(source.as_str()).or_default().push(target);
        }

        let mut owner: BTreeMap<String, String> = BTreeMap::new();
        for repo_id in repos.keys() {
            let mut stack: Vec<&str> = vec![repo_id.as_str()];
            while let Some(node_id) = stack.pop() {
                if owner
                    .insert(node_id.to_owned(), repo_id.clone())
                    .is_some_and(|prev| prev == *repo_id)
                {
                    continue;
                }
                if let Some(next) = adjacency.get(node_id) {
                    stack.extend(next.iter().copied());
                }
            }
        }

        // SemanticDrift nodes hang off their target symbol, not the
        // containment topology: attribute them through DRIFTS_FROM (preferred)
        // or the drift metadata's record handles.
        let mut drift_targets: BTreeMap<&str, &str> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Edge {
                label: EdgeLabel::DriftsFrom,
                source,
                target,
                ..
            } = record
            {
                drift_targets.entry(source.as_str()).or_insert(target);
            }
        }
        for record in records {
            let GraphRecord::Node {
                id,
                kind: NodeKind::SemanticDrift,
                semantic_drift,
                ..
            } = record
            else {
                continue;
            };
            if owner.contains_key(id.as_str()) {
                continue;
            }
            let target = drift_targets.get(id.as_str()).copied().or_else(|| {
                semantic_drift
                    .as_deref()
                    .map(|d| d.target_record_id.as_str())
            });
            let fallback = semantic_drift
                .as_deref()
                .map(|d| d.prior_record_id.as_str());
            let resolved = target
                .and_then(|t| owner.get(t))
                .or_else(|| fallback.and_then(|t| owner.get(t)))
                .cloned();
            if let Some(repo_id) = resolved {
                owner.insert(id.clone(), repo_id);
            }
        }
        // Log-domain nodes (issue #362, schema v3): `scan-logs` persists a
        // retrievable `repository_id` on every log payload, byte-equal to the
        // code `Repository` node ID computed at scan time. Attribute each log
        // node directly from that field so `owner_of` resolves and `--repo` can
        // soundly filter log signatures. Log records live off the containment
        // topology (their edges run signature→source, bucket→signature), so they
        // are never already in `owner`; a plain insert suffices. A legacy
        // `log:v2:` record deserializes `repository_id` to an empty string
        // (serde default) and stays unattributed — `owner_of` returns `None`, and
        // scoped consumers exclude it rather than guess its repository. Populated
        // BEFORE the highest-version remap below so a log owner recorded under an
        // older `Repository` schema version aligns with the highest-version
        // selector the same way code owners do.
        for record in records {
            if let GraphRecord::Node {
                id,
                log: Some(payload),
                ..
            } = record
            {
                let repo_id = payload.repository_id();
                if !repo_id.is_empty() {
                    owner
                        .entry(id.clone())
                        .or_insert_with(|| repo_id.to_owned());
                }
            }
        }

        // Remap owners to their highest-version counterpart to preserve all schema-version owners.
        let mut suffix_to_versions: HashMap<&str, Vec<(u32, &str)>> = HashMap::new();
        for repo_id in repos.keys() {
            if let Some((version, suffix)) = parse_codegraph_id(repo_id) {
                suffix_to_versions
                    .entry(suffix)
                    .or_default()
                    .push((version, repo_id.as_str()));
            }
        }
        let mut repo_translation: HashMap<String, String> = HashMap::new();
        for versions in suffix_to_versions.values() {
            if let Some((_, highest_repo_id)) = versions.iter().max_by_key(|(v, _)| v) {
                for (_, repo_id) in versions {
                    repo_translation.insert((*repo_id).to_owned(), (*highest_repo_id).to_owned());
                }
            }
        }
        for val in owner.values_mut() {
            if let Some(highest_id) = repo_translation.get(val) {
                *val = highest_id.clone();
            }
        }

        let highest_version: BTreeMap<String, String> = repo_translation.into_iter().collect();

        Self {
            owner,
            repos,
            highest_version,
        }
    }

    /// Returns the owning repository record ID for a node record ID.
    #[must_use]
    pub fn owner_of(&self, record_id: &str) -> Option<&str> {
        self.owner.get(record_id).map(String::as_str)
    }

    /// Returns the human-usable display handle for a repository record ID.
    #[must_use]
    pub fn display_of(&self, repository_id: &str) -> Option<&str> {
        self.repos.get(repository_id).map(|e| e.display.as_str())
    }

    /// Returns every repository record ID known to the index, sorted ascending.
    #[must_use]
    pub fn repository_ids(&self) -> Vec<&str> {
        self.repos.keys().map(String::as_str).collect()
    }

    /// Resolves a repository selector to a stable repository record ID.
    ///
    /// Accepts the stable repository record ID directly, or any human-usable
    /// handle derived from the identity payload: the display handle (e.g.
    /// remote `owner/name`), the basename / operator override, the normalized
    /// remote URL, the root commit SHA, or the canonical path.
    ///
    /// # Errors
    ///
    /// Returns [`RepositorySelectorError::Unknown`] when nothing matches and
    /// [`RepositorySelectorError::Ambiguous`] (with every candidate listed)
    /// when more than one repository matches. Ambiguity is never resolved by
    /// picking a repository implicitly.
    pub fn resolve_selector(&self, selector: &str) -> Result<&str, RepositorySelectorError> {
        let resolved = if let Some((id, _)) = self.repos.get_key_value(selector) {
            id.as_str()
        } else {
            let mut candidates: Vec<&str> = self
                .repos
                .iter()
                .filter(|(_, entry)| entry.selectors.contains(selector))
                .map(|(id, _)| id.as_str())
                .collect();

            // Deduplicate candidates that represent the same repository under different schema versions.
            if candidates.len() > 1 {
                let mut groups: std::collections::HashMap<&str, (u32, &str)> =
                    std::collections::HashMap::new();
                let mut has_unparseable = false;
                for candidate in &candidates {
                    if let Some((version, suffix)) = parse_codegraph_id(candidate) {
                        let entry = groups.entry(suffix).or_insert((0, ""));
                        if version > entry.0 {
                            *entry = (version, candidate);
                        }
                    } else {
                        has_unparseable = true;
                        break;
                    }
                }
                if !has_unparseable {
                    candidates = groups.values().map(|(_, id)| *id).collect();
                    candidates.sort_unstable();
                }
            }

            match candidates.as_slice() {
                [] => {
                    return Err(RepositorySelectorError::Unknown {
                        selector: selector.to_owned(),
                    });
                }
                [single] => *single,
                _ => {
                    return Err(RepositorySelectorError::Ambiguous {
                        selector: selector.to_owned(),
                        candidates: candidates.into_iter().map(str::to_owned).collect(),
                    });
                }
            }
        };

        Ok(self
            .highest_version
            .get(resolved)
            .map_or(resolved, String::as_str))
    }

    /// Builds the repository catalog (issue #193): one entry per live
    /// repository in the store, each carrying the stable record ID, its
    /// `identity_source`, the #67-accepted scope selector, the attributed node
    /// count, and a recency hint.
    ///
    /// The catalog is a pure view of this index:
    /// - Only the highest-version (current) view of each repository is
    ///   listed; superseded schema versions and tombstoned repositories are
    ///   excluded, so `--graph` and embedded `--data-dir` reads agree.
    /// - The emitted `selector` is the repository's human-usable handle
    ///   (e.g. remote `owner/name`, the operator override) whenever
    ///   [`Self::resolve_selector`] accepts it verbatim for exactly this
    ///   repository; when the handle is shared with (or shadowed by) another
    ///   repository, the entry falls back to the distinct stable record ID
    ///   and flags `selector_collision`, so the collision is surfaced rather
    ///   than silently de-duplicated.
    /// - Entries sort by ascending stable record ID and carry no
    ///   catalog-time timestamps, so the output is deterministic and
    ///   byte-identical across runs on an unchanged store.
    /// - The output allow-list is IDs, identity handles/hashes, selectors,
    ///   counts, and a recency handle — never raw transcript text, command
    ///   output, patch hunks, issue/PR bodies, environment values, or tokens.
    #[must_use]
    pub fn catalog(&self) -> Vec<RepositoryCatalogEntry> {
        // `highest_version` translates every repository record ID to its
        // highest-version (current) ID. The catalog lists each distinct
        // current ID once; a `BTreeSet` keeps ascending-ID order. Repository
        // IDs the translation does not cover (unparseable version prefixes)
        // are their own current view, matching `resolve_selector`.
        let mut current: BTreeSet<&str> =
            self.highest_version.values().map(String::as_str).collect();
        for id in self.repos.keys() {
            if !self.highest_version.contains_key(id) {
                current.insert(id.as_str());
            }
        }
        current
            .into_iter()
            .filter_map(|id| self.catalog_entry(id))
            .collect()
    }

    /// Builds one catalog entry for a highest-version repository ID, or
    /// `None` when the index no longer carries the repository row.
    fn catalog_entry(&self, id: &str) -> Option<RepositoryCatalogEntry> {
        let entry = self.repos.get(id)?;

        // The human-usable handle is the #67 selector only when the #67
        // resolver accepts it verbatim for exactly this repository.
        // Otherwise the entry keeps its distinct stable ID as the selector
        // and flags the collision so it is surfaced, never de-duplicated.
        let (selector, colliding_selector) = match self.resolve_selector(&entry.display) {
            Ok(resolved) if resolved == id => (entry.display.clone(), None),
            _ => (id.to_owned(), Some(entry.display.clone())),
        };

        let node_count = self
            .owner
            .values()
            .filter(|owner| owner.as_str() == id)
            .count();

        let (recency_kind, recency_handle) = Self::catalog_recency(entry);

        Some(RepositoryCatalogEntry {
            repository_id: id.to_owned(),
            identity_source: entry
                .identity
                .as_ref()
                .map(|identity| identity.identity_source),
            selector,
            selector_collision: colliding_selector.is_some(),
            colliding_selector,
            node_count,
            recency_kind,
            recency_handle,
        })
    }

    /// Recency hint for one repository row: the source-snapshot HEAD commit
    /// SHA when one is recorded, otherwise the repository node's
    /// `ingested_at` timestamp, otherwise nothing.
    fn catalog_recency(entry: &RepositoryEntry) -> (Option<RecencyKind>, Option<String>) {
        let commit_sha = entry
            .source_snapshot
            .as_ref()
            .and_then(|snapshot| match &snapshot.head {
                SnapshotHead::Commit { sha } => Some(sha.as_str()),
                SnapshotHead::NoGit | SnapshotHead::UnbornHead => None,
            });
        commit_sha
            .map(|sha| (Some(RecencyKind::Commit), Some(sha.to_owned())))
            .or_else(|| {
                entry.ingested_at.as_deref().map(|ingested_at| {
                    (
                        Some(RecencyKind::IngestTimestamp),
                        Some(ingested_at.to_owned()),
                    )
                })
            })
            .unwrap_or((None, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        ErrorSignaturePayload, IdentitySource, LOG_SCHEMA_VERSION, LogOccurrenceBucketPayload,
        LogPayload, RepositoryIdentityPayload, SCHEMA_VERSION, SnapshotHead, SourceSnapshotPayload,
        log_stable_id,
    };

    fn repo_node(id: &str, basename: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(basename.to_owned()),
            format!("repository {basename}"),
        )
        .with_repository_identity(RepositoryIdentityPayload {
            identity_source: IdentitySource::OperatorOverride,
            remote_url: None,
            root_commit_sha: None,
            canonical_path: None,
            basename: basename.to_owned(),
        })
    }

    fn repo_tombstone(deleted_id: &str) -> GraphRecord {
        GraphRecord::Tombstone {
            id: format!("codegraph:v{SCHEMA_VERSION}:tomb-{deleted_id}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: deleted_id.to_owned(),
            summary: "removed".to_owned(),
            producer: None,
        }
    }

    #[test]
    fn repository_reingested_after_tombstone_resolves_selector() {
        // Append-only `--graph`: a Repository node re-ingested AFTER its own
        // tombstone is live again (latest write wins), matching the embedded
        // coalesced read (issue #432).
        let repo_id = "codegraph:v1:repo-x";
        let records = vec![
            repo_node(repo_id, "myrepo"),
            repo_tombstone(repo_id),
            repo_node(repo_id, "myrepo"),
        ];
        let index = RepositoryIndex::build(&records);
        assert_eq!(index.resolve_selector("myrepo"), Ok(repo_id));
    }

    #[test]
    fn revived_repository_with_changed_selectors_uses_latest_version() {
        // Issue #432 (round 2): a Repository revived AFTER its tombstone with an
        // UPDATED identity/selector set must resolve on its LATEST selectors, not
        // the STALE pre-tombstone ones. Over an append-only `--graph` both
        // physical writes are present; the embedded `--data-dir` read exposes only
        // the latest. Building the entry from the latest write per id keeps the
        // two transports in agreement.
        let repo_id = "codegraph:v1:repo-x";
        let records = vec![
            repo_node(repo_id, "old-name"),
            repo_tombstone(repo_id),
            repo_node(repo_id, "new-name"),
        ];
        let index = RepositoryIndex::build(&records);
        // The latest selector resolves.
        assert_eq!(index.resolve_selector("new-name"), Ok(repo_id));
        // The stale pre-tombstone selector must NOT resolve.
        assert!(
            matches!(
                index.resolve_selector("old-name"),
                Err(RepositorySelectorError::Unknown { .. })
            ),
            "a stale pre-tombstone selector must not resolve the revived repository"
        );
    }

    #[test]
    fn tombstoned_containment_edge_does_not_attribute_ownership() {
        // Issue #112 review round 17: a CONTAINS/DEFINES/IMPORTS edge later
        // retracted (e.g. a re-scan after a file moved out of the repo) must
        // not still attribute ownership over an append-only `--graph` — an
        // embedded `--data-dir` current-state read never sees the retracted
        // edge, so including it here would let the two transports disagree
        // on which repository owns the SAME live node for the SAME current
        // state.
        let repo_id = "codegraph:v1:repo-x";
        let file_id = "codegraph:v1:file-y";
        let contains = GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.to_owned(),
            file_id.to_owned(),
            Some("1.0".to_owned()),
            "repository contains file".to_owned(),
        );
        let edge_id = contains.id().to_owned();
        let edge_tombstone = GraphRecord::Tombstone {
            id: format!("codegraph:v{SCHEMA_VERSION}:tomb-{edge_id}"),
            schema_version: SCHEMA_VERSION,
            deleted_id: edge_id,
            summary: "removed".to_owned(),
            producer: None,
        };
        let records = vec![repo_node(repo_id, "myrepo"), contains, edge_tombstone];
        let index = RepositoryIndex::build(&records);
        assert_eq!(
            index.owner_of(file_id),
            None,
            "a tombstoned containment edge must not attribute ownership"
        );
    }

    #[test]
    fn re_ingested_containment_edge_still_attributes_from_its_latest_write() {
        // Two physical writes of the SAME edge id (identical label/source/
        // target — e.g. the edge re-observed on a later scan with a changed
        // confidence value) must still attribute ownership normally: the
        // round-17 liveness gate excludes an edge only when it is actually
        // TOMBSTONED, never merely because an earlier write of the same id
        // exists. Guards against the tombstone-exclusion fix above being
        // over-broad.
        let repo_id = "codegraph:v1:repo-x";
        let file_id = "codegraph:v1:file-y";
        let contains_v1 = GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.to_owned(),
            file_id.to_owned(),
            Some("0.5".to_owned()),
            "repository contains file (v1)".to_owned(),
        );
        let contains_v2 = GraphRecord::edge(
            EdgeLabel::Contains,
            repo_id.to_owned(),
            file_id.to_owned(),
            Some("1.0".to_owned()),
            "repository contains file (v2)".to_owned(),
        );
        let records = vec![repo_node(repo_id, "myrepo"), contains_v1, contains_v2];
        let index = RepositoryIndex::build(&records);
        assert_eq!(index.owner_of(file_id), Some(repo_id));
    }

    #[test]
    fn repository_tombstoned_without_reingest_stays_deleted() {
        let repo_id = "codegraph:v1:repo-x";
        let records = vec![repo_node(repo_id, "myrepo"), repo_tombstone(repo_id)];
        let index = RepositoryIndex::build(&records);
        assert!(matches!(
            index.resolve_selector("myrepo"),
            Err(RepositorySelectorError::Unknown { .. })
        ));
    }

    /// Builds an `ErrorSignature` log node carrying `repository_id` (schema v3,
    /// issue #362). An empty `repository_id` models a legacy `log:v2:` record.
    fn error_signature(seed: &str, repository_id: &str) -> GraphRecord {
        let id = log_stable_id(&["error_signature", "repo", seed]);
        GraphRecord::node(
            id,
            NodeKind::ErrorSignature,
            None,
            None,
            Some("error signature".to_owned()),
            "Error signature".to_owned(),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::ErrorSignature(ErrorSignaturePayload {
            fingerprint_algorithm: "template-v1".to_owned(),
            template_excerpt: format!("template {seed}"),
            severity: "error".to_owned(),
            occurrence_count: 1,
            first_seen: "2026-01-01T00:00:00Z".to_owned(),
            last_seen: "2026-01-01T00:00:00Z".to_owned(),
            frames: None,
            repository_id: repository_id.to_owned(),
        }))
    }

    /// Builds a `LogOccurrenceBucket` log node carrying `repository_id`.
    fn occurrence_bucket(seed: &str, repository_id: &str) -> GraphRecord {
        let id = log_stable_id(&["log_occurrence_bucket", seed]);
        GraphRecord::node(
            id,
            NodeKind::LogOccurrenceBucket,
            None,
            None,
            Some("bucket".to_owned()),
            "Occurrence bucket".to_owned(),
        )
        .with_domain("log", LOG_SCHEMA_VERSION)
        .with_log(LogPayload::LogOccurrenceBucket(
            LogOccurrenceBucketPayload {
                bucket_start: "2026-01-01T00:00:00Z".to_owned(),
                bucket_width: "1h".to_owned(),
                occurrence_count: 1,
                source_id: "log:v3:source".to_owned(),
                repository_id: repository_id.to_owned(),
                occurrence_timestamps: Vec::new(),
            },
        ))
    }

    #[test]
    fn owner_of_resolves_log_record_from_repository_id() {
        // A v3 scan-logs graph persists `repository_id` on every log payload,
        // byte-equal to the code `Repository` node ID. `owner_of` must resolve
        // through it even without any containment edge (issue #362).
        let repo_id = "codegraph:v1:repo-a";
        let sig = error_signature("boom", repo_id);
        let bucket = occurrence_bucket("boom-hour", repo_id);
        let sig_id = sig.id().to_owned();
        let bucket_id = bucket.id().to_owned();

        let index = RepositoryIndex::build(&[sig, bucket]);

        assert_eq!(index.owner_of(&sig_id), Some(repo_id));
        assert_eq!(index.owner_of(&bucket_id), Some(repo_id));
    }

    #[test]
    fn owner_of_returns_none_for_legacy_unattributed_log_record() {
        // A legacy `log:v2:` record deserializes `repository_id` to an empty
        // string (serde default) and stays unattributed — `owner_of` is `None`.
        let sig = error_signature("legacy", "");
        let sig_id = sig.id().to_owned();

        let index = RepositoryIndex::build(&[sig]);

        assert_eq!(index.owner_of(&sig_id), None);
    }

    fn code_node(id: &str, kind: NodeKind, path: Option<&str>, name: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            kind,
            path.map(str::to_owned),
            None,
            Some(name.to_owned()),
            format!("{kind:?} {name}"),
        )
    }

    fn code_edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            None,
            format!("{label:?} {source} -> {target}"),
        )
    }

    // ── Issue #193: repository catalog ──────────────────────────────────

    fn repo_node_with_identity(
        id: &str,
        display_name: &str,
        payload: RepositoryIdentityPayload,
    ) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Repository,
            None,
            None,
            Some(display_name.to_owned()),
            format!("Repository {display_name}"),
        )
        .with_repository_identity(payload)
    }

    fn remote_payload(owner_name: &str) -> RepositoryIdentityPayload {
        RepositoryIdentityPayload {
            identity_source: IdentitySource::Remote,
            remote_url: Some(format!("https://github.com/{owner_name}")),
            root_commit_sha: None,
            canonical_path: None,
            basename: owner_name.to_owned(),
        }
    }

    fn override_payload(name: &str) -> RepositoryIdentityPayload {
        RepositoryIdentityPayload {
            identity_source: IdentitySource::OperatorOverride,
            remote_url: None,
            root_commit_sha: None,
            canonical_path: None,
            basename: name.to_owned(),
        }
    }

    fn with_snapshot(record: GraphRecord, repo_id: &str, sha: &str) -> GraphRecord {
        record.with_source_snapshot(SourceSnapshotPayload {
            head: SnapshotHead::Commit {
                sha: sha.to_owned(),
            },
            dirty: false,
            repository_id: repo_id.to_owned(),
            scanned_at: "2026-01-01T00:00:00Z".to_owned(),
        })
    }

    fn with_ingested_at(record: GraphRecord, ts: &str) -> GraphRecord {
        let mut record = record;
        if let GraphRecord::Node { ingested_at, .. } = &mut record {
            *ingested_at = Some(ts.to_owned());
        }
        record
    }

    #[test]
    fn catalog_lists_two_repositories_with_selectors_counts_and_recency() {
        // Issue #193 AC: a two-repository fixture (both containing src/lib.rs
        // and a same-named symbol) catalogs exactly those two repositories —
        // recall and precision are both 100%.
        let repo_a = "codegraph:v5:repo-alpha";
        let repo_b = "codegraph:v5:repo-beta";
        let file_a = "codegraph:v5:file-alpha";
        let file_b = "codegraph:v5:file-beta";
        let sym_a = "codegraph:v5:sym-alpha";
        let sym_b = "codegraph:v5:sym-beta";
        let records = vec![
            with_snapshot(
                repo_node_with_identity(repo_a, "acme/alpha", remote_payload("acme/alpha")),
                repo_a,
                "aaa111",
            ),
            with_ingested_at(
                repo_node_with_identity(repo_b, "beta", override_payload("beta")),
                "2026-02-01T00:00:00Z",
            ),
            code_node(file_a, NodeKind::File, Some("src/lib.rs"), "lib.rs"),
            code_node(file_b, NodeKind::File, Some("src/lib.rs"), "lib.rs"),
            code_node(sym_a, NodeKind::Symbol, Some("src/lib.rs"), "greet"),
            code_node(sym_b, NodeKind::Symbol, Some("src/lib.rs"), "greet"),
            code_edge(EdgeLabel::Contains, repo_a, file_a),
            code_edge(EdgeLabel::Contains, repo_b, file_b),
            code_edge(EdgeLabel::Defines, file_a, sym_a),
            code_edge(EdgeLabel::Defines, file_b, sym_b),
        ];
        let index = RepositoryIndex::build(&records);
        let catalog = index.catalog();

        assert_eq!(catalog.len(), 2, "recall and precision are both 100%");
        // Canonical ordering: ascending stable record ID.
        assert_eq!(catalog[0].repository_id, repo_a);
        assert_eq!(catalog[1].repository_id, repo_b);

        assert_eq!(catalog[0].identity_source, Some(IdentitySource::Remote));
        assert_eq!(catalog[0].selector, "acme/alpha");
        assert!(!catalog[0].selector_collision);
        assert_eq!(catalog[0].colliding_selector, None);

        assert_eq!(
            catalog[1].identity_source,
            Some(IdentitySource::OperatorOverride)
        );
        assert_eq!(catalog[1].selector, "beta");
        assert!(!catalog[1].selector_collision);

        // Node counts: repository node + file + symbol each.
        assert_eq!(catalog[0].node_count, 3);
        assert_eq!(catalog[1].node_count, 3);

        // Recency: commit handle preferred, ingest timestamp as fallback.
        assert_eq!(catalog[0].recency_kind, Some(RecencyKind::Commit));
        assert_eq!(catalog[0].recency_handle.as_deref(), Some("aaa111"));
        assert_eq!(catalog[1].recency_kind, Some(RecencyKind::IngestTimestamp));
        assert_eq!(
            catalog[1].recency_handle.as_deref(),
            Some("2026-02-01T00:00:00Z")
        );

        // Round-trip (issue #193 AC): every emitted selector is accepted
        // verbatim by the #67 scoped-query path and scopes to the right
        // repository — no catalog entry advertises a selector that scoping
        // rejects or that cites the wrong repository.
        for entry in &catalog {
            let resolved = index
                .resolve_selector(&entry.selector)
                .expect("every catalog selector must resolve verbatim");
            assert_eq!(resolved, entry.repository_id);
        }
        assert_eq!(index.owner_of(file_a), Some(repo_a));
        assert_eq!(index.owner_of(sym_a), Some(repo_a));
        assert_eq!(index.owner_of(file_b), Some(repo_b));
        assert_eq!(index.owner_of(sym_b), Some(repo_b));
    }

    #[test]
    fn catalog_collision_surfaces_distinct_stable_ids() {
        // Issue #193 AC: two repositories sharing one human selector (same
        // basename, different stable IDs) are both listed — never silently
        // de-duplicated — and each entry's emitted selector falls back to its
        // distinct stable ID so the #67 path still accepts it verbatim.
        let repo_a = "codegraph:v5:repo-one";
        let repo_b = "codegraph:v5:repo-two";
        let records = vec![
            repo_node_with_identity(repo_a, "acme", override_payload("acme")),
            repo_node_with_identity(repo_b, "acme", override_payload("acme")),
        ];
        let index = RepositoryIndex::build(&records);
        let catalog = index.catalog();

        assert_eq!(
            catalog.len(),
            2,
            "colliding repositories must both be listed"
        );
        assert_ne!(catalog[0].repository_id, catalog[1].repository_id);
        for entry in &catalog {
            assert!(
                entry.selector_collision,
                "the shared human selector must be flagged"
            );
            assert_eq!(
                entry.selector, entry.repository_id,
                "on collision the stable ID is the verbatim-accepted selector"
            );
            assert_eq!(entry.colliding_selector.as_deref(), Some("acme"));
            assert_eq!(entry.node_count, 1, "the repository node itself");
            let resolved = index
                .resolve_selector(&entry.selector)
                .expect("a stable ID must always resolve verbatim");
            assert_eq!(resolved, entry.repository_id);
        }
        // The shared human handle is genuinely ambiguous on the #67 path —
        // that is exactly why the catalog does not emit it.
        assert!(
            matches!(
                index.resolve_selector("acme"),
                Err(RepositorySelectorError::Ambiguous { .. })
            ),
            "the colliding human selector must stay ambiguous"
        );
    }

    #[test]
    fn catalog_collapses_superseded_repository_versions() {
        // One logical repository written at two schema versions catalogs
        // once, at its highest-version (current) view.
        let old = "codegraph:v1:repo-x";
        let new = "codegraph:v5:repo-x";
        let file = "codegraph:v5:file-x";
        let records = vec![
            repo_node_with_identity(old, "old-name", override_payload("old-name")),
            repo_node_with_identity(new, "new-name", override_payload("new-name")),
            code_node(file, NodeKind::File, Some("src/lib.rs"), "lib.rs"),
            code_edge(EdgeLabel::Contains, old, file),
            code_edge(EdgeLabel::Contains, new, file),
        ];
        let catalog = RepositoryIndex::build(&records).catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].repository_id, new);
        assert_eq!(catalog[0].selector, "new-name");
        assert!(!catalog[0].selector_collision);
    }

    #[test]
    fn catalog_excludes_tombstoned_repository() {
        let live = "codegraph:v5:repo-live";
        let dead = "codegraph:v5:repo-dead";
        let records = vec![
            repo_node_with_identity(live, "live", override_payload("live")),
            repo_node_with_identity(dead, "dead", override_payload("dead")),
            repo_tombstone(dead),
        ];
        let catalog = RepositoryIndex::build(&records).catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].repository_id, live);
    }

    #[test]
    fn catalog_is_empty_without_repositories() {
        let records = vec![code_node(
            "codegraph:v5:file-a",
            NodeKind::File,
            Some("src/lib.rs"),
            "lib.rs",
        )];
        assert!(RepositoryIndex::build(&records).catalog().is_empty());
    }

    #[test]
    fn catalog_recency_is_none_without_snapshot_or_ingest_time() {
        let repo = "codegraph:v5:repo-plain";
        let records = vec![repo_node_with_identity(
            repo,
            "plain",
            override_payload("plain"),
        )];
        let catalog = RepositoryIndex::build(&records).catalog();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].recency_kind, None);
        assert_eq!(catalog[0].recency_handle, None);
    }

    #[test]
    fn catalog_entries_serialize_with_stable_field_shape() {
        // The documented NDJSON contract: every field present on every line,
        // `identity_source` in the closed snake_case vocabulary.
        let repo = "codegraph:v5:repo-ser";
        let records = vec![with_snapshot(
            repo_node_with_identity(repo, "acme/ser", remote_payload("acme/ser")),
            repo,
            "fff999",
        )];
        let catalog = RepositoryIndex::build(&records).catalog();
        assert_eq!(catalog.len(), 1);
        let line = serde_json::to_string(&catalog[0]).expect("entry must serialize");
        let value: serde_json::Value =
            serde_json::from_str(&line).expect("entry must be valid JSON");
        assert_eq!(value["repository_id"], repo);
        assert_eq!(value["identity_source"], "remote");
        assert_eq!(value["selector"], "acme/ser");
        assert_eq!(value["selector_collision"], false);
        assert!(value["colliding_selector"].is_null());
        assert_eq!(value["node_count"], 1);
        assert_eq!(value["recency_kind"], "commit");
        assert_eq!(value["recency_handle"], "fff999");
    }

    #[test]
    fn import_target_edge_does_not_reattribute_module_to_importing_repo() {
        // Issue #444: a `File —IMPORTS→ Module` target edge is a dependency
        // edge, not containment. The ownership walk must not follow it: the
        // imported module stays owned by its own repository even when the
        // importing repository sorts LAST (the order whose DFS would otherwise
        // claim the module last and win the overwrite).
        let repo_target = "codegraph:v1:repo-alpha";
        let repo_importer = "codegraph:v1:repo-zebra";
        let file_target = "codegraph:v1:file-alpha";
        let file_importer = "codegraph:v1:file-zebra";
        let module = "codegraph:v1:module-alpha";
        let import_decl = "codegraph:v1:import-zebra";
        let records = vec![
            repo_node(repo_target, "alpha"),
            repo_node(repo_importer, "zebra"),
            code_node(file_target, NodeKind::File, Some("src/lib.rs"), "lib.rs"),
            code_node(file_importer, NodeKind::File, Some("src/use.rs"), "use.rs"),
            code_node(module, NodeKind::Module, Some("src/lib.rs"), "inner"),
            code_node(
                import_decl,
                NodeKind::Import,
                Some("src/use.rs"),
                "crate::inner",
            ),
            code_edge(EdgeLabel::Contains, repo_target, file_target),
            code_edge(EdgeLabel::Contains, repo_importer, file_importer),
            code_edge(EdgeLabel::Defines, file_target, module),
            // The extractor's containment shape: the import declaration is
            // owned by its file, so this edge IS followed.
            code_edge(EdgeLabel::Imports, file_importer, import_decl),
            // The issue-#444 target shape: a dependency edge into another
            // file's tree, NOT followed.
            code_edge(EdgeLabel::Imports, file_importer, module),
        ];
        let index = RepositoryIndex::build(&records);

        assert_eq!(
            index.owner_of(module),
            Some(repo_target),
            "the imported module must stay owned by its own repository"
        );
        assert_eq!(index.owner_of(file_importer), Some(repo_importer));
        assert_eq!(
            index.owner_of(import_decl),
            Some(repo_importer),
            "the containment-shaped IMPORTS edge still attributes the import declaration"
        );
    }
}
