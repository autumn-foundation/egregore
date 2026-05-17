//! Embedded `AletheiaDB` adapter.

use std::{collections::BTreeMap, fs, path::Path};

use crate::{
    adapters::{AdapterError, AdapterResult, GraphSink},
    ir::{EdgeLabel, GraphRecord, NodeKind, SemanticDriftMetadata, SourceSpan, TemporalMetadata},
};

/// Graph sink backed by an embedded `AletheiaDB` store.
pub struct EmbeddedAletheiaSink {
    db: ::aletheiadb::AletheiaDB,
    node_ids: BTreeMap<String, ::aletheiadb::NodeId>,
    node_observations: BTreeMap<String, Vec<NodeObservation>>,
    record_handles: BTreeMap<String, StoredRecord>,
    tombstones: BTreeMap<String, GraphRecord>,
}

#[derive(Debug, Clone, Copy)]
enum StoredRecord {
    Node(::aletheiadb::NodeId),
    Edge(::aletheiadb::EdgeId),
}

#[derive(Debug, Clone)]
struct NodeObservation {
    node_id: ::aletheiadb::NodeId,
    git_commit: Option<String>,
}

#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd)]
struct TemporalReadKey {
    valid_time: String,
    git_commit: String,
    observed_at: String,
}

#[derive(Debug, Clone)]
struct ReadBackCandidate<Id> {
    storage_id: Id,
    temporal_key: Option<TemporalReadKey>,
}

impl EmbeddedAletheiaSink {
    /// Opens an embedded `AletheiaDB` store rooted at `data_dir`.
    ///
    /// # Errors
    ///
    /// Returns an error when `AletheiaDB` cannot open the requested data dir.
    pub fn open(data_dir: impl AsRef<Path>) -> AdapterResult<Self> {
        let data_dir = data_dir.as_ref();
        let mut config = ::aletheiadb::config::durable_config_for_data_dir(data_dir);
        if is_fresh_data_dir(data_dir) {
            config.persistence.load_on_startup = false;
        }
        let db = ::aletheiadb::AletheiaDB::with_unified_config(config).map_err(|error| {
            AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: error.to_string(),
            }
        })?;
        Ok(Self {
            db,
            node_ids: BTreeMap::new(),
            node_observations: BTreeMap::new(),
            record_handles: BTreeMap::new(),
            tombstones: BTreeMap::new(),
        })
    }

    /// Persists embedded indexes so a subsequent process can reopen without
    /// replaying from scratch.
    ///
    /// # Errors
    ///
    /// Returns an error when `AletheiaDB` cannot persist its index manifest.
    pub fn persist_indexes(&self) -> AdapterResult<()> {
        self.db
            .persist_indexes()
            .map_err(|error| AdapterError::Rejected {
                record_id: "embedded-store".to_owned(),
                message: error.to_string(),
            })
    }

    /// Reads a graph record back by stable ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the embedded store cannot perform read-back.
    pub fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        <Self as GraphSink>::read_back(self, record_id)
    }

    /// Returns true if the embedded graph contains a Repository -> File -> Symbol path.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    pub fn has_repository_file_symbol_path(&self, repository_id: &str) -> AdapterResult<bool> {
        let Some(repo_node_id) = self.node_ids.get(repository_id).copied() else {
            return Ok(false);
        };

        for contains_edge_id in self
            .db
            .get_outgoing_edges_with_label(repo_node_id, "CONTAINS")
        {
            let file_node_id = self.db.get_edge_target(contains_edge_id).map_err(|error| {
                AdapterError::ReadBack {
                    record_id: repository_id.to_owned(),
                    message: error.to_string(),
                }
            })?;
            for defines_edge_id in self
                .db
                .get_outgoing_edges_with_label(file_node_id, "DEFINES")
            {
                let symbol_node_id = self.db.get_edge_target(defines_edge_id).map_err(|error| {
                    AdapterError::ReadBack {
                        record_id: repository_id.to_owned(),
                        message: error.to_string(),
                    }
                })?;
                let symbol =
                    self.db
                        .get_node(symbol_node_id)
                        .map_err(|error| AdapterError::ReadBack {
                            record_id: repository_id.to_owned(),
                            message: error.to_string(),
                        })?;
                if symbol.get_property("kind").and_then(|value| value.as_str()) == Some("Symbol") {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Returns true if the embedded graph contains a Commit -> Change <- Symbol path.
    ///
    /// # Errors
    ///
    /// Returns an error if an embedded read operation fails.
    pub fn has_commit_change_symbol_path(&self, commit_id: &str) -> AdapterResult<bool> {
        let Some(commit_node_id) = self.node_ids.get(commit_id).copied() else {
            return Ok(false);
        };

        for contains_edge_id in self
            .db
            .get_outgoing_edges_with_label(commit_node_id, "CONTAINS")
        {
            let change_node_id = self.db.get_edge_target(contains_edge_id).map_err(|error| {
                AdapterError::ReadBack {
                    record_id: commit_id.to_owned(),
                    message: error.to_string(),
                }
            })?;
            let change =
                self.db
                    .get_node(change_node_id)
                    .map_err(|error| AdapterError::ReadBack {
                        record_id: commit_id.to_owned(),
                        message: error.to_string(),
                    })?;
            if change.get_property("kind").and_then(|value| value.as_str()) != Some("Change") {
                continue;
            }

            for changed_edge_id in self
                .db
                .get_incoming_edges_with_label(change_node_id, "CHANGED_IN")
            {
                let symbol_node_id = self.db.get_edge_source(changed_edge_id).map_err(|error| {
                    AdapterError::ReadBack {
                        record_id: commit_id.to_owned(),
                        message: error.to_string(),
                    }
                })?;
                let symbol =
                    self.db
                        .get_node(symbol_node_id)
                        .map_err(|error| AdapterError::ReadBack {
                            record_id: commit_id.to_owned(),
                            message: error.to_string(),
                        })?;
                if symbol.get_property("kind").and_then(|value| value.as_str()) == Some("Symbol") {
                    return Ok(true);
                }
            }
        }

        Ok(false)
    }
}

impl GraphSink for EmbeddedAletheiaSink {
    fn write_record(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        match record {
            GraphRecord::Node { .. } => self.write_node(record),
            GraphRecord::Edge { .. } => self.write_edge(record),
            GraphRecord::Tombstone { .. } => {
                self.tombstones
                    .insert(record.id().to_owned(), record.clone());
                Ok(())
            }
        }
    }

    fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        if let Some(handle) = self.record_handles.get(record_id).copied() {
            return self.read_handle(record_id, handle).map(Some);
        }
        if let Some(tombstone) = self.tombstones.get(record_id) {
            return Ok(Some(tombstone.clone()));
        }
        if let Some(node_id) = self.find_node_id_by_codegraph_id(record_id)? {
            return self.read_node_record(record_id, node_id).map(Some);
        }
        if let Some(edge_id) = self.find_edge_id_by_codegraph_id(record_id)? {
            return self.read_edge_record(record_id, edge_id).map(Some);
        }
        Ok(None)
    }
}

impl EmbeddedAletheiaSink {
    fn write_node(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        let GraphRecord::Node {
            id,
            kind,
            schema_version,
            repo_relative_path,
            span,
            name,
            language,
            symbol_kind,
            temporal,
            semantic_drift,
            summary,
        } = record
        else {
            unreachable!("write_node called with non-node record");
        };

        let mut builder =
            base_properties(id, "node", *schema_version, summary).insert("kind", kind.as_str());
        builder = insert_optional(builder, "repo_relative_path", repo_relative_path.as_deref());
        builder = insert_optional(builder, "name", name.as_deref());
        builder = insert_optional(builder, "language", language.as_deref());
        builder = insert_optional(builder, "symbol_kind", symbol_kind.as_deref());
        builder = insert_temporal(builder, temporal.as_ref());
        builder = insert_semantic_drift(builder, semantic_drift.as_deref());
        if let Some(span) = span {
            builder = insert_span(builder, *span);
        }

        let node_id = self
            .db
            .create_node(node_label(*kind), builder.build())
            .map_err(|error| AdapterError::Rejected {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| AdapterError::ReadBack {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        if node
            .get_property("codegraph_id")
            .and_then(|value| value.as_str())
            != Some(id.as_str())
        {
            return Err(AdapterError::ReadBack {
                record_id: id.clone(),
                message: "embedded node codegraph_id mismatch".to_owned(),
            });
        }

        self.node_ids.insert(id.clone(), node_id);
        self.node_observations
            .entry(id.clone())
            .or_default()
            .push(NodeObservation {
                node_id,
                git_commit: temporal
                    .as_ref()
                    .map(|metadata| metadata.git_commit.clone()),
            });
        self.record_handles
            .insert(id.clone(), StoredRecord::Node(node_id));
        Ok(())
    }

    fn write_edge(&mut self, record: &GraphRecord) -> AdapterResult<()> {
        let GraphRecord::Edge {
            id,
            schema_version,
            label,
            source,
            target,
            confidence,
            temporal,
            summary,
        } = record
        else {
            unreachable!("write_edge called with non-edge record");
        };
        let source_id = self.resolve_node_id(id, source, temporal.as_ref(), "source")?;
        let target_id = self.resolve_node_id(id, target, temporal.as_ref(), "target")?;
        let mut builder = base_properties(id, "edge", *schema_version, summary)
            .insert("label", label.as_str())
            .insert("source_codegraph_id", source.as_str())
            .insert("target_codegraph_id", target.as_str());
        builder = insert_optional(builder, "confidence", confidence.as_deref());
        builder = insert_temporal(builder, temporal.as_ref());

        let edge_id = self
            .db
            .create_edge(source_id, target_id, label.as_str(), builder.build())
            .map_err(|error| AdapterError::Rejected {
                record_id: id.clone(),
                message: error.to_string(),
            })?;
        let stored_source =
            self.db
                .get_edge_source(edge_id)
                .map_err(|error| AdapterError::ReadBack {
                    record_id: id.clone(),
                    message: error.to_string(),
                })?;
        let stored_target =
            self.db
                .get_edge_target(edge_id)
                .map_err(|error| AdapterError::ReadBack {
                    record_id: id.clone(),
                    message: error.to_string(),
                })?;
        if stored_source != source_id || stored_target != target_id {
            return Err(AdapterError::ReadBack {
                record_id: id.clone(),
                message: "embedded edge endpoint mismatch".to_owned(),
            });
        }

        self.record_handles
            .insert(id.clone(), StoredRecord::Edge(edge_id));
        Ok(())
    }

    fn resolve_node_id(
        &self,
        edge_id: &str,
        record_id: &str,
        temporal: Option<&TemporalMetadata>,
        endpoint: &str,
    ) -> AdapterResult<::aletheiadb::NodeId> {
        let observations =
            self.node_observations
                .get(record_id)
                .ok_or_else(|| AdapterError::Rejected {
                    record_id: edge_id.to_owned(),
                    message: format!("{endpoint} node {record_id} has not been written"),
                })?;

        if let Some(git_commit) = temporal.map(|metadata| metadata.git_commit.as_str())
            && let Some(observation) = observations
                .iter()
                .rev()
                .find(|observation| observation.git_commit.as_deref() == Some(git_commit))
        {
            return Ok(observation.node_id);
        }

        if let Some(observation) = observations
            .iter()
            .rev()
            .find(|observation| observation.git_commit.is_none())
        {
            return Ok(observation.node_id);
        }

        if let [observation] = observations.as_slice() {
            return Ok(observation.node_id);
        }

        Err(AdapterError::Rejected {
            record_id: edge_id.to_owned(),
            message: format!(
                "{endpoint} node {record_id} has multiple temporal observations and no matching edge commit"
            ),
        })
    }

    fn read_handle(&self, record_id: &str, handle: StoredRecord) -> AdapterResult<GraphRecord> {
        match handle {
            StoredRecord::Node(node_id) => self.read_node_record(record_id, node_id),
            StoredRecord::Edge(edge_id) => self.read_edge_record(record_id, edge_id),
        }
    }

    fn find_node_id_by_codegraph_id(
        &self,
        record_id: &str,
    ) -> AdapterResult<Option<::aletheiadb::NodeId>> {
        let mut found = None;
        for node_id in self.db.get_all_node_ids() {
            let node = self
                .db
                .get_node(node_id)
                .map_err(|error| read_back_error(record_id, error.to_string()))?;
            if optional_str_property(record_id, "codegraph_id", node.get_property("codegraph_id"))?
                .as_deref()
                == Some(record_id)
            {
                let temporal_key =
                    temporal_read_key_from_properties(record_id, |key| node.get_property(key))?;
                if should_replace_read_back_candidate(found.as_ref(), temporal_key.as_ref()) {
                    found = Some(ReadBackCandidate {
                        storage_id: node_id,
                        temporal_key,
                    });
                }
            }
        }
        Ok(found.map(|candidate| candidate.storage_id))
    }

    fn find_edge_id_by_codegraph_id(
        &self,
        record_id: &str,
    ) -> AdapterResult<Option<::aletheiadb::EdgeId>> {
        let mut found = None;
        for node_id in self.db.get_all_node_ids() {
            for edge_id in self.db.get_outgoing_edges(node_id) {
                let edge = self
                    .db
                    .get_edge(edge_id)
                    .map_err(|error| read_back_error(record_id, error.to_string()))?;
                if optional_str_property(
                    record_id,
                    "codegraph_id",
                    edge.get_property("codegraph_id"),
                )?
                .as_deref()
                    == Some(record_id)
                {
                    let temporal_key =
                        temporal_read_key_from_properties(record_id, |key| edge.get_property(key))?;
                    if should_replace_read_back_candidate(found.as_ref(), temporal_key.as_ref()) {
                        found = Some(ReadBackCandidate {
                            storage_id: edge_id,
                            temporal_key,
                        });
                    }
                }
            }
        }
        Ok(found.map(|candidate| candidate.storage_id))
    }

    fn read_node_record(
        &self,
        record_id: &str,
        node_id: ::aletheiadb::NodeId,
    ) -> AdapterResult<GraphRecord> {
        let node = self
            .db
            .get_node(node_id)
            .map_err(|error| read_back_error(record_id, error.to_string()))?;
        let id =
            required_str_property(record_id, "codegraph_id", node.get_property("codegraph_id"))?;
        if id != record_id {
            return Err(read_back_error(
                record_id,
                format!("embedded node codegraph_id mismatch: {id}"),
            ));
        }

        Ok(GraphRecord::Node {
            id,
            kind: parse_node_kind(
                record_id,
                &required_str_property(record_id, "kind", node.get_property("kind"))?,
            )?,
            schema_version: required_u32_property(
                record_id,
                "schema_version",
                node.get_property("schema_version"),
            )?,
            repo_relative_path: optional_str_property(
                record_id,
                "repo_relative_path",
                node.get_property("repo_relative_path"),
            )?,
            span: source_span_from_properties(record_id, |key| node.get_property(key))?,
            name: optional_str_property(record_id, "name", node.get_property("name"))?,
            language: optional_str_property(record_id, "language", node.get_property("language"))?,
            symbol_kind: optional_str_property(
                record_id,
                "symbol_kind",
                node.get_property("symbol_kind"),
            )?,
            temporal: temporal_from_properties(record_id, |key| node.get_property(key))?,
            semantic_drift: semantic_drift_from_properties(record_id, |key| {
                node.get_property(key)
            })?,
            summary: required_str_property(record_id, "summary", node.get_property("summary"))?,
        })
    }

    fn read_edge_record(
        &self,
        record_id: &str,
        edge_id: ::aletheiadb::EdgeId,
    ) -> AdapterResult<GraphRecord> {
        let edge = self
            .db
            .get_edge(edge_id)
            .map_err(|error| read_back_error(record_id, error.to_string()))?;
        let id =
            required_str_property(record_id, "codegraph_id", edge.get_property("codegraph_id"))?;
        if id != record_id {
            return Err(read_back_error(
                record_id,
                format!("embedded edge codegraph_id mismatch: {id}"),
            ));
        }

        Ok(GraphRecord::Edge {
            id,
            schema_version: required_u32_property(
                record_id,
                "schema_version",
                edge.get_property("schema_version"),
            )?,
            label: parse_edge_label(
                record_id,
                &required_str_property(record_id, "label", edge.get_property("label"))?,
            )?,
            source: required_str_property(
                record_id,
                "source_codegraph_id",
                edge.get_property("source_codegraph_id"),
            )?,
            target: required_str_property(
                record_id,
                "target_codegraph_id",
                edge.get_property("target_codegraph_id"),
            )?,
            confidence: optional_str_property(
                record_id,
                "confidence",
                edge.get_property("confidence"),
            )?,
            temporal: temporal_from_properties(record_id, |key| edge.get_property(key))?,
            summary: required_str_property(record_id, "summary", edge.get_property("summary"))?,
        })
    }
}

fn read_back_error(record_id: &str, message: impl Into<String>) -> AdapterError {
    AdapterError::ReadBack {
        record_id: record_id.to_owned(),
        message: message.into(),
    }
}

fn required_str_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<String> {
    optional_str_property(record_id, key, value)?.ok_or_else(|| {
        read_back_error(record_id, format!("missing embedded string property {key}"))
    })
}

fn optional_str_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<String>> {
    value.map_or(Ok(None), |value| {
        value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| {
                read_back_error(
                    record_id,
                    format!("embedded property {key} is not a string"),
                )
            })
    })
}

fn required_u32_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<u32> {
    let raw = value
        .and_then(::aletheiadb::PropertyValue::as_int)
        .ok_or_else(|| {
            read_back_error(
                record_id,
                format!("missing embedded integer property {key}"),
            )
        })?;
    u32::try_from(raw).map_err(|error| {
        read_back_error(
            record_id,
            format!("embedded integer property {key} is out of range: {error}"),
        )
    })
}

fn optional_usize_property(
    record_id: &str,
    key: &str,
    value: Option<&::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<usize>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let raw = value.as_int().ok_or_else(|| {
        read_back_error(
            record_id,
            format!("embedded property {key} is not an integer"),
        )
    })?;
    usize::try_from(raw).map(Some).map_err(|error| {
        read_back_error(
            record_id,
            format!("embedded integer property {key} is out of range: {error}"),
        )
    })
}

fn source_span_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<SourceSpan>> {
    let start_byte = optional_usize_property(record_id, "start_byte", get("start_byte"))?;
    let end_byte = optional_usize_property(record_id, "end_byte", get("end_byte"))?;
    let start_line = optional_usize_property(record_id, "start_line", get("start_line"))?;
    let end_line = optional_usize_property(record_id, "end_line", get("end_line"))?;

    match (start_byte, end_byte, start_line, end_line) {
        (None, None, None, None) => Ok(None),
        (Some(start_byte), Some(end_byte), Some(start_line), Some(end_line)) => {
            Ok(Some(SourceSpan {
                start_byte,
                end_byte,
                start_line,
                end_line,
            }))
        }
        _ => Err(read_back_error(
            record_id,
            "embedded source span is only partially present",
        )),
    }
}

fn temporal_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<TemporalMetadata>> {
    let Some(git_commit) = optional_str_property(record_id, "git_commit", get("git_commit"))?
    else {
        return Ok(None);
    };
    let parents =
        optional_str_property(record_id, "git_parent_commits", get("git_parent_commits"))?
            .map(|parents| parents.split_whitespace().map(ToOwned::to_owned).collect())
            .unwrap_or_default();

    Ok(Some(TemporalMetadata {
        git_commit,
        git_parent_commits: parents,
        valid_time: required_str_property(record_id, "valid_time", get("valid_time"))?,
        author_time: optional_str_property(record_id, "author_time", get("author_time"))?,
        observed_at: required_str_property(record_id, "observed_at", get("observed_at"))?,
    }))
}

fn temporal_read_key_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<TemporalReadKey>> {
    let Some(git_commit) = optional_str_property(record_id, "git_commit", get("git_commit"))?
    else {
        return Ok(None);
    };

    Ok(Some(TemporalReadKey {
        valid_time: required_str_property(record_id, "valid_time", get("valid_time"))?,
        git_commit,
        observed_at: required_str_property(record_id, "observed_at", get("observed_at"))?,
    }))
}

fn should_replace_read_back_candidate<Id>(
    current: Option<&ReadBackCandidate<Id>>,
    candidate_key: Option<&TemporalReadKey>,
) -> bool {
    let Some(current) = current else {
        return true;
    };

    match (&current.temporal_key, candidate_key) {
        (None, Some(_)) => true,
        (Some(current_key), Some(candidate_key)) => candidate_key > current_key,
        (None | Some(_), None) => false,
    }
}

fn semantic_drift_from_properties<'a>(
    record_id: &str,
    get: impl Fn(&str) -> Option<&'a ::aletheiadb::PropertyValue>,
) -> AdapterResult<Option<Box<SemanticDriftMetadata>>> {
    let Some(model_id) =
        optional_str_property(record_id, "semantic_model_id", get("semantic_model_id"))?
    else {
        return Ok(None);
    };

    Ok(Some(Box::new(SemanticDriftMetadata {
        model_id,
        target_record_id: required_str_property(
            record_id,
            "drift_target_record_id",
            get("drift_target_record_id"),
        )?,
        before_git_commit: required_str_property(
            record_id,
            "before_git_commit",
            get("before_git_commit"),
        )?,
        after_git_commit: required_str_property(
            record_id,
            "after_git_commit",
            get("after_git_commit"),
        )?,
        before_valid_time: required_str_property(
            record_id,
            "before_valid_time",
            get("before_valid_time"),
        )?,
        after_valid_time: required_str_property(
            record_id,
            "after_valid_time",
            get("after_valid_time"),
        )?,
        score: required_str_property(record_id, "drift_score", get("drift_score"))?,
    })))
}

fn parse_node_kind(record_id: &str, kind: &str) -> AdapterResult<NodeKind> {
    match kind {
        "Repository" => Ok(NodeKind::Repository),
        "File" => Ok(NodeKind::File),
        "Module" => Ok(NodeKind::Module),
        "Symbol" => Ok(NodeKind::Symbol),
        "Import" => Ok(NodeKind::Import),
        "Diagnostic" => Ok(NodeKind::Diagnostic),
        "Commit" => Ok(NodeKind::Commit),
        "Change" => Ok(NodeKind::Change),
        "SemanticDrift" => Ok(NodeKind::SemanticDrift),
        _ => Err(read_back_error(
            record_id,
            format!("unknown embedded node kind {kind}"),
        )),
    }
}

fn parse_edge_label(record_id: &str, label: &str) -> AdapterResult<EdgeLabel> {
    match label {
        "CONTAINS" => Ok(EdgeLabel::Contains),
        "DEFINES" => Ok(EdgeLabel::Defines),
        "IMPORTS" => Ok(EdgeLabel::Imports),
        "REFERENCES" => Ok(EdgeLabel::References),
        "CALLS" => Ok(EdgeLabel::Calls),
        "IMPLEMENTS" => Ok(EdgeLabel::Implements),
        "MENTIONS" => Ok(EdgeLabel::Mentions),
        "CHANGED_IN" => Ok(EdgeLabel::ChangedIn),
        "PARENT_OF" => Ok(EdgeLabel::ParentOf),
        "DRIFTS_FROM" => Ok(EdgeLabel::DriftsFrom),
        _ => Err(read_back_error(
            record_id,
            format!("unknown embedded edge label {label}"),
        )),
    }
}

fn base_properties(
    id: &str,
    record_type: &str,
    schema_version: u32,
    summary: &str,
) -> ::aletheiadb::PropertyMapBuilder {
    ::aletheiadb::PropertyMapBuilder::new()
        .insert("codegraph_id", id)
        .insert("record_type", record_type)
        .insert("schema_version", i64::from(schema_version))
        .insert("summary", summary)
}

fn insert_optional(
    builder: ::aletheiadb::PropertyMapBuilder,
    key: &str,
    value: Option<&str>,
) -> ::aletheiadb::PropertyMapBuilder {
    if let Some(value) = value {
        builder.insert(key, value)
    } else {
        builder
    }
}

fn insert_span(
    builder: ::aletheiadb::PropertyMapBuilder,
    span: SourceSpan,
) -> ::aletheiadb::PropertyMapBuilder {
    builder
        .insert(
            "start_byte",
            i64::try_from(span.start_byte).unwrap_or(i64::MAX),
        )
        .insert("end_byte", i64::try_from(span.end_byte).unwrap_or(i64::MAX))
        .insert(
            "start_line",
            i64::try_from(span.start_line).unwrap_or(i64::MAX),
        )
        .insert("end_line", i64::try_from(span.end_line).unwrap_or(i64::MAX))
}

fn insert_temporal(
    mut builder: ::aletheiadb::PropertyMapBuilder,
    temporal: Option<&crate::ir::TemporalMetadata>,
) -> ::aletheiadb::PropertyMapBuilder {
    if let Some(temporal) = temporal {
        builder = builder
            .insert("git_commit", temporal.git_commit.as_str())
            .insert("valid_time", temporal.valid_time.as_str())
            .insert("observed_at", temporal.observed_at.as_str());
        if let Some(author_time) = &temporal.author_time {
            builder = builder.insert("author_time", author_time.as_str());
        }
        if !temporal.git_parent_commits.is_empty() {
            builder = builder.insert("git_parent_commits", temporal.git_parent_commits.join(" "));
        }
    }
    builder
}

fn insert_semantic_drift(
    mut builder: ::aletheiadb::PropertyMapBuilder,
    drift: Option<&crate::ir::SemanticDriftMetadata>,
) -> ::aletheiadb::PropertyMapBuilder {
    if let Some(drift) = drift {
        builder = builder
            .insert("semantic_model_id", drift.model_id.as_str())
            .insert("drift_target_record_id", drift.target_record_id.as_str())
            .insert("before_git_commit", drift.before_git_commit.as_str())
            .insert("after_git_commit", drift.after_git_commit.as_str())
            .insert("before_valid_time", drift.before_valid_time.as_str())
            .insert("after_valid_time", drift.after_valid_time.as_str())
            .insert("drift_score", drift.score.as_str());
    }
    builder
}

const fn node_label(kind: NodeKind) -> &'static str {
    match kind {
        NodeKind::Repository
        | NodeKind::File
        | NodeKind::Module
        | NodeKind::Symbol
        | NodeKind::Import
        | NodeKind::Diagnostic
        | NodeKind::Commit
        | NodeKind::Change
        | NodeKind::SemanticDrift => kind.as_str(),
    }
}

#[allow(dead_code)]
const fn _edge_label(label: EdgeLabel) -> &'static str {
    label.as_str()
}

fn is_fresh_data_dir(data_dir: &Path) -> bool {
    match fs::read_dir(data_dir) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}
