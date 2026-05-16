//! Embedded `AletheiaDB` adapter.

use std::{collections::BTreeMap, fs, path::Path};

use crate::{
    adapters::{AdapterError, AdapterResult, GraphSink},
    ir::{EdgeLabel, GraphRecord, NodeKind, SourceSpan},
};

/// Graph sink backed by an embedded `AletheiaDB` store.
pub struct EmbeddedAletheiaSink {
    db: ::aletheiadb::AletheiaDB,
    node_ids: BTreeMap<String, ::aletheiadb::NodeId>,
    records: BTreeMap<String, GraphRecord>,
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
            records: BTreeMap::new(),
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
                self.records.insert(record.id().to_owned(), record.clone());
                Ok(())
            }
        }
    }

    fn read_back(&self, record_id: &str) -> AdapterResult<Option<GraphRecord>> {
        Ok(self.records.get(record_id).cloned())
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
        self.records.insert(id.clone(), record.clone());
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
        let source_id =
            self.node_ids
                .get(source)
                .copied()
                .ok_or_else(|| AdapterError::Rejected {
                    record_id: id.clone(),
                    message: format!("source node {source} has not been written"),
                })?;
        let target_id =
            self.node_ids
                .get(target)
                .copied()
                .ok_or_else(|| AdapterError::Rejected {
                    record_id: id.clone(),
                    message: format!("target node {target} has not been written"),
                })?;
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

        self.records.insert(id.clone(), record.clone());
        Ok(())
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
