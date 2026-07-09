use std::collections::{BTreeMap, BTreeSet};

use chrono::DateTime;

use super::{CommitOrder, RepositoryIndex};
use crate::ir::{GraphRecord, NodeKind, SnapshotHead, SourceSpan};

/// The temporal point selector accepted by [`file_symbols_at_point`].
///
/// Mirrors the `eg query symbol` valid-time flags (see
/// `docs/schema/temporal-selectors.md`): `--at` pins the point to a commit
/// handle, `--as-of` to the most recent commit at or before an RFC 3339
/// instant. The two are mutually exclusive, which the type makes structural.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FileAtPointSelector<'a> {
    /// A commit handle: full SHA or unique prefix.
    At(&'a str),
    /// An RFC 3339 valid-time instant.
    AsOf(&'a str),
}

/// One symbol row of a file-at-point response (issue #158).
///
/// Serialization is deliberately bounded to identity/path/span/commit
/// metadata resolved *as-of the selected point* — never node summaries, which
/// embed normalized source bodies for `scan-history` records.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct FileAtPointSymbol<'a> {
    /// Stable record ID of the symbol snapshot at the resolved commit.
    pub record_id: &'a str,
    /// Schema version stamped on the backing record.
    pub schema_version: u32,
    /// Symbol name as recorded at the resolved commit.
    pub name: &'a str,
    /// Always `Symbol`.
    pub kind: &'static str,
    /// Language-specific symbol category, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<&'a str>,
    /// Repository-relative path of the file as recorded at the point.
    pub repo_relative_path: &'a str,
    /// Source span resolved as-of the point (not the current tree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
    /// Documented reason a symbol row carries no span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub absent_span_reason: Option<&'static str>,
    /// The resolved commit the row's state was computed against.
    pub commit: &'a str,
    /// Valid time (committer date) of the resolved commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_time: Option<&'a str>,
}

/// One stable, machine-readable diagnostic on a *successful* file-at-point
/// response (e.g. `empty_symbol_set`). Never used for failures, which are
/// [`FileAtPointError`] values.
#[derive(Debug, Clone, serde::Serialize, Eq, PartialEq)]
pub struct FileAtPointDiagnostic {
    /// Stable diagnostic code.
    pub code: &'static str,
    /// Bounded human-readable detail (identity fields only, never payloads).
    pub detail: String,
}

/// A file's defined-symbol set reconstructed at a past commit or instant.
/// Returned by [`file_symbols_at_point`] (issue #158).
///
/// Rows are canonically ordered by `(span.start_line, name, record_id)` so
/// repeated queries against an unchanged store serialize byte-identically.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileSymbolsAtPoint<'a> {
    /// The queried repository-relative path, echoed.
    pub path: &'a str,
    /// The `--at` commit handle input, echoed (`null` for `--as-of` queries).
    pub at: Option<&'a str>,
    /// The `--as-of` instant input, echoed (`null` for `--at` queries).
    pub as_of: Option<&'a str>,
    /// Full SHA of the commit the result was computed against.
    pub resolved_commit: &'a str,
    /// Valid time (committer date) of the resolved commit, when recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_valid_time: Option<&'a str>,
    /// Stable record ID of the file snapshot at the resolved commit.
    pub file_record_id: &'a str,
    /// Schema version stamped on the file snapshot record.
    pub file_schema_version: u32,
    /// The symbols the file defined at the resolved point.
    pub symbols: Vec<FileAtPointSymbol<'a>>,
    /// Number of symbol rows returned.
    pub returned: usize,
    /// Stable diagnostics (`empty_symbol_set` when the file existed at the
    /// point but defined zero symbols — explicitly distinguishable from
    /// not-found, which is an error).
    pub diagnostics: Vec<FileAtPointDiagnostic>,
}

/// Errors that can occur while resolving a file-at-point query.
///
/// Each variant serializes to a stable machine-readable diagnostic
/// (`error_type` + snake_case payload) rather than partial, fabricated, or
/// silently empty output.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "error_type", rename_all = "snake_case")]
pub enum FileAtPointError {
    /// The store carries no commit history (`scan-history` graph required).
    EmptyHistory,
    /// The `--at` commit handle resolved to no commit.
    MissingCommit {
        /// The prefix that could not be resolved.
        commit_prefix: String,
    },
    /// The `--at` commit handle was ambiguous.
    AmbiguousCommitPrefix {
        /// The prefix that resolved to multiple commits.
        commit_prefix: String,
        /// The full SHAs of the matching commits.
        matches: Vec<String>,
    },
    /// The `--as-of` instant is not a valid RFC 3339 timestamp.
    InvalidInstant {
        /// The malformed input, echoed.
        as_of: String,
        /// Parser detail.
        detail: String,
    },
    /// No commit exists at or before the `--as-of` instant.
    NoCommitAtOrBeforeInstant {
        /// The instant, echoed.
        as_of: String,
    },
    /// The path matches no file or symbol snapshot at any recorded commit.
    UnknownPath {
        /// The queried path, echoed.
        path: String,
    },
    /// The path is known to history but did not exist at the resolved point.
    FileAbsentAtPoint {
        /// The queried path, echoed.
        path: String,
        /// The full SHA of the resolved point.
        resolved_commit: String,
    },
    /// The unscoped query matched file snapshots in more than one repository;
    /// rerun with `--repo <SELECTOR>` (issue #67 contract).
    AmbiguousRepository {
        /// The queried path, echoed.
        path: String,
        /// The stable repository IDs that matched.
        repositories: Vec<String>,
    },
}

/// Reconstruct the deterministic set of symbols a file defined at a chosen
/// commit or valid-time instant (issue #158).
///
/// `scan-history` emits a full `File`/`Symbol` snapshot at every commit, so
/// the file's symbol set at a point is exactly the symbol snapshots recorded
/// at the resolved commit for that path: a symbol tombstoned at or before the
/// point has no snapshot there and can never leak into the result. Spans and
/// names are the recorded state as-of the point, not the current tree.
///
/// Code-facts only: the result reads `File`/`Symbol`/`Commit` history records
/// exclusively — agent observations, project/task, artifact, and verification
/// records are never mixed in. Purely read-time: reads only the provided
/// records, never Git state or the working tree.
///
/// # Errors
///
/// Returns a [`FileAtPointError`] when the history is empty, the commit
/// handle is missing or ambiguous, the instant is malformed or precedes the
/// first commit, the path is unknown, the path did not exist at the point, or
/// an unscoped query collides across repositories.
pub fn file_symbols_at_point<'a>(
    records: &'a [GraphRecord],
    path: &'a str,
    selector: FileAtPointSelector<'a>,
    repo_scope: Option<&str>,
) -> Result<FileSymbolsAtPoint<'a>, FileAtPointError> {
    let index = RepositoryIndex::build(records);
    let in_scope =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // ── commit timeline (scoped) ─────────────────────────────────────────────
    let mut commit_valid_time: BTreeMap<&str, &str> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Node {
            kind: NodeKind::Commit,
            name: Some(sha),
            temporal: Some(t),
            ..
        } = r
        {
            if in_scope(r.id()) {
                commit_valid_time
                    .entry(sha.as_str())
                    .or_insert(t.valid_time.as_str());
            }
        }
    }
    if commit_valid_time.is_empty() {
        return Err(FileAtPointError::EmptyHistory);
    }

    // ── point resolution ─────────────────────────────────────────────────────
    let (resolved_sha, at_input, as_of_input) = match selector {
        FileAtPointSelector::At(prefix) => {
            let lowered = prefix.to_lowercase();
            let mut matches: Vec<&str> = commit_valid_time
                .keys()
                .copied()
                .filter(|sha| sha.to_lowercase().starts_with(&lowered))
                .collect();
            matches.sort_unstable();
            matches.dedup();
            if matches.is_empty() {
                return Err(FileAtPointError::MissingCommit {
                    commit_prefix: prefix.to_owned(),
                });
            }
            if matches.len() > 1 {
                return Err(FileAtPointError::AmbiguousCommitPrefix {
                    commit_prefix: prefix.to_owned(),
                    matches: matches.iter().map(|s| (*s).to_owned()).collect(),
                });
            }
            (matches[0], Some(prefix), None)
        }
        FileAtPointSelector::AsOf(instant) => {
            let as_of_dt = DateTime::parse_from_rfc3339(instant).map_err(|e| {
                FileAtPointError::InvalidInstant {
                    as_of: instant.to_owned(),
                    detail: e.to_string(),
                }
            })?;
            // An instant must resolve on the queried path's own repository
            // timeline: in a shared multi-repository store, an unrelated
            // repository's newer commit would otherwise win the at-or-before
            // race and make the file look absent at a commit its repository
            // never had. Narrow the candidate commits to the repository
            // group(s) that actually record the path.
            let mut path_owner_groups: BTreeSet<Option<&str>> = BTreeSet::new();
            for r in records {
                if let GraphRecord::Node {
                    id,
                    kind: NodeKind::File | NodeKind::Symbol,
                    repo_relative_path: Some(p),
                    temporal: Some(_),
                    ..
                } = r
                {
                    if p == path && in_scope(id) {
                        path_owner_groups.insert(index.owner_of(id));
                    }
                }
            }
            if path_owner_groups.is_empty() {
                return Err(FileAtPointError::UnknownPath {
                    path: path.to_owned(),
                });
            }
            // Two repositories recording the same path have two distinct
            // timelines; an unscoped single-answer time view never picks one
            // implicitly (issue #67).
            if path_owner_groups.len() > 1 {
                return Err(FileAtPointError::AmbiguousRepository {
                    path: path.to_owned(),
                    repositories: path_owner_groups
                        .iter()
                        .filter_map(|g| *g)
                        .map(str::to_owned)
                        .collect(),
                });
            }
            // Exactly one group remains; `flatten` keeps the unattributed
            // (`None`) group as `None` without a panicking unwrap.
            let path_owner = path_owner_groups.into_iter().next().flatten();
            let owned_commit_shas: BTreeSet<&str> = records
                .iter()
                .filter_map(|r| {
                    if let GraphRecord::Node {
                        kind: NodeKind::Commit,
                        name: Some(sha),
                        ..
                    } = r
                    {
                        (index.owner_of(r.id()) == path_owner).then_some(sha.as_str())
                    } else {
                        None
                    }
                })
                .collect();
            // Most recent owned commit at or before the instant. Git
            // timestamps are second-resolution, so equal valid times are
            // broken by topological rank (a descendant outranks its
            // ancestors), then by SHA for full determinism.
            let order = CommitOrder::build(records);
            let best = commit_valid_time
                .iter()
                .filter(|&(&sha, _)| {
                    // Degenerate mixed-attribution stores (path attributed,
                    // commits not) fall back to the full scoped timeline
                    // rather than an empty one.
                    owned_commit_shas.is_empty() || owned_commit_shas.contains(sha)
                })
                .filter_map(|(&sha, &vt)| {
                    let parsed = DateTime::parse_from_rfc3339(vt).ok()?;
                    (parsed <= as_of_dt).then_some((parsed, order.rank(sha), sha))
                })
                .max();
            let Some((_, _, sha)) = best else {
                return Err(FileAtPointError::NoCommitAtOrBeforeInstant {
                    as_of: instant.to_owned(),
                });
            };
            (sha, None, Some(instant))
        }
    };

    // ── file existence at the point (not-found vs absent-at-point) ──────────
    let mut file_snapshots_at_point: Vec<&GraphRecord> = Vec::new();
    let mut path_known_to_history = false;
    for r in records {
        let GraphRecord::Node {
            kind,
            repo_relative_path,
            temporal: Some(t),
            ..
        } = r
        else {
            continue;
        };
        if repo_relative_path.as_deref() != Some(path) || !in_scope(r.id()) {
            continue;
        }
        match kind {
            NodeKind::File => {
                path_known_to_history = true;
                if t.git_commit == resolved_sha {
                    file_snapshots_at_point.push(r);
                }
            }
            NodeKind::Symbol => path_known_to_history = true,
            _ => {}
        }
    }

    if file_snapshots_at_point.is_empty() {
        if path_known_to_history {
            return Err(FileAtPointError::FileAbsentAtPoint {
                path: path.to_owned(),
                resolved_commit: resolved_sha.to_owned(),
            });
        }
        return Err(FileAtPointError::UnknownPath {
            path: path.to_owned(),
        });
    }

    // A shared store can carry the same path+commit under distinct repository
    // identities; never pick one implicitly (issue #67).
    let owner_groups: BTreeSet<Option<&str>> = file_snapshots_at_point
        .iter()
        .map(|r| index.owner_of(r.id()))
        .collect();
    if owner_groups.len() > 1 {
        return Err(FileAtPointError::AmbiguousRepository {
            path: path.to_owned(),
            repositories: owner_groups
                .iter()
                .filter_map(|g| *g)
                .map(str::to_owned)
                .collect(),
        });
    }

    file_snapshots_at_point.sort_by(|a, b| a.id().cmp(b.id()));
    let file_record = file_snapshots_at_point[0];
    let owner = index.owner_of(file_record.id());
    let (file_record_id, file_schema_version) = match file_record {
        GraphRecord::Node {
            id, schema_version, ..
        } => (id.as_str(), *schema_version),
        _ => unreachable!("file snapshots are node records"),
    };

    // ── symbol snapshots at the resolved commit ──────────────────────────────
    let mut symbols: Vec<FileAtPointSymbol<'a>> = Vec::new();
    for r in records {
        let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            schema_version,
            name,
            symbol_kind,
            repo_relative_path,
            span,
            temporal: Some(t),
            ..
        } = r
        else {
            continue;
        };
        if repo_relative_path.as_deref() != Some(path)
            || t.git_commit != resolved_sha
            || index.owner_of(id) != owner
        {
            continue;
        }
        symbols.push(FileAtPointSymbol {
            record_id: id,
            schema_version: *schema_version,
            name: name.as_deref().unwrap_or(""),
            kind: "Symbol",
            symbol_kind: symbol_kind.as_deref(),
            repo_relative_path: path,
            span: *span,
            absent_span_reason: span.is_none().then_some("no_span_module_level"),
            commit: resolved_sha,
            valid_time: Some(t.valid_time.as_str()),
        });
    }
    symbols.sort_by(|a, b| {
        a.span
            .map(|s| s.start_line)
            .cmp(&b.span.map(|s| s.start_line))
            .then_with(|| a.name.cmp(b.name))
            .then_with(|| a.record_id.cmp(b.record_id))
    });

    let diagnostics = if symbols.is_empty() {
        vec![FileAtPointDiagnostic {
            code: "empty_symbol_set",
            detail: format!(
                "file {path} existed at commit {resolved_sha} but defined zero symbols"
            ),
        }]
    } else {
        Vec::new()
    };

    Ok(FileSymbolsAtPoint {
        path,
        at: at_input,
        as_of: as_of_input,
        resolved_commit: resolved_sha,
        resolved_valid_time: commit_valid_time.get(resolved_sha).copied(),
        file_record_id,
        file_schema_version,
        returned: symbols.len(),
        symbols,
        diagnostics,
    })
}

// ---------------------------------------------------------------------------
// public-api surface query (issue #213)
// ---------------------------------------------------------------------------

// ── file:line → enclosing symbol resolution (issue #151) ─────────────────────

/// Resolution of one `file:line` location against stored symbol spans.
///
/// `chain` holds every `Module` and `Symbol` node whose span contains the
/// line, ordered outermost → innermost; `primary` is the innermost (smallest
/// enclosing) `Symbol` node, when one exists. `file_record` is the `File`
/// node for the path in the selected view, when present. `repo_groups`
/// carries one entry per repository owner group among the path's records so
/// callers can fail closed on an unscoped cross-repository collision
/// (issue #67); unattributed records group under `None`.
#[derive(Debug, Default)]
pub struct LocationContext<'a> {
    /// `File` node for the queried path in the selected view.
    pub file_record: Option<&'a GraphRecord>,
    /// Containing `Module`/`Symbol` nodes, outermost → innermost.
    pub chain: Vec<&'a GraphRecord>,
    /// Smallest enclosing `Symbol` node (the innermost), when one exists.
    pub primary: Option<&'a GraphRecord>,
    /// Repository owner groups among the path's matched records.
    pub repo_groups: BTreeSet<Option<&'a str>>,
}

/// Recency ordering for two versions of one stable record ID in the
/// current-state view. A non-temporal (current-scan) record outranks every
/// history-backed snapshot; history-backed snapshots order by parsed valid
/// time (unparseable valid times sort oldest), with the commit SHA as a
/// deterministic tiebreak for equal-time commits (e.g. rebases).
fn version_recency_key(record: &GraphRecord) -> (u8, Option<DateTime<chrono::FixedOffset>>, &str) {
    let GraphRecord::Node { temporal, .. } = record else {
        return (0, None, "");
    };
    temporal.as_ref().map_or((1, None, ""), |t| {
        (
            0,
            DateTime::parse_from_rfc3339(&t.valid_time).ok(),
            t.git_commit.as_str(),
        )
    })
}

/// Resolves the smallest enclosing `Symbol` for a `path:line` location.
///
/// View selection mirrors the other single-answer query verbs:
///
/// - Without `at_commit`, the current-state view applies: tombstoned records
///   are excluded and history graphs resolve each stable ID to its newest
///   version (keep-last dedupe, as `public_api_surface` does).
/// - With `at_commit` (a fully resolved SHA), only records whose
///   `temporal.git_commit` equals that commit participate, so spans resolve
///   as they existed at that commit.
///
/// Containment is by recorded line span (`start_line <= line <= end_line`).
/// The primary answer is the `Symbol` with the narrowest containing span
/// (line width, then byte width, then record ID — all ascending), never a
/// nearest-neighbor guess: a line outside every symbol span yields
/// `primary: None` even when a `Module` or the file contains it.
#[must_use]
pub fn location_context<'a>(
    records: &'a [GraphRecord],
    path: &str,
    line: usize,
    at_commit: Option<&str>,
    index: &'a RepositoryIndex,
    repo_scope: Option<&str>,
) -> LocationContext<'a> {
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    let is_owned =
        |id: &str| -> bool { repo_scope.is_none_or(|scope| index.owner_of(id) == Some(scope)) };

    // HEAD-commit SHA per repository from the stamped source snapshot
    // (issue #82), later record winning deterministically. Anchors the
    // default view of a history graph to the HEAD snapshot — matching
    // `resolve_head_symbols` — because `scan-history` emits no tombstones
    // for a path deleted or renamed at HEAD: keep-last-per-ID alone would
    // resurrect the last pre-deletion version as if it were current.
    let mut repo_heads: BTreeMap<&str, &str> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            id,
            source_snapshot: Some(snapshot),
            ..
        } = record
            && let SnapshotHead::Commit { sha } = &snapshot.head
        {
            repo_heads.insert(id.as_str(), sha.as_str());
        }
    }

    // Select the view: the current state (HEAD snapshot for history-backed
    // records with a stamped head, newest-version-per-ID otherwise, always
    // tombstone-excluded), or the exact per-commit snapshot when a temporal
    // pin is supplied.
    let mut nodes: BTreeMap<&str, &'a GraphRecord> = BTreeMap::new();
    for record in records {
        let GraphRecord::Node {
            id,
            kind,
            repo_relative_path,
            temporal,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(kind, NodeKind::File | NodeKind::Module | NodeKind::Symbol) {
            continue;
        }
        if repo_relative_path.as_deref() != Some(path) || !is_owned(id) {
            continue;
        }
        if let Some(commit) = at_commit {
            if temporal.as_ref().map(|t| t.git_commit.as_str()) != Some(commit) {
                continue;
            }
        } else {
            if tombstoned.contains(id.as_str()) {
                continue;
            }
            // A history-backed record represents the current state only
            // at the stamped HEAD commit. Records without a resolvable
            // owner or without a stamped head (legacy stores) keep the
            // newest-version-per-ID view.
            if let Some(t) = temporal
                && let Some(head) = index.owner_of(id).and_then(|repo| repo_heads.get(repo))
                && t.git_commit != *head
            {
                continue;
            }
        }
        // Newest-version-per-ID must not depend on record emission order:
        // the embedded store emits temporal snapshots in commit-SHA lexical
        // order (`read_all_records`), not commit time, so a newer commit
        // whose SHA sorts first would lose a plain last-write-wins insert.
        // Replace only when the incoming record is at least as recent.
        let replace = nodes
            .get(id.as_str())
            .is_none_or(|existing| version_recency_key(record) >= version_recency_key(existing));
        if replace {
            nodes.insert(id.as_str(), record);
        }
    }

    let mut ctx = LocationContext {
        repo_groups: nodes.keys().map(|id| index.owner_of(id)).collect(),
        ..LocationContext::default()
    };

    for record in nodes.values() {
        let GraphRecord::Node { kind, span, .. } = record else {
            continue;
        };
        if matches!(kind, NodeKind::File) {
            ctx.file_record = Some(record);
            continue;
        }
        let Some(span) = span else { continue };
        if span.start_line <= line && line <= span.end_line {
            ctx.chain.push(record);
        }
    }

    // Outermost → innermost: wider spans first; ties resolve by earlier
    // start, then record ID, so the order is total and deterministic.
    let sort_key = |record: &GraphRecord| {
        let GraphRecord::Node {
            id,
            span: Some(span),
            ..
        } = record
        else {
            unreachable!("chain entries carry spans by construction");
        };
        (
            usize::MAX - (span.end_line - span.start_line),
            usize::MAX - (span.end_byte - span.start_byte),
            span.start_byte,
            id.clone(),
        )
    };
    ctx.chain.sort_by_key(|record| sort_key(record));

    ctx.primary = ctx
        .chain
        .iter()
        .rev()
        .find(|record| {
            matches!(
                record,
                GraphRecord::Node {
                    kind: NodeKind::Symbol,
                    ..
                }
            )
        })
        .copied();

    ctx
}
