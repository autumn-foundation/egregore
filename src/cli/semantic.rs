use super::*;

/// Embeds a natural-language query into a dense vector using the default local
/// model. Shared by the embedded and daemon-backed semantic search paths so
/// both produce identical query vectors (and therefore identical rankings).
///
/// The model is loaded from the local Hugging Face cache; no remote embedding
/// service is contacted at query time.
#[cfg(feature = "embeddings")]
pub(crate) fn embed_query_text(query: &str) -> Result<Vec<f32>> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_ARCHITECTURE, DEFAULT_EMBEDDING_MODEL_NAME, aletheia_embeddings,
    };

    let embedder = aletheia_embeddings::EmbedderBuilder::new()
        .model_architecture(DEFAULT_EMBEDDING_MODEL_ARCHITECTURE)
        .model_id(Some(DEFAULT_EMBEDDING_MODEL_NAME))
        .from_pretrained_hf()
        .context("failed to load embedding model")?;

    let rt = tokio::runtime::Runtime::new().context("failed to create tokio runtime")?;
    let embed_data = rt
        .block_on(aletheia_embeddings::embed_query(&[query], &embedder, None))
        .context("failed to embed query")?;

    aletheia_embeddings::embed_data_to_dense_iter(embed_data, Some(1))
        .next()
        .context("no embedding returned for query")?
        .context("embedding result was not dense")
        .map(|dense| dense.embedding)
}

/// Refuses a semantic query whose embedder does not share the store's vector
/// space (issue #104).
///
/// Runs BEFORE the query is embedded, so an incompatible store costs an operator
/// a refusal rather than a model load.
///
/// On a refusal this prints the stable machine-readable envelope on stdout, a
/// one-line human summary on stderr, and exits with the verdict's distinct
/// nonzero code. It never returns a ranked result list.
///
/// Otherwise it returns the verdict, which is one of the two non-refusal cases:
/// [`crate::embeddings::IndexCompatibility::Compatible`], or `IndexAbsent` for a
/// store that was never `--embed`ed and so has nothing to be incompatible with.
/// `IndexAbsent` is returned rather than handled here because each lane words its
/// own no-index outcome; the caller MUST handle it before searching, since a
/// vector search against a store with no index surfaces an opaque engine error
/// instead of the documented no-embeddings outcome.
///
/// A store whose index EXISTS on disk but was skipped at load is refused here
/// (`semantic_index_unreadable`, exit `11`) rather than returned as
/// `IndexAbsent` — issue #489: since `AletheiaDB` 0.2.0 skips a corrupted vector
/// index instead of failing the load, "absent" and "damaged" reach this gate
/// looking identical through the engine handle, and reporting the second as the
/// first would answer a data-loss condition with "you never ran `--embed`".
///
/// The comparison is the whole point of the gate: the semantic index stores only
/// vectors plus a dimension, and two different models can share a dimension, so
/// a dimension check alone lets a model swap, cache change, or version bump
/// produce a cosine ranking computed across incompatible vector spaces and
/// return it as a confident answer.
#[cfg(feature = "embeddings")]
pub(crate) fn enforce_index_compatibility(
    sink: &EmbeddedAletheiaSink,
    records: &[GraphRecord],
) -> Result<crate::embeddings::IndexCompatibility> {
    use crate::embeddings::{
        DEFAULT_EMBEDDING_MODEL_DIMENSIONS, classify_index_compatibility,
        default_embedding_model_identity, indexed_identities,
    };

    let verdict = classify_index_compatibility(
        &sink.embedding_index_state(),
        &indexed_identities(records),
        &default_embedding_model_identity(DEFAULT_EMBEDDING_MODEL_DIMENSIONS),
    );
    refuse_verdict(&verdict)?;
    Ok(verdict)
}

/// Prints and exits on a refusing verdict; returns `Ok(())` for a non-refusal.
///
/// The `Result` is a serialization-failure channel only.
#[cfg(feature = "embeddings")]
fn refuse_verdict(verdict: &crate::embeddings::IndexCompatibility) -> Result<()> {
    let (Some(envelope), Some(exit_code)) = (verdict.to_error_envelope(), verdict.exit_code())
    else {
        return Ok(());
    };
    println!("{}", serde_json::to_string(&envelope)?);
    eprintln!("semantic query refused: {}", verdict.message());
    std::process::exit(exit_code);
}

/// Embeds the query text, then re-checks the ACTUAL vector length against the
/// index (issue #104).
///
/// [`enforce_index_compatibility`] runs before the model loads and can therefore
/// only compare the embedder's DECLARED dimension constant. The vector the model
/// actually returns is the ground truth, and the two could disagree if the
/// resolved weights ever differ from the constant — so the real length is
/// verified here, once, before it is used to rank anything. Cheap (one integer
/// comparison) and fail-closed: a disagreement refuses with the same stable
/// `embedding_dimension_mismatch` contract rather than ranking across spaces.
#[cfg(feature = "embeddings")]
pub(crate) fn embed_query_checked(
    query: &str,
    sink: &EmbeddedAletheiaSink,
    records: &[GraphRecord],
) -> Result<Vec<f32>> {
    use crate::embeddings::{
        classify_index_compatibility, default_embedding_model_identity, indexed_identities,
    };

    let vector = embed_query_text(query)?;
    let verdict = classify_index_compatibility(
        &sink.embedding_index_state(),
        &indexed_identities(records),
        &default_embedding_model_identity(vector.len()),
    );
    refuse_verdict(&verdict)?;
    Ok(vector)
}

/// Applies subsystem-path scoping (issue #198), then deterministic ranking, then
/// the top-N cap to a raw code-hit match set.
///
/// Scoping runs BEFORE truncation so in-subsystem hits are never starved by
/// higher-ranked out-of-subsystem hits (AC4): an agent asking for the top-N in a
/// subsystem gets the N best in-subsystem hits, not N global hits filtered down
/// to fewer. `under` is the already-validated, trailing-slash-normalized prefix;
/// `None` leaves the set unscoped. Prefix matching reuses the #83 segment-aware
/// [`crate::query::path_is_under_prefix`] matcher, so `src/alpha` matches
/// `src/alpha/foo.rs` but never `src/alphabet/x.rs`. Ranking is canonical (score
/// descending, then record ID ascending) so repeated runs over an unchanged
/// store emit byte-identical output (AC7).
#[cfg(feature = "embeddings")]
pub(crate) fn scope_and_rank_semantic_matches(
    matches: &mut Vec<SemanticMatch>,
    under: Option<&str>,
    limit: usize,
) {
    if let Some(prefix) = under {
        matches.retain(|m| {
            m.repo_relative_path
                .as_deref()
                .is_some_and(|p| crate::query::path_is_under_prefix(p, prefix))
        });
    }
    matches.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    matches.truncate(limit);
}

/// Which empty-result outcome `eg query semantic` reports when the final match
/// set is empty (issue #198).
///
/// The command must tell "a valid `--under` (or `--repo`) scope selected nothing
/// from a store that DOES carry a semantic index" apart from "the store has no
/// semantic index at all", and the two are worded distinctly
/// (`docs/cli/query.md`) so an agent can tell "nothing under this prefix" from
/// "this store has no embeddings".
#[cfg(feature = "embeddings")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EmptySemanticOutcome {
    /// A valid `--under` prefix left no in-scope hit even though the store
    /// carries a live semantic index — the distinct scoped exit-2 outcome.
    ScopedNoMatch,
    /// The vector search returned nothing at all — no semantic index/embeddings.
    NoSemanticIndex,
}

/// Classifies the empty-result outcome of a scoped semantic query (issue #198).
///
/// The deciding signal for "the index exists" is `index_has_hits` — whether the
/// raw vector search returned ANY hit BEFORE the code-kind / `--repo` / `--under`
/// filters. A memory-only store, or a selected repo with no embedded code, still
/// has a live index even though those filters empty the set; using the
/// post-filter code-hit count here is the bug this classifier fixes, because it
/// mislabeled an index-present-but-out-of-scope result as "no embeddings".
#[cfg(feature = "embeddings")]
pub(crate) const fn classify_empty_semantic_result(
    under_prefix: Option<&str>,
    index_has_hits: bool,
) -> EmptySemanticOutcome {
    if under_prefix.is_some() && index_has_hits {
        EmptySemanticOutcome::ScopedNoMatch
    } else {
        EmptySemanticOutcome::NoSemanticIndex
    }
}

/// Stable diagnostic code for a store that carries no semantic vector index at
/// all — it was never ingested with `--embed` (issue #104).
///
/// Distinct from [`SEMANTIC_NO_MATCHES_CODE`]: "never embedded" and "embedded
/// but nothing matched" are different operator problems, and an agent must be
/// able to tell them apart. Both keep exit `2`, so the documented exit-code
/// contract is unchanged — the codes ride the stderr line because `eg query`
/// lanes leave stdout empty on exit `2`.
#[cfg(feature = "embeddings")]
pub(crate) const SEMANTIC_INDEX_ABSENT_CODE: &str = "semantic_index_absent";

/// Stable diagnostic code for a store whose vector index exists but returned no
/// match for this query.
#[cfg(feature = "embeddings")]
pub(crate) const SEMANTIC_NO_MATCHES_CODE: &str = "no_semantic_matches";

/// Stable diagnostic code for a valid `--under` scope that selected nothing from
/// a store that does carry a live semantic index (issue #198).
#[cfg(feature = "embeddings")]
pub(crate) const SEMANTIC_SCOPED_NO_MATCH_CODE: &str = "scoped_no_match";

/// Reports the empty-result outcome of `eg query semantic` and exits `2`.
///
/// Shared by the two places an empty result is decided — the pre-search
/// no-vector-index gate (issue #104) and the post-filter empty match set (issue
/// #198) — so both emit exactly the same documented wording for the same
/// classified outcome. `index_absent` distinguishes "this store was never
/// `--embed`ed" from "the index exists but nothing matched"; the two share exit
/// `2` but carry distinct stable codes.
#[cfg(feature = "embeddings")]
fn report_empty_semantic_result(
    under_prefix: Option<&str>,
    index_has_hits: bool,
    index_absent: bool,
) -> ! {
    match classify_empty_semantic_result(under_prefix, index_has_hits) {
        EmptySemanticOutcome::ScopedNoMatch => {
            let prefix = under_prefix.unwrap_or_default();
            eprintln!(
                "{SEMANTIC_SCOPED_NO_MATCH_CODE}: scoped to '{prefix}', no matches — the store has a semantic index but no embedded File/Symbol node falls under this prefix"
            );
        }
        EmptySemanticOutcome::NoSemanticIndex => {
            let code = if index_absent {
                SEMANTIC_INDEX_ABSENT_CODE
            } else {
                SEMANTIC_NO_MATCHES_CODE
            };
            eprintln!(
                "{code}: no results — store may not have embeddings (re-run ingest with --embed)"
            );
        }
    }
    std::process::exit(2);
}

/// Semantic similarity search against an embedded store.
///
/// `under` optionally scopes results to a repo-relative path prefix (issue #198,
/// segment-aware via the #83 matcher). A malformed/empty prefix exits `1` with a
/// machine-readable diagnostic; a valid prefix matching zero embedded nodes exits
/// `2` with a "scoped, no matches" outcome distinct from the "no semantic index"
/// message.
#[cfg(feature = "embeddings")]
pub(crate) fn query_semantic(
    query: &str,
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    under: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // Validate + normalize the scope prefix before any store I/O so a malformed
    // input fails fast with a machine-readable diagnostic (AC5), mirroring the
    // `eg query subsystem` prefix contract. `path_is_under_prefix` trims the
    // trailing slash itself; the empty check is all that remains here.
    let under_prefix = match under {
        Some(raw) => {
            let normalized = raw.trim_end_matches('/');
            if normalized.is_empty() {
                let envelope = serde_json::json!({
                    "ok": false,
                    "error": {
                        "code": "malformed_under_prefix",
                        "under": raw,
                        "message": "--under prefix must be non-empty after stripping trailing slashes"
                    }
                });
                println!("{}", serde_json::to_string(&envelope)?);
                std::process::exit(1);
            }
            Some(normalized)
        }
        None => None,
    };

    validate_existing_embedded_store(data_dir)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    // Repository attribution requires the store topology, not just the vector
    // index: build the index from the full record set so each retrieval lead
    // carries its repository identity handle (issue #67). Resolve the selector
    // before loading the embedding model so a bad `--repo` fails fast.
    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    // Vector-space compatibility gate (issue #104), before the model is loaded.
    // A store with no vector index at all is not an identity failure: report the
    // documented no-embeddings outcome here rather than letting the vector search
    // below surface an opaque engine error at exit 1.
    if enforce_index_compatibility(&sink, &records)?
        == crate::embeddings::IndexCompatibility::IndexAbsent
    {
        report_empty_semantic_result(under_prefix, false, true);
    }

    let query_vector = embed_query_checked(query, &sink, &records)?;

    // Over-fetch the whole index, not just `limit` raw hits: the shared vector
    // index now also embeds agent-memory nodes (issue #91), so a query whose top
    // `limit` raw matches are memory would otherwise drop them all and never see
    // the code hits ranked just behind them. Fetching the full pool lets the
    // code-kind filter below recover those code hits; the limit then bounds the
    // filtered result set. Scoping needs the full pool for the same reason.
    let fetch = records.len().max(limit);
    let mut matches = sink
        .semantic_search(&query_vector, fetch)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;

    // The store carries a live semantic index iff the raw vector search returned
    // at least one hit — captured BEFORE the code-kind / `--repo` / `--under`
    // filters. A memory-only store, or a selected repo with no embedded code,
    // still has an index even though the filters below empty the set; keying the
    // scoped-vs-no-index distinction on this raw signal (not the post-filter code
    // hits) is what makes a valid `--under` over such a store report the distinct
    // "scoped, no matches" outcome instead of the "no embeddings" message (#198).
    let index_has_hits = !matches.is_empty();

    // Code search must never blend agent-authored memory hits into deterministic
    // code results (issue #91): the shared vector index now also embeds
    // observation-class memory nodes, recalled only via `eg query semantic-memory`.
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    if let Some(repo) = selected.as_deref() {
        matches.retain(|m| index.owner_of(&m.record_id) == Some(repo));
    }

    // Subsystem scoping (issue #198) is applied to the full candidate pool BEFORE
    // the top-N cap (AC4); the same helper also imposes canonical ordering (AC7).
    scope_and_rank_semantic_matches(&mut matches, under_prefix, limit);

    if matches.is_empty() {
        report_empty_semantic_result(under_prefix, index_has_hits, false);
    }

    for m in &matches {
        print_result(&SemanticResult::from_match(m, &index), format)?;
    }
    Ok(())
}

/// One agent-authored memory record recalled by meaning (issue #91).
///
/// Typed `agent_authored` so a consuming agent can never mistake a recalled
/// lesson for deterministic source truth. Every emitted row carries a citable
/// `source_handle`; a hit lacking provenance is excluded upstream, never
/// returned with empty provenance.
#[cfg(feature = "embeddings")]
#[derive(Serialize)]
pub(crate) struct MemoryRecallResult<'a> {
    record_id: &'a str,
    kind: &'static str,
    trust_class: &'static str,
    retrieval_score: f32,
    /// Citable source transcript / session / turn handle proving where the
    /// memory came from.
    source_handle: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confidence: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    observed_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ingested_at: Option<&'a str>,
    /// `verified` when the claim cites present verification evidence, else
    /// `unverified` — a structural, non-inferential trust signal (issue #64).
    review_state: &'static str,
    redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<&'a str>,
    /// Resolved code handles this memory cites (`OBSERVES`/`MENTIONS_SYMBOL`/…).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    linked_code_handles: Vec<String>,
    /// The recalled memory body (post-redaction stored text).
    memory_text: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temporal_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by_records: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

#[cfg(feature = "embeddings")]
impl PrintText for MemoryRecallResult<'_> {
    fn as_text(&self) -> String {
        format!(
            "{} [{}] {} score={:.4} author={} source={} review={}\n  {}",
            self.record_id,
            self.kind,
            self.trust_class,
            self.retrieval_score,
            self.agent_id.unwrap_or("(unknown)"),
            self.source_handle,
            self.review_state,
            self.memory_text,
        )
    }
}

/// Returns a trimmed, non-empty string slice, or `None` for a missing or
/// blank-only value. Used so an imported memory record carrying
/// `source_handle: ""` is treated as having no provenance rather than passing
/// the recall gate and being emitted with an empty handle (issue #91).
#[cfg(feature = "embeddings")]
pub(crate) fn non_empty(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|s| !s.trim().is_empty())
}

/// Resolves the repositories a memory record belongs to (issue #91).
///
/// Agent-memory nodes are not part of the code-graph containment topology, so
/// [`query::RepositoryIndex::owner_of`] returns `None` for them directly. A
/// memory record is attributed to a repository through the code it cites: any
/// cited code target that resolves to a repository-owned node scopes the memory
/// to that repository. Both citation shapes are honored — inline
/// `evidence_links` and standalone outgoing `GraphRecord::Edge` records (e.g.
/// the `link-evidence` `MENTIONS_SYMBOL` / `FAILED_ON` / `TOUCHED_FILE` edges) —
/// so imported memory that stores normalized edges is not dropped under `--repo`.
/// Returned sorted and deduplicated for deterministic selection.
#[cfg(feature = "embeddings")]
pub(crate) fn memory_repo_owners<'a>(
    record_id: &str,
    links: Option<&Vec<EvidenceLink>>,
    edges_from: &query::OutgoingEdgeIndex<'_>,
    index: &'a query::RepositoryIndex,
) -> Vec<&'a str> {
    if let Some(owner) = index.owner_of(record_id) {
        return vec![owner];
    }
    let mut owners: Vec<&str> = Vec::new();
    if let Some(links) = links {
        owners.extend(
            links
                .iter()
                .filter_map(|l| l.target_record_id.as_deref())
                .filter_map(|target| index.owner_of(target)),
        );
    }
    if let Some(out) = edges_from.get(record_id) {
        owners.extend(out.iter().filter_map(|(_, target)| index.owner_of(target)));
    }
    owners.sort_unstable();
    owners.dedup();
    owners
}

/// Resolves one evidence link to a citable code handle string when it points at
/// the code-graph domain.
#[cfg(feature = "embeddings")]
pub(crate) fn code_handle_from_link(
    link: &EvidenceLink,
    by_id: &BTreeMap<&str, &GraphRecord>,
) -> Option<String> {
    let is_code = link.target_domain == "codegraph"
        || matches!(
            link.relation.as_str(),
            "OBSERVES" | "MENTIONS_SYMBOL" | "TOUCHED_FILE"
        );
    if !is_code {
        return None;
    }
    if let Some(target_id) = link.target_record_id.as_deref()
        && let Some(GraphRecord::Node {
            repo_relative_path,
            name,
            ..
        }) = by_id.get(target_id).copied()
    {
        if let Some(path) = repo_relative_path {
            return Some(
                name.as_ref()
                    .map_or_else(|| path.clone(), |n| format!("{path}::{n}")),
            );
        }
        return Some(target_id.to_owned());
    }
    link.target_repo_relative_path
        .clone()
        .or_else(|| link.target_record_id.clone())
}

/// Decides whether a semantic hit is a recallable agent-memory record (issue #91).
///
/// A hit qualifies only when it is an agent-memory observation-class kind, can
/// cite where it came from (a `source_handle`, source artifact path, or session
/// handle), and — under `verified_only` — cites present verification evidence.
/// A hit lacking provenance is rejected here so it is excluded, never returned.
#[cfg(feature = "embeddings")]
pub(crate) fn is_recallable_memory(
    m: &SemanticMatch,
    by_id: &BTreeMap<&str, &GraphRecord>,
    edges_from: &query::OutgoingEdgeIndex<'_>,
    tombstoned: &query::TombstonedSet<'_>,
    verified_only: bool,
) -> bool {
    if !m
        .kind
        .as_deref()
        .is_some_and(|k| matches!(k, "Observation" | "Decision" | "Failure"))
    {
        return false;
    }
    let Some(record) = by_id.get(m.record_id.as_str()).copied() else {
        return false;
    };
    let GraphRecord::Node {
        session_id,
        source_handle,
        source_artifact_path,
        ..
    } = record
    else {
        return false;
    };
    // Provenance must be a present, non-blank handle: a record carrying only
    // empty strings is excluded, never emitted with an empty `source_handle`.
    let has_provenance = non_empty(source_handle.as_ref()).is_some()
        || non_empty(source_artifact_path.as_ref()).is_some()
        || non_empty(session_id.as_ref()).is_some();
    if !has_provenance {
        return false;
    }
    // Verified-only reuses the memory-audit structural rule (issue #64): a
    // resolvable, non-tombstoned verification record cited via VALIDATED_BY /
    // HAS_EVIDENCE / PRODUCED_EVIDENCE, on either an inline evidence link or an
    // outgoing edge. A triple-only citation stub never counts as verified.
    if verified_only && !query::is_verified_claim(record, by_id, edges_from, tombstoned) {
        return false;
    }
    true
}

/// Recalls prior agent memory by meaning, trust-separated from code (issue #91).
///
/// Embeds the natural-language query with the local model, runs the same vector
/// search the code path uses, then keeps only agent-memory observation-class
/// hits — each enriched with its provenance handle. A hit that cannot cite
/// where it came from is excluded, not returned. With `--verified-only`,
/// observations lacking cited verification evidence are excluded too.
#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
#[derive(Serialize)]
pub(crate) struct ExcludedRecallDiagnostic<'a> {
    record_id: &'a str,
    reason: &'static str,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    superseded_by: Option<Vec<crate::temporal_status::TemporalReference>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    contradicted_by: Option<Vec<crate::temporal_status::TemporalReference>>,
}

#[cfg(feature = "embeddings")]
impl PrintText for ExcludedRecallDiagnostic<'_> {
    fn as_text(&self) -> String {
        format!("Excluded record {} due to: {}", self.record_id, self.reason)
    }
}

#[cfg(feature = "embeddings")]
#[allow(clippy::too_many_lines)]
pub(crate) fn query_semantic_memory(
    query: &str,
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    verified_only: bool,
    format: OutputFormat,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    let by_id: BTreeMap<&str, &GraphRecord> = records.iter().map(|r| (r.id(), r)).collect();
    let (edges_from, tombstoned) = query::verification_support_indexes(&records);
    let resolver = crate::temporal_status::TemporalResolver::build(&records);
    let mut excluded_recall_diagnostics = Vec::new();

    // Memory recall reads the SAME shared vector index code search does, so it
    // carries the same cross-vector-space hazard and the same gate (issue #104).
    if enforce_index_compatibility(&sink, &records)?
        == crate::embeddings::IndexCompatibility::IndexAbsent
    {
        eprintln!(
            "no memory results — store may lack embedded memory (re-run ingest with --embed) or all hits were filtered"
        );
        std::process::exit(2);
    }

    let query_vector = embed_query_checked(query, &sink, &records)?;

    // The shared vector index holds both code and memory; fetch a generous pool
    // and filter to memory so the `limit` bounds recalled memory, not the blend.
    let fetch = records.len().max(limit);
    let matches = sink
        .semantic_search(&query_vector, fetch)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;

    let mut rows: Vec<MemoryRecallResult> = Vec::new();
    for m in &matches {
        // Trust separation + provenance exclusion (AC3): keep only agent-memory
        // observation-class hits that can cite where they came from.
        if !is_recallable_memory(m, &by_id, &edges_from, &tombstoned, verified_only) {
            continue;
        }
        let Some(record) = by_id.get(m.record_id.as_str()).copied() else {
            continue;
        };
        let GraphRecord::Node {
            text,
            summary,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            source_handle,
            source_artifact_path,
            redaction_policy_version,
            superseded_by,
            evidence_links,
            ..
        } = record
        else {
            continue;
        };

        // Scope through the code this memory cites: memory nodes are not in the
        // containment topology, so a `--repo` filter must resolve the repository
        // from the linked code handles (inline links and outgoing edges), not the
        // memory record ID directly.
        let owners = memory_repo_owners(&m.record_id, evidence_links.as_ref(), &edges_from, &index);
        if let Some(repo) = selected.as_deref()
            && !owners.contains(&repo)
        {
            continue;
        }

        // `is_recallable_memory` guarantees a present, non-blank handle; pick the
        // first non-empty among source handle, artifact path, and session ID.
        let source_handle_value = non_empty(source_handle.as_ref())
            .or_else(|| non_empty(source_artifact_path.as_ref()))
            .or_else(|| non_empty(session_id.as_ref()))
            .unwrap_or_default()
            .to_owned();

        let verified = query::is_verified_claim(record, &by_id, &edges_from, &tombstoned);

        let linked_code_handles: Vec<String> = evidence_links
            .as_ref()
            .map(|links| {
                let mut handles: Vec<String> = links
                    .iter()
                    .filter_map(|l| code_handle_from_link(l, &by_id))
                    .collect();
                handles.sort();
                handles.dedup();
                handles
            })
            .unwrap_or_default();

        // Label with the selected repository when scoped (the membership filter
        // above guarantees it is among `owners`), so a memory citing code in
        // several repositories is never misattributed to a different one than the
        // user selected; otherwise fall back to the first owner deterministically.
        let (status, superseded_by_refs, contradicted_by_refs) =
            resolver.resolve_status(record.id());
        let is_superseded = status == "superseded" || status == "cycle";
        let is_contradicted = status == "contradicted";

        if is_superseded || is_contradicted {
            let reason = if is_superseded {
                "superseded"
            } else {
                "contradicted"
            };
            match supersession {
                crate::temporal_status::SupersessionMode::Exclude => {
                    excluded_recall_diagnostics.push(ExcludedRecallDiagnostic {
                        record_id: record.id(),
                        reason,
                        status: "excluded",
                        superseded_by: if superseded_by_refs.is_empty() {
                            None
                        } else {
                            Some(superseded_by_refs)
                        },
                        contradicted_by: if contradicted_by_refs.is_empty() {
                            None
                        } else {
                            Some(contradicted_by_refs)
                        },
                    });
                }
                crate::temporal_status::SupersessionMode::IncludeButFlag => {
                    let repository_id = selected.as_deref().or_else(|| owners.first().copied());
                    rows.push(MemoryRecallResult {
                        record_id: record.id(),
                        kind: record.node_kind_name().unwrap_or("Observation"),
                        trust_class: "agent_authored",
                        retrieval_score: m.score,
                        source_handle: source_handle_value,
                        agent_id: agent_id.as_deref(),
                        agent_kind: agent_kind.as_deref(),
                        session_id: session_id.as_deref(),
                        confidence: confidence.as_deref(),
                        observed_at: observed_at.as_deref(),
                        ingested_at: ingested_at.as_deref(),
                        review_state: if verified { "verified" } else { "unverified" },
                        redacted: redaction_policy_version.is_some(),
                        superseded_by: superseded_by.as_deref(),
                        linked_code_handles,
                        memory_text: text.as_deref().unwrap_or(summary.as_str()),
                        repository_id,
                        repository: repository_id.and_then(|id| index.display_of(id)),
                        temporal_status: Some(status.to_string()),
                        superseded_by_records: if superseded_by_refs.is_empty() {
                            None
                        } else {
                            Some(superseded_by_refs)
                        },
                        contradicted_by: if contradicted_by_refs.is_empty() {
                            None
                        } else {
                            Some(contradicted_by_refs)
                        },
                    });
                }
            }
        } else {
            let repository_id = selected.as_deref().or_else(|| owners.first().copied());
            rows.push(MemoryRecallResult {
                record_id: record.id(),
                kind: record.node_kind_name().unwrap_or("Observation"),
                trust_class: "agent_authored",
                retrieval_score: m.score,
                source_handle: source_handle_value,
                agent_id: agent_id.as_deref(),
                agent_kind: agent_kind.as_deref(),
                session_id: session_id.as_deref(),
                confidence: confidence.as_deref(),
                observed_at: observed_at.as_deref(),
                ingested_at: ingested_at.as_deref(),
                review_state: if verified { "verified" } else { "unverified" },
                redacted: redaction_policy_version.is_some(),
                superseded_by: superseded_by.as_deref(),
                linked_code_handles,
                memory_text: text.as_deref().unwrap_or(summary.as_str()),
                repository_id,
                repository: repository_id.and_then(|id| index.display_of(id)),
                temporal_status: match supersession {
                    crate::temporal_status::SupersessionMode::IncludeButFlag => {
                        Some(status.to_string())
                    }
                    crate::temporal_status::SupersessionMode::Exclude => None,
                },
                superseded_by_records: None,
                contradicted_by: None,
            });
        }
    }

    // Canonical ordering before truncation (AC7): equal-score ANN results can be
    // returned in arbitrary order, so sort by score descending then record ID
    // ascending so repeated runs print byte-identical output and the row chosen
    // at the `limit` boundary is stable.
    rows.sort_by(|a, b| {
        b.retrieval_score
            .total_cmp(&a.retrieval_score)
            .then_with(|| a.record_id.cmp(b.record_id))
    });
    rows.truncate(limit);

    if rows.is_empty() {
        eprintln!(
            "no memory results — store may lack embedded memory (re-run ingest with --embed) or all hits were filtered"
        );
        std::process::exit(2);
    }

    for row in &rows {
        print_result(row, format)?;
    }

    for diag in &excluded_recall_diagnostics {
        print_result(diag, format)?;
    }
    Ok(())
}

/// Semantic similarity search routed through the running daemon (issue #59).
///
/// Connects to the daemon first (so a missing or stale daemon fails fast,
/// before the model is loaded), embeds the query locally, then dispatches the
/// `semantic_search` verb. Results are the same retrieval-lead rows the
/// embedded path emits; the daemon owns the shared store, token, and snapshot.
#[cfg(feature = "embeddings")]
pub(crate) fn query_semantic_via_daemon(
    query: &str,
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let client = DaemonClient::from_data_dir(data_dir)
        .with_context(|| format!("failed to connect to daemon at {}", data_dir.display()))?;

    // The daemon owns the store, so the local lane cannot inspect its vector
    // index; wiring the compatibility gate into the daemon verb is #59/#53's
    // scope, not this slice's. Disclose the gap rather than let an agent that
    // fell back to `--daemon` silently receive an answer the local lane would
    // have refused (issue #104).
    eprintln!(
        "note: --daemon does not apply the embedding-model compatibility gate (issue #104); \
         results are not verified to share the index's vector space — re-run without --daemon \
         for a verified answer"
    );

    let query_vector = embed_query_text(query)?;
    let mut params = serde_json::json!({
        "query_vector": query_vector,
        "limit": limit as u64,
    });
    if let Some(repo) = repo {
        params["repo"] = serde_json::json!(repo);
    }
    let records = client
        .query_verb("semantic_search", &params, None)
        .map_err(|e| surface_daemon_selector_rejection(e, repo))?;

    if records.is_empty() {
        eprintln!("no results — store may not have embeddings (re-run ingest with --embed)");
        std::process::exit(2);
    }

    for rec in &records {
        print_daemon_semantic_record(rec, format)?;
    }
    Ok(())
}

/// Prints a daemon semantic result row (`serde_json::Value`) in the requested
/// format. JSON output forwards the row verbatim; text output renders the
/// bounded handle fields only.
#[cfg(feature = "embeddings")]
pub(crate) fn print_daemon_semantic_record(
    rec: &serde_json::Value,
    format: OutputFormat,
) -> Result<()> {
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(rec)?),
        OutputFormat::Text => {
            let record_id = rec["record_id"].as_str().unwrap_or("(unknown)");
            let score = rec["score"].as_f64().unwrap_or(0.0);
            let path = rec["repo_relative_path"].as_str().unwrap_or("(unknown)");
            let line = rec["span"]["start_line"].as_u64();
            let location = line.map_or_else(
                || path.to_owned(),
                |start_line| format!("{path}:{start_line}"),
            );
            println!("{record_id} score={score:.4} @ {location}");
        }
    }
    Ok(())
}

/// Emits the `eg query semantic-context` no-match envelope and exits `2`.
///
/// Shared by the two places a no-match is decided — the pre-search
/// no-vector-index gate (issue #104) and an all-below-`min_score` bundle (issue
/// #90) — so both emit exactly the same documented envelope.
///
/// The `Result` return is a serialization-failure channel only; on success this
/// never returns.
#[cfg(feature = "embeddings")]
fn report_semantic_context_no_match(
    query: &str,
    min_score: f32,
) -> Result<std::convert::Infallible> {
    let envelope = serde_json::json!({
        "ok": false,
        "error": {
            "code": "no_match",
            "query": query,
            "min_score": min_score,
        }
    });
    println!("{}", serde_json::to_string(&envelope)?);
    std::process::exit(2);
}

/// Natural-language query → evidence-backed context for the top-N semantic
/// matches, in a single read-only call (issue #90).
///
/// Embeds the query locally, ranks matches against the embedded store, then —
/// for each match clearing `min_score` — resolves the same trust-separated
/// context sections as `eg query context`, anchored on the match's record ID so
/// File-typed matches are first-class. A no-match (no hit clears the floor)
/// emits a stable diagnostic to stdout and exits 2.
#[cfg(feature = "embeddings")]
pub(crate) fn query_semantic_context(
    query: &str,
    data_dir: &Path,
    limit: usize,
    min_score: f32,
    repo: Option<&str>,
    supersession: crate::temporal_status::SupersessionMode,
) -> Result<()> {
    validate_existing_embedded_store(data_dir)?;

    let sink = EmbeddedAletheiaSink::open_unleased(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;

    let records = sink
        .read_all_records()
        .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
    let index = query::RepositoryIndex::build(&records);
    let selected = resolve_repo_scope(&index, repo);

    // Same shared vector index, same gate (issue #104).
    if enforce_index_compatibility(&sink, &records)?
        == crate::embeddings::IndexCompatibility::IndexAbsent
    {
        report_semantic_context_no_match(query, min_score)?;
    }

    let query_vector = embed_query_checked(query, &sink, &records)?;

    // Over-fetch the whole index, not just `limit` raw hits: the shared vector
    // index also embeds agent-memory nodes (issue #91), so a query whose top
    // `limit` raw matches are memory would otherwise drop the code hits ranked
    // just behind them. Fetch the full pool so the code-kind filter below
    // recovers those code hits; the limit then bounds the filtered set.
    let fetch = records.len().max(limit);
    let mut matches = sink
        .semantic_search(&query_vector, fetch)
        .with_context(|| "semantic search failed — was the store ingested with --embed?")?;
    // `semantic-context` is a code-context bridge: never expand agent-authored
    // memory hits (issue #91). Mirror `query semantic` and keep only
    // deterministic code kinds before building leads.
    matches.retain(|m| {
        m.kind
            .as_deref()
            .is_some_and(|k| k == "File" || k == "Symbol")
    });
    if let Some(repo) = selected.as_deref() {
        matches.retain(|m| index.owner_of(&m.record_id) == Some(repo));
    }
    // Canonical ordering before truncation: equal-score ANN results can be
    // returned in arbitrary order, so sort by score descending then record ID
    // ascending so repeated runs choose the same rows at the `limit` boundary
    // and emit byte-identical output.
    matches.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.record_id.cmp(&b.record_id))
    });
    matches.truncate(limit);

    let leads: Vec<query::SemanticLead> = matches
        .iter()
        .map(|m| query::SemanticLead {
            record_id: m.record_id.clone(),
            name: m.name.clone(),
            repo_relative_path: m.repo_relative_path.clone(),
            score: m.score,
            span: m.span,
        })
        .collect();

    // Scope the record slice for context expansion when a repo is selected so
    // that ambiguity detection (candidate_record_ids) and the path-based file
    // fallback in record_context don't return IDs from other repos. Cross-
    // domain records (observations, artifacts, verification) are unowned and
    // always kept so that context sections remain fully populated.
    let records: Vec<GraphRecord> = if let Some(repo) = selected.as_deref() {
        records
            .into_iter()
            .filter(|r| index.owner_of(r.id()).is_none_or(|o| o == repo))
            .collect()
    } else {
        records
    };

    let bundle = query::semantic_context_bundle(&records, &leads, min_score);
    let resolver = crate::temporal_status::TemporalResolver::build(&records);

    if bundle.is_no_match() {
        report_semantic_context_no_match(query, min_score)?;
    }

    // Derived trust labels (issue #114): built ONCE over the record slice and
    // shared by every retrieval lead, so per-match rows cannot disagree.
    let trust_ctx = query::TrustContext::build(&records);
    let match_rows: Vec<SemanticContextMatch<'_>> = bundle
        .matches
        .iter()
        .map(|m| {
            let sections = build_context_sections(&m.context, &trust_ctx);
            let (observations, excluded) =
                apply_supersession(sections.observations, &resolver, supersession);
            let repository_id = index.owner_of(&m.lead.record_id);
            SemanticContextMatch {
                record_id: &m.lead.record_id,
                name: m.lead.name.as_deref(),
                repo_relative_path: m.lead.repo_relative_path.as_deref(),
                span: m.lead.span,
                score: m.lead.score,
                match_kind: m.anchor_kind.as_str(),
                repository_id,
                repository: repository_id.and_then(|id| index.display_of(id)),
                ambiguous: !m.candidate_record_ids.is_empty(),
                candidate_record_ids: m.candidate_record_ids.iter().map(String::as_str).collect(),
                source_facts: sections.source_facts,
                topology_edges: sections.topology_edges,
                observations,
                project_state: sections.project_state,
                artifacts: sections.artifacts,
                verification_evidence: sections.verification_evidence,
                unresolved: sections.unresolved,
                excluded,
            }
        })
        .collect();

    let response = SemanticContextResponse {
        ok: true,
        query,
        min_score,
        matches: match_rows,
    };

    let output =
        serde_json::to_string_pretty(&response).context("failed to serialize semantic context")?;
    println!("{output}");
    Ok(())
}

#[cfg(feature = "embeddings")]
impl PrintText for SemanticResult<'_> {
    fn as_text(&self) -> String {
        let name = self.name.unwrap_or("(unknown)");
        let path = self.repo_relative_path.unwrap_or("(unknown)");
        let line = self.span.map_or(0, |s| s.start_line);
        format!("{name} score={:.4} @ {path}:{line}", self.score)
    }
}

// -----------------------------------------------------------------------------------------------------------
// AC7: Semantic query JSON output contract conformance
//
// This test module locks the stable field names for `eg query semantic --format
// json`. If any field is removed or renamed without updating this test (and the
// docs in docs/cli/query.md), the test suite will fail during CI.
// ---------------------------------------------------------------------------
// -----------------------------------------------------------------------------------------------------------
// Issue #198: subsystem-path-scoped semantic search (`--under <prefix>`).
//
// The scope filter must be applied to the raw candidate set BEFORE the top-N
// truncation so in-subsystem hits are never starved by higher-ranked global
// hits, and must reuse the #83 segment-aware prefix matcher so `src/alpha`
// never bleeds into `src/alphabet`. These tests exercise the pure
// scope+rank+truncate helper with a deterministic fixed match set — no
// embedding model required.
// ---------------------------------------------------------------------------
#[cfg(all(test, feature = "embeddings"))]
mod scoped_semantic {
    use super::*;

    fn match_at(record_id: &str, path: &str, score: f32) -> SemanticMatch {
        SemanticMatch {
            record_id: record_id.to_owned(),
            kind: Some("File".to_owned()),
            name: Some(record_id.to_owned()),
            repo_relative_path: Some(path.to_owned()),
            score,
            span: None,
        }
    }

    /// A scoped query returns only hits under the prefix; sibling subsystems
    /// never leak (AC2).
    #[test]
    fn scope_filters_to_prefix_and_excludes_siblings() {
        let mut matches = vec![
            match_at("a", "src/alpha/one.rs", 0.9),
            match_at("b", "src/beta/two.rs", 0.8),
            match_at("c", "src/alpha/three.rs", 0.7),
        ];
        scope_and_rank_semantic_matches(&mut matches, Some("src/alpha"), 10);
        let paths: Vec<&str> = matches
            .iter()
            .map(|m| m.repo_relative_path.as_deref().unwrap())
            .collect();
        assert_eq!(paths, vec!["src/alpha/one.rs", "src/alpha/three.rs"]);
    }

    /// Prefix matching is path-segment aware: `src/alpha` never matches
    /// `src/alphabet` (AC3, reusing the #83 matcher).
    #[test]
    fn scope_is_segment_aware() {
        let mut matches = vec![
            match_at("a", "src/alpha/foo.rs", 0.9),
            match_at("b", "src/alphabet/bar.rs", 0.8),
        ];
        scope_and_rank_semantic_matches(&mut matches, Some("src/alpha"), 10);
        let paths: Vec<&str> = matches
            .iter()
            .map(|m| m.repo_relative_path.as_deref().unwrap())
            .collect();
        assert_eq!(paths, vec!["src/alpha/foo.rs"]);
    }

    /// The trailing-slash and bare forms resolve identically (AC3).
    #[test]
    fn trailing_slash_and_bare_forms_identical() {
        let fixture = vec![
            match_at("a", "src/alpha/foo.rs", 0.9),
            match_at("b", "src/alphabet/bar.rs", 0.8),
        ];
        let mut bare = fixture.clone();
        let mut slashed = fixture;
        scope_and_rank_semantic_matches(&mut bare, Some("src/alpha"), 10);
        scope_and_rank_semantic_matches(&mut slashed, Some("src/alpha/"), 10);
        let ids = |v: &[SemanticMatch]| v.iter().map(|m| m.record_id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&bare), ids(&slashed));
        assert_eq!(ids(&bare), vec!["a".to_owned()]);
    }

    /// Scoping runs BEFORE truncation: an agent asking for the top-3 in a
    /// subsystem gets the 3 best in-subsystem hits, not 3 global hits filtered
    /// down to zero (AC4). Here the 5 highest-scored hits are all out of scope;
    /// without before-truncation ordering the result would be empty.
    #[test]
    fn scope_applies_before_truncation() {
        let mut matches = vec![
            match_at("beta1", "src/beta/a.rs", 0.99),
            match_at("beta2", "src/beta/b.rs", 0.98),
            match_at("beta3", "src/beta/c.rs", 0.97),
            match_at("beta4", "src/beta/d.rs", 0.96),
            match_at("beta5", "src/beta/e.rs", 0.95),
            match_at("alpha1", "src/alpha/a.rs", 0.50),
            match_at("alpha2", "src/alpha/b.rs", 0.40),
            match_at("alpha3", "src/alpha/c.rs", 0.30),
            match_at("alpha4", "src/alpha/d.rs", 0.20),
        ];
        scope_and_rank_semantic_matches(&mut matches, Some("src/alpha"), 3);
        let ids: Vec<&str> = matches.iter().map(|m| m.record_id.as_str()).collect();
        assert_eq!(ids, vec!["alpha1", "alpha2", "alpha3"]);
    }

    /// Output ordering is deterministic: equal scores break ties on record ID
    /// so repeated runs over an unchanged store are byte-identical (AC7).
    #[test]
    fn ordering_is_deterministic_on_ties() {
        let mut matches = vec![
            match_at("zzz", "src/alpha/z.rs", 0.5),
            match_at("aaa", "src/alpha/a.rs", 0.5),
            match_at("mmm", "src/alpha/m.rs", 0.5),
        ];
        scope_and_rank_semantic_matches(&mut matches, Some("src/alpha"), 10);
        let ids: Vec<&str> = matches.iter().map(|m| m.record_id.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "mmm", "zzz"]);
    }

    /// A valid `--under` prefix over a store that has a live semantic index but
    /// whose retrieved pool yields zero in-scope code hits (memory-only store, or
    /// a selected repo with no embedded code) must report the DISTINCT scoped
    /// no-match outcome — never the "no embeddings" message. Regression test for
    /// the Codex #415 finding: the outcome must be independent of whether any
    /// post-filter CODE hit survived, keyed only on the raw index having hits.
    #[test]
    fn empty_scoped_result_over_indexed_store_is_scoped_no_match() {
        assert_eq!(
            classify_empty_semantic_result(Some("src/alpha"), true),
            EmptySemanticOutcome::ScopedNoMatch
        );
    }

    /// A valid `--under` prefix over a store whose raw vector search returned
    /// nothing at all is a genuine no-index outcome, not a scoped no-match.
    #[test]
    fn empty_result_over_unindexed_store_is_no_semantic_index() {
        assert_eq!(
            classify_empty_semantic_result(Some("src/alpha"), false),
            EmptySemanticOutcome::NoSemanticIndex
        );
    }

    /// Without `--under` there is no scope, so an empty result is always the
    /// no-index message regardless of whether the raw index had hits.
    #[test]
    fn unscoped_empty_result_is_never_scoped_no_match() {
        assert_eq!(
            classify_empty_semantic_result(None, true),
            EmptySemanticOutcome::NoSemanticIndex
        );
        assert_eq!(
            classify_empty_semantic_result(None, false),
            EmptySemanticOutcome::NoSemanticIndex
        );
    }

    /// `None` scope leaves the candidate set unscoped (only ranked + capped),
    /// so the unscoped code path is unchanged.
    #[test]
    fn unscoped_keeps_all_kinds() {
        let mut matches = vec![
            match_at("a", "src/alpha/one.rs", 0.9),
            match_at("b", "src/beta/two.rs", 0.8),
        ];
        scope_and_rank_semantic_matches(&mut matches, None, 10);
        assert_eq!(matches.len(), 2);
    }
}

#[cfg(all(test, feature = "embeddings"))]
mod semantic_contract {
    use super::*;

    const fn full_span() -> SourceSpan {
        SourceSpan {
            start_byte: 4096,
            end_byte: 5200,
            start_line: 142,
            end_line: 168,
        }
    }

    /// All stable fields present — verifies required and optional contract fields.
    #[test]
    fn semantic_result_json_contract_all_stable_fields_present() {
        let result = SemanticResult {
            record_id: "codegraph:v1:abc123",
            name: Some("EmbeddedAletheiaSink::write_record"),
            repo_relative_path: Some("src/sink/embedded.rs"),
            score: 0.9231_f32,
            span: Some(full_span()),
            repository_id: Some("codegraph:v1:repo"),
            repository: Some("acme/widget"),
        };
        let json =
            serde_json::to_value(&result).expect("SemanticResult must serialize to JSON value");

        // Required stable fields — test fails if either is removed or renamed.
        assert!(
            json.get("record_id").is_some(),
            "stable contract field 'record_id' must be present in JSON output"
        );
        assert!(
            json.get("score").is_some(),
            "stable contract field 'score' must be present in JSON output"
        );

        // Optional stable fields — must appear in the JSON when the field is populated.
        assert!(
            json.get("name").is_some(),
            "optional contract field 'name' must appear in JSON when populated"
        );
        assert!(
            json.get("repo_relative_path").is_some(),
            "optional contract field 'repo_relative_path' must appear in JSON when populated"
        );
        assert!(
            json.get("span").is_some(),
            "optional contract field 'span' must appear in JSON when populated"
        );

        // span sub-fields are part of the stable contract.
        let span = &json["span"];
        for sub in ["start_byte", "end_byte", "start_line", "end_line"] {
            assert!(
                span.get(sub).is_some(),
                "span.{sub} is a stable contract sub-field and must be present"
            );
        }
    }

    /// Optional fields absent when None — verifies `skip_serializing_if` contract.
    #[test]
    fn semantic_result_json_contract_optional_fields_omitted_when_none() {
        let result = SemanticResult {
            record_id: "codegraph:v1:abc123",
            name: None,
            repo_relative_path: None,
            score: 0.42_f32,
            span: None,
            repository_id: None,
            repository: None,
        };
        let json = serde_json::to_value(&result).expect("serialize");

        assert!(json.get("record_id").is_some(), "record_id always present");
        assert!(json.get("score").is_some(), "score always present");
        assert!(
            json.get("name").is_none(),
            "contract: 'name' must be absent from JSON when None"
        );
        assert!(
            json.get("repo_relative_path").is_none(),
            "contract: 'repo_relative_path' must be absent from JSON when None"
        );
        assert!(
            json.get("span").is_none(),
            "contract: 'span' must be absent from JSON when None"
        );
    }
}
