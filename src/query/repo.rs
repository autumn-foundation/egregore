use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ir::{EdgeLabel, GraphRecord, NodeKind, parse_codegraph_id};

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
        // as selectors nor make a live repository's selector ambiguous.
        let tombstoned: BTreeSet<&str> = records
            .iter()
            .filter_map(|r| {
                if let GraphRecord::Tombstone { deleted_id, .. } = r {
                    Some(deleted_id.as_str())
                } else {
                    None
                }
            })
            .collect();

        let mut repos: BTreeMap<String, RepositoryEntry> = BTreeMap::new();
        for record in records {
            let GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                name,
                repository_identity,
                ..
            } = record
            else {
                continue;
            };
            if tombstoned.contains(id.as_str()) {
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
            });
        }

        // Containment adjacency over the deterministic code-graph topology.
        let mut adjacency: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for record in records {
            if let GraphRecord::Edge {
                label: EdgeLabel::Contains | EdgeLabel::Defines | EdgeLabel::Imports,
                source,
                target,
                ..
            } = record
            {
                adjacency.entry(source.as_str()).or_default().push(target);
            }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{
        ErrorSignaturePayload, LOG_SCHEMA_VERSION, LogOccurrenceBucketPayload, LogPayload,
        log_stable_id,
    };

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
}
