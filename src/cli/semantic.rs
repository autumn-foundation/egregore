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

/// Semantic similarity search against an embedded store.
#[cfg(feature = "embeddings")]
pub(crate) fn query_semantic(
    query: &str,
    data_dir: &Path,
    limit: usize,
    repo: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
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

    let query_vector = embed_query_text(query)?;

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
    matches.truncate(limit);

    if matches.is_empty() {
        eprintln!("no results — store may not have embeddings (re-run ingest with --embed)");
        std::process::exit(2);
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

    let query_vector = embed_query_text(query)?;

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

    let query_vector = embed_query_text(query)?;

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

    let match_rows: Vec<SemanticContextMatch<'_>> = bundle
        .matches
        .iter()
        .map(|m| {
            let sections = build_context_sections(&m.context);
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
