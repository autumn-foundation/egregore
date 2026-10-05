use super::*;

/// Stable pre-commit refusal for `eg re-embed`.
///
/// Carries the exit code, the machine-readable error code, and the human
/// message. Emitting it prints the refusal envelope and exits; the store is
/// untouched by construction.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
#[derive(Debug)]
struct ReembedHalt {
    code: i32,
    error_code: &'static str,
    message: String,
    extra: serde_json::Value,
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
impl std::fmt::Display for ReembedHalt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_code, self.message)
    }
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
impl std::error::Error for ReembedHalt {}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
impl ReembedHalt {
    /// Prints the refusal (JSON envelope on stdout, or a human line on
    /// stderr) and exits with the stable code. Never returns.
    fn emit(&self, json: bool) -> ! {
        if json {
            if let Some(envelope) = self.extra.get("envelope") {
                println!("{envelope}");
            } else {
                let mut error = serde_json::Map::new();
                error.insert("code".to_owned(), serde_json::json!(self.error_code));
                error.insert("message".to_owned(), serde_json::json!(self.message));
                if let serde_json::Value::Object(fields) = &self.extra {
                    error.extend(fields.clone());
                }
                println!(
                    "{}",
                    serde_json::json!({"ok": false, "error": serde_json::Value::Object(error)})
                );
            }
        } else {
            eprintln!("{}: {}", self.error_code, self.message);
        }
        std::process::exit(self.code);
    }
}

/// Converts an `anyhow::Result` into a halt when it carries a [`ReembedHalt`],
/// passing operational errors through untouched.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn or_halt<T>(result: Result<T>, json: bool) -> Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            if let Some(halt) = error.downcast_ref::<ReembedHalt>() {
                halt.emit(json);
            }
            Err(error)
        }
    }
}

/// Everything a re-embed run needs after the preflight: the current (#104)
/// identity, the preflight-passed target model, the full record set (for
/// text recovery), and the candidate set. The store handle is dropped after
/// the reads — the commit phase opens a fresh one.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
struct PreparedReembed {
    data_dir: PathBuf,
    current: crate::ir::EmbeddingModel,
    previous_model_json: serde_json::Value,
    resolved: crate::reembed::ResolvedLocalModel,
    declared_dim: usize,
    target_declared: crate::ir::EmbeddingModel,
    target_declared_json: serde_json::Value,
    records: Vec<crate::GraphRecord>,
    candidates: Vec<crate::adapters::ReembedNode>,
    index_dim: Option<usize>,
}

#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
impl PreparedReembed {
    /// The #104 compatibility check against the current store identity.
    fn identity_matches(&self, identity: &crate::ir::EmbeddingModel) -> bool {
        crate::embeddings::differing_identity_fields(&self.current, identity).is_empty()
            && self.current.dim == identity.dim
    }

    /// True when the store already matches the declared target: identity
    /// matches AND the loaded index is at the target dimension.
    fn no_work_remains(&self) -> bool {
        self.identity_matches(&self.target_declared) && self.index_dim == Some(self.declared_dim)
    }
}

/// Steps 1–5: local-availability preflight, declared-dimension target
/// identity, store open, current identity + candidate enumeration, and the
/// nothing-to-migrate / dry-run preconditions. Reads only; the store handle
/// is dropped before returning.
///
/// # Errors
///
/// Returns a [`ReembedHalt`] (via `anyhow`) for the stable pre-commit
/// refusals (exits 12 and 14), or an operational error when the store cannot
/// be read.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn prepare_reembed(data_dir: &Path, model: &str) -> Result<PreparedReembed> {
    use crate::reembed::{
        self, REEMBED_MODEL_UNAVAILABLE_EXIT_CODE, REEMBED_NOTHING_TO_MIGRATE_EXIT_CODE,
        ReembedError,
    };

    // 1. Local-availability preflight (issue #167 AC7): refuse BEFORE
    //    anything loads. Egregore never downloads a model as a side effect.
    let spec = reembed::resolve_model_spec(model);
    let resolved = match reembed::ensure_model_available_locally(&spec) {
        Ok(resolved) => resolved,
        Err(ReembedError::ModelUnavailableLocally {
            spec,
            checked,
            missing,
        }) => {
            let envelope = reembed::model_unavailable_envelope(&spec, &checked, &missing);
            return Err(anyhow::Error::new(ReembedHalt {
                code: REEMBED_MODEL_UNAVAILABLE_EXIT_CODE,
                error_code: reembed::MODEL_UNAVAILABLE_LOCALLY_CODE,
                message: format!(
                    "embedding model is not available locally (checked {}); \
                     Egregore never downloads models",
                    checked.display(),
                ),
                extra: serde_json::json!({ "envelope": envelope }),
            }));
        }
        Err(error) => {
            return Err(anyhow::Error::new(error).context("re-embed preflight failed"));
        }
    };

    // 2. Declared dimension + target identity, before the expensive load.
    //    The measured vector length (compute phase) remains the ground truth.
    let declared_dim = reembed::declared_model_dim(&resolved)
        .map_err(|error| anyhow::Error::new(error).context("re-embed preflight failed"))?;
    let target_declared = reembed::target_model_identity(&resolved, declared_dim);

    // 3. Open the store and read the current identity + candidate set.
    validate_existing_embedded_store(data_dir)?;
    let sink = EmbeddedAletheiaSink::open(data_dir)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
    let records = sink
        .read_all_records()
        .map_err(|error| anyhow::anyhow!("failed to read from embedded store: {error}"))?;
    let identities = crate::embeddings::indexed_identities(&records);
    let candidates = sink
        .reembed_candidate_nodes()
        .map_err(|error| anyhow::anyhow!("failed to enumerate re-embed candidates: {error}"))?;
    let index_dim = match sink.embedding_index_state() {
        crate::embeddings::VectorIndexState::Loaded { dimensions } => Some(dimensions),
        _ => None,
    };
    drop(sink);

    // 4. Nothing to migrate: no identity record, or no embedded nodes.
    let current = identities.first();
    if current.is_none() || candidates.is_empty() {
        return Err(anyhow::Error::new(ReembedHalt {
            code: REEMBED_NOTHING_TO_MIGRATE_EXIT_CODE,
            error_code: reembed::NOTHING_TO_MIGRATE_CODE,
            message: "the store carries no embedding-model identity or no embedded \
                      nodes; nothing to re-embed"
                .to_owned(),
            extra: serde_json::json!({
                "remedy": "ingest the graph with `eg ingest <graph> --adapter embedded --data-dir <dir> --embed` first",
                "data_dir": data_dir.display().to_string(),
            }),
        }));
    }
    let current = current.expect("identity presence checked above").clone();

    Ok(PreparedReembed {
        data_dir: data_dir.to_path_buf(),
        previous_model_json: identity_json(&current),
        current,
        resolved,
        declared_dim,
        target_declared_json: identity_json(&target_declared),
        target_declared,
        records,
        candidates,
        index_dim,
    })
}

/// Steps 6–7: recover the embeddable text for exactly the candidate set from
/// the records (no source re-scan), load model B, and embed every candidate.
/// All vectors are computed before any commit transaction opens.
///
/// Text is matched by the FULL vector key (record ID + temporal identity),
/// not by record ID alone: two observations of the same stable symbol at
/// different commits are different nodes and must re-embed from their own
/// text.
///
/// # Errors
///
/// Returns a [`ReembedHalt`] (exit 1) when a candidate's source text is no
/// longer recoverable, or an operational error when the model fails to load
/// or embedding fails. The store is untouched in every failure path.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn compute_reembed_vectors(prepared: &PreparedReembed) -> Result<(Vec<Vec<f32>>, usize)> {
    use crate::reembed;

    let embed_candidates = crate::embeddings::embedding_candidates(&prepared.records);
    let texts_by_key: std::collections::BTreeMap<crate::embeddings::EmbeddingVectorKey, &str> =
        embed_candidates
            .iter()
            .map(|candidate| {
                (
                    crate::embeddings::EmbeddingVectorKey::from_candidate(candidate),
                    candidate.text.as_str(),
                )
            })
            .collect();
    let mut missing_text = Vec::new();
    let mut ordered_texts = Vec::with_capacity(prepared.candidates.len());
    for node in &prepared.candidates {
        match texts_by_key.get(&node.vector_key) {
            Some(text) => ordered_texts.push(*text),
            None => missing_text.push(node.record_id.clone()),
        }
    }
    if !missing_text.is_empty() {
        return Err(anyhow::Error::new(ReembedHalt {
            code: 1,
            error_code: "reembed_text_unrecoverable",
            message: format!(
                "cannot re-embed {} node(s) whose source text is no longer recoverable \
                 from the store; refusing before any write",
                missing_text.len(),
            ),
            extra: serde_json::json!({ "record_ids": missing_text }),
        }));
    }

    let embedder = reembed::load_embedder(&prepared.resolved).map_err(|error| {
        anyhow::Error::new(error).context("failed to load the re-embed target model")
    })?;
    let vectors = reembed::embed_texts(&embedder, &ordered_texts).map_err(|error| {
        anyhow::Error::new(error).context("failed to embed with the re-embed target model")
    })?;
    let measured_dim = vectors.first().map_or(0, <Vec<f32>>::len);
    if measured_dim == 0 {
        anyhow::bail!("re-embed target model returned zero-dimension vectors");
    }
    if vectors.iter().any(|vector| vector.len() != measured_dim) {
        anyhow::bail!("re-embed target model returned ragged vectors");
    }
    Ok((vectors, measured_dim))
}

/// Step 8: commit the replacement vectors and the superseded identity in one
/// transaction, rebuilding the vector index when the dimension changed.
/// Returns the machine-readable report.
///
/// The identity records the MEASURED dimension (same precedent as `--embed`),
/// and the no-op check is re-evaluated against it so a config that lies
/// about its dimension still converges honestly.
///
/// # Errors
///
/// Returns an operational error when the commit, index rebuild, or persist
/// fails. The commit itself is transactional: a failure there rolls back,
/// leaving the store still needing re-embed.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn commit_reembed(
    prepared: &PreparedReembed,
    vectors: Vec<Vec<f32>>,
    measured_dim: usize,
) -> Result<serde_json::Value> {
    use crate::reembed;

    // Re-check the no-op against the MEASURED identity.
    let target = reembed::target_model_identity(&prepared.resolved, measured_dim);
    let target_model_json = identity_json(&target);
    if prepared.identity_matches(&target) && prepared.index_dim == Some(measured_dim) {
        return Ok(serde_json::json!({
            "ok": true,
            "dry_run": false,
            "data_dir": prepared.data_dir.display().to_string(),
            "candidates": prepared.candidates.len(),
            "reembedded": 0,
            "skipped": prepared.candidates.len(),
            "failed": 0,
            "dimension_changed": false,
            "index_rebuilt": false,
            "previous_model": prepared.previous_model_json,
            "target_model": target_model_json,
        }));
    }

    let updates: Vec<crate::adapters::ReembedVectorUpdate> = prepared
        .candidates
        .iter()
        .zip(vectors)
        .map(|(node, vector)| crate::adapters::ReembedVectorUpdate {
            record_id: node.record_id.clone(),
            node_id: node.node_id,
            vector,
        })
        .collect();
    let candidate_count = updates.len();
    let dimension_changed =
        usize::try_from(prepared.current.dim).unwrap_or(usize::MAX) != measured_dim;

    // Same dimension: one transaction on the store — the loaded index absorbs
    // the patches (the #98 refresh mechanism). Changed dimension: delete the
    // old-dimension index artifacts, reopen, commit, then rebuild the index
    // from the replaced vectors. Either way the identity flips only inside
    // the same transaction as the vectors (AC6).
    let mut index_rebuilt = false;
    if prepared.index_dim == Some(measured_dim) {
        let sink = EmbeddedAletheiaSink::open(&prepared.data_dir).with_context(|| {
            format!(
                "failed to open embedded store {} for re-embed commit",
                prepared.data_dir.display()
            )
        })?;
        sink.reembed_commit(&updates, &target)
            .map_err(|error| anyhow::anyhow!("re-embed commit failed: {error}"))?;
        sink.persist_indexes()
            .map_err(|error| anyhow::anyhow!("failed to persist re-embedded store: {error}"))?;
    } else {
        commit_reembed_dim_change(prepared, updates, &target, measured_dim)?;
        index_rebuilt = true;
    }

    Ok(serde_json::json!({
        "ok": true,
        "dry_run": false,
        "data_dir": prepared.data_dir.display().to_string(),
        "candidates": candidate_count,
        "reembedded": candidate_count,
        "skipped": 0,
        "failed": 0,
        "dimension_changed": dimension_changed,
        "index_rebuilt": index_rebuilt,
        "previous_model": prepared.previous_model_json,
        "target_model": target_model_json,
    }))
}

/// The dimension-change half of [`commit_reembed`]: deletes the old-dimension
/// vector-index artifacts, reopens the store, re-derives the candidate
/// node ids (failing closed on any drift), commits the replacement vectors
/// with the superseded identity in one transaction, then rebuilds the vector
/// index at the new dimension from the replaced vectors.
///
/// # Errors
///
/// Returns an operational error when the index removal, reopen, commit,
/// rebuild, or persist fails. A crash between the commit and the rebuild
/// leaves the explicit incomplete state documented in `docs/cli/re-embed.md`
/// (identity B over B-vectors, no usable index): queries report
/// `semantic_index_absent` and re-running re-embed heals it.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn commit_reembed_dim_change(
    prepared: &PreparedReembed,
    updates: Vec<crate::adapters::ReembedVectorUpdate>,
    target: &crate::ir::EmbeddingModel,
    measured_dim: usize,
) -> Result<()> {
    let vector_keys: Vec<&crate::embeddings::EmbeddingVectorKey> = prepared
        .candidates
        .iter()
        .map(|node| &node.vector_key)
        .collect();
    crate::adapters::remove_persisted_vector_index(&prepared.data_dir).map_err(|error| {
        anyhow::anyhow!(
            "failed to remove the old-dimension vector index at {}: {error}",
            prepared.data_dir.display()
        )
    })?;
    let sink = EmbeddedAletheiaSink::open(&prepared.data_dir).with_context(|| {
        format!(
            "failed to reopen embedded store {} after index removal",
            prepared.data_dir.display()
        )
    })?;
    // NodeIds are engine-assigned; re-derive them after the reopen rather
    // than trusting handles across it, and fail closed on drift. The full
    // vector key (record ID + temporal identity) is compared, not just the
    // record ID, so a history store's per-commit observations are covered.
    let reopened = sink
        .reembed_candidate_nodes()
        .map_err(|error| anyhow::anyhow!("failed to re-enumerate re-embed candidates: {error}"))?;
    let reopened_keys: Vec<&crate::embeddings::EmbeddingVectorKey> =
        reopened.iter().map(|node| &node.vector_key).collect();
    if reopened_keys != vector_keys {
        anyhow::bail!(
            "re-embed candidate set changed across the index-rebuild reopen; refusing to commit"
        );
    }
    let updates: Vec<crate::adapters::ReembedVectorUpdate> = reopened
        .into_iter()
        .zip(updates.into_iter().map(|update| update.vector))
        .map(|(node, vector)| crate::adapters::ReembedVectorUpdate {
            record_id: node.record_id,
            node_id: node.node_id,
            vector,
        })
        .collect();
    sink.reembed_commit(&updates, target)
        .map_err(|error| anyhow::anyhow!("re-embed commit failed: {error}"))?;
    let indexed = sink
        .reembed_rebuild_vector_index(measured_dim)
        .map_err(|error| anyhow::anyhow!("failed to rebuild the vector index: {error}"))?;
    if indexed != updates.len() {
        anyhow::bail!(
            "vector index rebuild indexed {indexed} vectors for {} re-embedded nodes; refusing to report success",
            updates.len(),
        );
    }
    sink.persist_indexes()
        .map_err(|error| anyhow::anyhow!("failed to persist re-embedded store: {error}"))?;
    Ok(())
}

/// Re-embeds an `--embed` store under a new local model without re-scanning
/// sources (issue #167).
///
/// The candidate set is exactly the node set that already carries a persisted
/// `embedding` vector — no source scan, no graph re-extraction, no remote
/// embedding service. Vectors are all computed BEFORE the single commit
/// transaction opens, so a failure anywhere upstream leaves the store
/// untouched ("still needs re-embed"); the transaction replaces every vector
/// and supersedes the `#104` identity record atomically, so a crash can never
/// leave identity B over model-A vectors.
///
/// Machine-readable contract (`--format json`):
/// - exit `0`: `{ok:true, candidates, reembedded, skipped, failed,
///   dimension_changed, index_rebuilt, previous_model, target_model}`
/// - exit `12` (`embedding_model_unavailable_locally`): model B is not
///   available locally — never downloaded, store untouched.
/// - exit `13`: `--dry-run` with work remaining (plan only, store untouched).
/// - exit `14` (`reembed_nothing_to_migrate`): the store carries no embedding
///   identity or no embedded nodes.
/// - exit `1`: operational failure before the commit (JSON envelope on
///   stdout for `--format json`); the store is untouched.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
pub(crate) fn reembed_cmd(
    data_dir: &Path,
    model: &str,
    format: OutputFormat,
    dry_run: bool,
) -> Result<()> {
    use crate::reembed::REEMBED_DRY_RUN_PENDING_EXIT_CODE;

    let json = matches!(format, OutputFormat::Json);
    let prepared = or_halt(prepare_reembed(data_dir, model), json)?;

    if dry_run {
        let plan = serde_json::json!({
            "ok": true,
            "dry_run": true,
            "data_dir": prepared.data_dir.display().to_string(),
            "candidates": prepared.candidates.len(),
            "would_reembed": if prepared.identity_matches(&prepared.target_declared) {
                0
            } else {
                prepared.candidates.len()
            },
            "would_rebuild_index": prepared.index_dim != Some(prepared.declared_dim),
            "dimension_changed": Some(prepared.declared_dim)
                != Some(prepared.current.dim as usize),
            "previous_model": prepared.previous_model_json,
            "target_model": prepared.target_declared_json,
        });
        if json {
            println!("{plan}");
        } else {
            println!("re-embed plan for {}:", prepared.data_dir.display());
            println!("  candidates: {}", prepared.candidates.len());
            println!(
                "  target model: {} (dim {})",
                prepared.target_declared.name, prepared.target_declared.dim,
            );
        }
        if prepared.no_work_remains() {
            return Ok(());
        }
        std::process::exit(REEMBED_DRY_RUN_PENDING_EXIT_CODE);
    }

    if prepared.no_work_remains() {
        let report = serde_json::json!({
            "ok": true,
            "dry_run": false,
            "data_dir": prepared.data_dir.display().to_string(),
            "candidates": prepared.candidates.len(),
            "reembedded": 0,
            "skipped": prepared.candidates.len(),
            "failed": 0,
            "dimension_changed": false,
            "index_rebuilt": false,
            "previous_model": prepared.previous_model_json,
            "target_model": prepared.target_declared_json,
        });
        if json {
            println!("{report}");
        } else {
            println!(
                "re-embed: store already under {} (dim {}); nothing to do",
                prepared.target_declared.name, prepared.target_declared.dim,
            );
        }
        return Ok(());
    }

    let (vectors, measured_dim) = or_halt(compute_reembed_vectors(&prepared), json)?;
    let report = commit_reembed(&prepared, vectors, measured_dim)?;
    if json {
        println!("{report}");
    } else {
        let target_model = &report["target_model"];
        println!(
            "re-embed: re-embedded {} node(s) under {} (dim {}){}",
            report["candidates"].as_u64().unwrap_or(0),
            target_model["name"].as_str().unwrap_or("?"),
            target_model["dim"].as_u64().unwrap_or(0),
            if report["dimension_changed"].as_bool().unwrap_or(false) {
                "; vector index rebuilt at the new dimension"
            } else {
                ""
            },
        );
    }
    Ok(())
}

/// Allow-listed identity rendering for the re-embed report (issue #167):
/// provider, name, version, dim, content hash — never vectors or payloads.
#[cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]
fn identity_json(model: &crate::ir::EmbeddingModel) -> serde_json::Value {
    use crate::embeddings::bounded_identity_field;
    serde_json::json!({
        "provider": bounded_identity_field(&model.provider),
        "name": bounded_identity_field(&model.name),
        "version": bounded_identity_field(&model.version),
        "dim": model.dim,
        "content_hash": bounded_identity_field(&model.content_hash),
    })
}
