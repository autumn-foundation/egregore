//! Persistent sidecar index for `eg query … --graph <file>` lanes (issue #447).
//!
//! A targeted query such as `eg query deps <symbol>` needs only a handful of
//! records, yet the cold loader (`load_records_from_jsonl`) deserializes every
//! physical line of the graph JSONL. This module builds a content-addressed
//! sidecar index (`<graph>.idx`) that maps record ids/names/paths/kinds and edge
//! adjacency to line byte offsets, so a lane can seek to the closure of records
//! its answer needs and reproduce the EXACT records the cold path produces.
//!
//! The index is a pure access-path optimization: hydrated records are
//! byte-identical with the cold scan, so every migrated lane's stdout and exit
//! code are unchanged (proven by differential tests). Validity is
//! content-addressed on the graph file's BLAKE3 + length; any mismatch makes the
//! caller transparently fall back to the cold scan. `eg index` is the only
//! writer. This is a SEPARATE mechanism from the scan-time incremental cache
//! (`crate::incremental`); it shares that module's versioned self-invalidation
//! discipline but none of its code.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    adapters::records_from_jsonl,
    ir::{EdgeLabel, GraphRecord, NodeKind},
    schema_version::{RecordLineRead, read_record_line},
};

/// Sidecar index format version.
///
/// Bumped whenever the header or body layout changes; an index whose stored
/// version differs is treated as invalid and the caller cold-scans
/// (self-invalidation, mirroring `crate::incremental`).
pub const INDEX_FORMAT_VERSION: u32 = 1;

/// File magic identifying an Egregore graph sidecar index.
const INDEX_MAGIC: &[u8; 4] = b"EGIX";

/// Fixed header width: 4 (magic) + 4 (version `u32`) + 8 (`graph_len` `u64`)
/// + 32 (`graph_blake3`) = 48 bytes, followed by a single `\n` separator.
const HEADER_LEN: usize = 4 + 4 + 8 + 32;

/// Returns the sidecar index path for a graph file (`foo.jsonl` →
/// `foo.jsonl.idx`).
#[must_use]
pub fn index_path_for(graph: &Path) -> PathBuf {
    let mut name = graph.as_os_str().to_os_string();
    name.push(".idx");
    PathBuf::from(name)
}

/// Which records a targeted lane needs from the graph.
///
/// `Whole` is the cold-scan escape hatch (byte-identical to today). The other
/// variants name a closure the sidecar index can hydrate by seek; each is a
/// SUPERSET chosen so the migrated lane's output is byte-identical to the cold
/// path (see [`GraphIndex::hydrate`]). Closed + minimal for v1.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Selector {
    /// Every record (cold scan).
    Whole,
    /// One record id's neighbourhood closure.
    ById(String),
    /// Every node named `name`, each expanded like [`Selector::ById`].
    ByName(String),
    /// Every node under one repo-relative path (spans/symbols for one file).
    ByPath(String),
    /// Every node of one kind string (e.g. `Import` for who-imports).
    ByKind(String),
}

/// The serializable body of a [`GraphIndex`]: sorted maps from lookup keys to
/// sorted line-start byte offsets.
///
/// `BTreeMap`/sorted `Vec`s make the serialized bytes deterministic
/// (byte-identical across builds of an unchanged graph).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphIndexBody {
    /// Record `id()` → sorted line-start offsets of ALL physical versions.
    pub by_id: BTreeMap<String, Vec<u64>>,
    /// Tombstone `deleted_id` → sorted offsets (so hydrating an id also pulls
    /// the tombstones that retract it).
    pub by_deleted_id: BTreeMap<String, Vec<u64>>,
    /// Node `name` → sorted offsets (all versions).
    pub by_name: BTreeMap<String, Vec<u64>>,
    /// Node `repo_relative_path` → sorted offsets.
    pub by_path: BTreeMap<String, Vec<u64>>,
    /// Node-kind string → sorted offsets (whole-kind lanes like who-imports).
    pub by_kind: BTreeMap<String, Vec<u64>>,
    /// Node id → sorted offsets of every incident edge (source OR target).
    pub adjacency: BTreeMap<String, Vec<u64>>,
}

/// A parsed sidecar index: the content-addressing header plus the offset body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphIndex {
    /// Byte length of the graph file the index was built over.
    pub graph_len: u64,
    /// BLAKE3 of the graph file bytes the index was built over.
    pub graph_blake3: [u8; 32],
    /// The offset maps.
    pub body: GraphIndexBody,
}

/// Failure building a graph index — mirrors the cold loader so a graph that
/// cold-loads cleanly is the only graph that indexes.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    /// The graph file could not be read.
    #[error("failed to read graph file {path}: {source}")]
    Read {
        /// The graph path.
        path: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// A non-blank line failed to parse or carried an unknown schema version.
    /// The build refuses so the cold path's error is reproduced on fallback.
    #[error("graph line {line} is not an indexable record: {message}")]
    Line {
        /// 1-based line number.
        line: usize,
        /// Parse / unknown-version detail.
        message: String,
    },
}

/// Why a sidecar index could not be used, forcing a cold-scan fallback. These
/// are never surfaced to the user as errors — the caller silently cold-scans.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The `.idx` file is absent.
    #[error("index file absent")]
    Absent,
    /// The `.idx` file could not be read.
    #[error("index read error: {0}")]
    Read(std::io::Error),
    /// The header is too short, lacks the magic, or the body is malformed.
    #[error("index is corrupt: {0}")]
    Corrupt(String),
    /// The stored format version is not [`INDEX_FORMAT_VERSION`].
    #[error("index format version {found} is not {expected}")]
    VersionMismatch {
        /// Version read from the file.
        found: u32,
        /// Version this build understands.
        expected: u32,
    },
    /// The graph's current length/hash differs from the index's — it is stale.
    #[error("index is stale (graph length or hash mismatch)")]
    Stale,
}

impl GraphIndex {
    /// Builds an index over a graph JSONL file, streaming it once.
    ///
    /// Blank lines are skipped exactly as [`records_from_jsonl`] skips them.
    /// Every non-blank line is parsed with [`read_record_line`]; a parse error
    /// or an unknown schema version aborts the build with [`BuildError::Line`]
    /// so the cold path's behaviour is reproduced on fallback.
    ///
    /// # Errors
    ///
    /// Returns [`BuildError`] if the file cannot be read or a line is not an
    /// indexable record.
    pub fn build(graph: &Path) -> Result<Self, BuildError> {
        let bytes = fs::read(graph).map_err(|source| BuildError::Read {
            path: graph.display().to_string(),
            source,
        })?;
        Self::build_from_bytes(&bytes)
    }

    /// Builds an index from graph bytes already in memory (the unit-test entry
    /// point; [`build`](Self::build) reads the file then calls this).
    ///
    /// # Errors
    ///
    /// Returns [`BuildError::Line`] if any non-blank line is not an indexable
    /// record.
    pub fn build_from_bytes(bytes: &[u8]) -> Result<Self, BuildError> {
        let graph_len = bytes.len() as u64;
        let graph_blake3 = *blake3::hash(bytes).as_bytes();
        let mut body = GraphIndexBody::default();

        // Stream the file tracking a running byte cursor at each line start.
        // Splitting on b'\n' and stripping a trailing '\r' matches `str::lines`
        // (the semantics `records_from_jsonl` relies on) while keeping offsets
        // in raw file bytes so a later seek lands on the exact line start.
        let mut cursor: u64 = 0;
        for (index, raw_line) in bytes.split_inclusive(|b| *b == b'\n').enumerate() {
            let line_no = index + 1;
            let start = cursor;
            cursor += raw_line.len() as u64;
            // Trim the trailing '\n' and optional '\r'.
            let mut content = raw_line;
            if content.last() == Some(&b'\n') {
                content = &content[..content.len() - 1];
            }
            if content.last() == Some(&b'\r') {
                content = &content[..content.len() - 1];
            }
            let text = match std::str::from_utf8(content) {
                Ok(text) => text,
                Err(error) => {
                    return Err(BuildError::Line {
                        line: line_no,
                        message: format!("line is not valid UTF-8: {error}"),
                    });
                }
            };
            if text.trim().is_empty() {
                continue;
            }
            let record = match read_record_line(text) {
                Ok(RecordLineRead::Record(record)) => *record,
                Ok(RecordLineRead::UnknownSchemaVersion(unknown)) => {
                    return Err(BuildError::Line {
                        line: line_no,
                        message: format!("unknown schema version: {unknown}"),
                    });
                }
                Err(error) => {
                    return Err(BuildError::Line {
                        line: line_no,
                        message: error.to_string(),
                    });
                }
            };
            index_record(&mut body, &record, start);
        }

        // Every offset list is sorted for determinism.
        for map in [
            &mut body.by_id,
            &mut body.by_deleted_id,
            &mut body.by_name,
            &mut body.by_path,
            &mut body.by_kind,
            &mut body.adjacency,
        ] {
            for offsets in map.values_mut() {
                offsets.sort_unstable();
                offsets.dedup();
            }
        }

        Ok(Self {
            graph_len,
            graph_blake3,
            body,
        })
    }

    /// Serializes the index to its on-disk byte form (header + `\n` + JSON body).
    ///
    /// # Errors
    ///
    /// Returns an error if the body cannot be serialized to JSON.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        let json = serde_json::to_vec(&self.body)?;
        let mut out = Vec::with_capacity(HEADER_LEN + 1 + json.len());
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&INDEX_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.graph_len.to_le_bytes());
        out.extend_from_slice(&self.graph_blake3);
        out.push(b'\n');
        out.extend_from_slice(&json);
        Ok(out)
    }

    /// Writes the index to `index_path` atomically (`.tmp` + rename), so a reader
    /// never observes a torn index. `eg index` is the only caller.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization, the temp write, or the rename fails.
    pub fn write_atomic(&self, index_path: &Path) -> std::io::Result<()> {
        let bytes = self
            .to_bytes()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let mut tmp = index_path.as_os_str().to_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, index_path)
    }

    /// Parses the header + body from raw index bytes WITHOUT validating them
    /// against a graph file. Callers use [`load_for`](Self::load_for) to also
    /// enforce content-addressed validity.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Corrupt`] / [`LoadError::VersionMismatch`] on a bad
    /// header or body.
    pub fn parse(bytes: &[u8]) -> Result<Self, LoadError> {
        if bytes.len() < HEADER_LEN + 1 {
            return Err(LoadError::Corrupt("file shorter than header".to_owned()));
        }
        if &bytes[0..4] != INDEX_MAGIC {
            return Err(LoadError::Corrupt("bad magic".to_owned()));
        }
        // The length check above guarantees at least `HEADER_LEN + 1` bytes, so
        // these fixed-width reads never index out of range; building the arrays
        // by element avoids a fallible `try_into` (and its panic-doc lint).
        let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        if version != INDEX_FORMAT_VERSION {
            return Err(LoadError::VersionMismatch {
                found: version,
                expected: INDEX_FORMAT_VERSION,
            });
        }
        let graph_len = u64::from_le_bytes([
            bytes[8], bytes[9], bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        ]);
        let mut graph_blake3 = [0u8; 32];
        graph_blake3.copy_from_slice(&bytes[16..48]);
        if bytes[HEADER_LEN] != b'\n' {
            return Err(LoadError::Corrupt("missing header separator".to_owned()));
        }
        let body: GraphIndexBody = serde_json::from_slice(&bytes[HEADER_LEN + 1..])
            .map_err(|error| LoadError::Corrupt(format!("body JSON: {error}")))?;
        Ok(Self {
            graph_len,
            graph_blake3,
            body,
        })
    }

    /// Loads and validates the sidecar index for a graph file, returning it only
    /// when it is present, well-formed, the current format version, and
    /// content-addressed to the graph's current length and BLAKE3.
    ///
    /// # Errors
    ///
    /// Returns a [`LoadError`] for every reason to fall back to a cold scan
    /// (absent, unreadable, corrupt, version mismatch, stale). Callers treat any
    /// error as "cold-scan"; none is a user-facing failure.
    pub fn load_for(graph: &Path) -> Result<Self, LoadError> {
        let index_path = index_path_for(graph);
        let idx_bytes = match fs::read(&index_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(LoadError::Absent);
            }
            Err(error) => return Err(LoadError::Read(error)),
        };
        let index = Self::parse(&idx_bytes)?;
        let graph_len = fs::metadata(graph).map_err(LoadError::Read)?.len();
        if graph_len != index.graph_len {
            return Err(LoadError::Stale);
        }
        let graph_bytes = fs::read(graph).map_err(LoadError::Read)?;
        if *blake3::hash(&graph_bytes).as_bytes() != index.graph_blake3 {
            return Err(LoadError::Stale);
        }
        Ok(index)
    }

    /// Computes the sorted set of line-start offsets the selector's closure needs.
    ///
    /// `Whole` returns `None` (the caller cold-scans). Every other selector
    /// returns the byte offsets to hydrate; see the module SPEC for the closure
    /// definition. Offsets are returned ascending, i.e. file order.
    #[must_use]
    pub fn closure_offsets(&self, selector: &Selector, graph_bytes: &[u8]) -> Option<Vec<u64>> {
        let mut offsets: BTreeSet<u64> = BTreeSet::new();
        // Track ids already fully expanded so cycles and shared neighbours do not
        // loop or re-walk.
        let mut expanded: BTreeSet<String> = BTreeSet::new();
        let mut climbed: BTreeSet<String> = BTreeSet::new();

        match selector {
            Selector::Whole => return None,
            Selector::ById(id) => {
                self.expand_by_id(id, graph_bytes, &mut offsets, &mut expanded, &mut climbed);
            }
            Selector::ByName(name) => {
                for id in Self::ids_at(&self.body.by_name, name, graph_bytes) {
                    self.expand_by_id(&id, graph_bytes, &mut offsets, &mut expanded, &mut climbed);
                }
            }
            Selector::ByPath(path) => {
                let ids = Self::ids_at(&self.body.by_path, path, graph_bytes);
                for id in &ids {
                    self.add_id_versions(id, &mut offsets);
                }
                // Ancestry climb so RepositoryIndex owner/display attribution
                // reproduces the cold answer (at/locate/file emit repository_id).
                for id in &ids {
                    self.climb_ancestry(id, graph_bytes, &mut offsets, &mut climbed);
                }
            }
            Selector::ByKind(kind) => {
                let ids = Self::ids_at(&self.body.by_kind, kind, graph_bytes);
                for id in &ids {
                    self.add_id_versions(id, &mut offsets);
                }
            }
        }
        Some(offsets.into_iter().collect())
    }

    /// Hydrates the closure for a selector by seeking to each offset and reading
    /// exactly one line, reproducing the records the cold path would produce.
    ///
    /// Returns `None` for [`Selector::Whole`] (the caller cold-scans).
    ///
    /// # Errors
    ///
    /// Returns an error if the graph file cannot be opened/read or a hydrated
    /// line does not parse. (A validated index guarantees every indexed line
    /// parsed at build time, so a parse error here means the file changed under
    /// an unvalidated caller.)
    pub fn hydrate(
        &self,
        graph: &Path,
        selector: &Selector,
    ) -> std::io::Result<Option<Vec<GraphRecord>>> {
        let graph_bytes = fs::read(graph)?;
        let Some(offsets) = self.closure_offsets(selector, &graph_bytes) else {
            return Ok(None);
        };
        let file = fs::File::open(graph)?;
        let mut reader = BufReader::new(file);
        let mut records = Vec::with_capacity(offsets.len());
        let mut line = String::new();
        for offset in offsets {
            reader.seek(SeekFrom::Start(offset))?;
            line.clear();
            reader.read_line(&mut line)?;
            let text = line.trim_end_matches(['\n', '\r']);
            if text.trim().is_empty() {
                continue;
            }
            match read_record_line(text) {
                Ok(RecordLineRead::Record(record)) => records.push(*record),
                Ok(RecordLineRead::UnknownSchemaVersion(_)) | Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("hydrated line at offset {offset} did not parse"),
                    ));
                }
            }
        }
        Ok(Some(records))
    }

    // ── closure helpers ──────────────────────────────────────────────────────

    /// Reads the ids present at a lookup key's offsets (by seeking to each and
    /// reading the record's `id()`). Deterministic (a `BTreeSet`).
    fn ids_at(map: &BTreeMap<String, Vec<u64>>, key: &str, graph_bytes: &[u8]) -> Vec<String> {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        if let Some(offsets) = map.get(key) {
            for &offset in offsets {
                if let Some(record) = read_record_at(graph_bytes, offset) {
                    ids.insert(record.id().to_owned());
                }
            }
        }
        ids.into_iter().collect()
    }

    /// Adds a single id's every physical version plus every tombstone that
    /// retracts it.
    fn add_id_versions(&self, id: &str, offsets: &mut BTreeSet<u64>) {
        if let Some(list) = self.body.by_id.get(id) {
            offsets.extend(list.iter().copied());
        }
        if let Some(list) = self.body.by_deleted_id.get(id) {
            offsets.extend(list.iter().copied());
        }
    }

    /// Expands one id into its neighbourhood closure: its versions+tombstones,
    /// its incident edges, each neighbour's versions+tombstones and incident
    /// edges, then a containment-ancestry climb from the id and each neighbour.
    fn expand_by_id(
        &self,
        id: &str,
        graph_bytes: &[u8],
        offsets: &mut BTreeSet<u64>,
        expanded: &mut BTreeSet<String>,
        climbed: &mut BTreeSet<String>,
    ) {
        if !expanded.insert(id.to_owned()) {
            return;
        }
        self.add_id_versions(id, offsets);
        // Incident edges + one hop of neighbours (with their incident edges).
        if let Some(edge_offsets) = self.body.adjacency.get(id) {
            for &edge_offset in edge_offsets {
                offsets.insert(edge_offset);
                if let Some(GraphRecord::Edge { source, target, .. }) =
                    read_record_at(graph_bytes, edge_offset)
                {
                    let other = if source == id { &target } else { &source };
                    self.add_id_versions(other, offsets);
                    if let Some(neighbour_edges) = self.body.adjacency.get(other) {
                        offsets.extend(neighbour_edges.iter().copied());
                    }
                    self.climb_ancestry(other, graph_bytes, offsets, climbed);
                }
            }
        }
        self.climb_ancestry(id, graph_bytes, offsets, climbed);
    }

    /// Climbs the containment ancestry (`CONTAINS`/`DEFINES`/`IMPORTS` edges where
    /// the node is the target) to the repository root, adding each ancestor's
    /// versions and incident edges so `RepositoryIndex` can reconstruct
    /// owner/display attribution for `id`.
    fn climb_ancestry(
        &self,
        id: &str,
        graph_bytes: &[u8],
        offsets: &mut BTreeSet<u64>,
        climbed: &mut BTreeSet<String>,
    ) {
        let mut stack = vec![id.to_owned()];
        while let Some(node) = stack.pop() {
            if !climbed.insert(node.clone()) {
                continue;
            }
            let Some(edge_offsets) = self.body.adjacency.get(&node) else {
                continue;
            };
            for &edge_offset in edge_offsets {
                let Some(GraphRecord::Edge {
                    label,
                    source,
                    target,
                    ..
                }) = read_record_at(graph_bytes, edge_offset)
                else {
                    continue;
                };
                // A parent edge points AT this node with a containment label.
                if target == node
                    && matches!(
                        label,
                        EdgeLabel::Contains | EdgeLabel::Defines | EdgeLabel::Imports
                    )
                {
                    offsets.insert(edge_offset);
                    self.add_id_versions(&source, offsets);
                    if let Some(parent_edges) = self.body.adjacency.get(&source) {
                        offsets.extend(parent_edges.iter().copied());
                    }
                    stack.push(source);
                }
            }
        }
    }
}

/// Reads a single record at a known line-start byte offset from in-memory graph
/// bytes. Returns `None` if the offset is out of range or the line does not
/// parse (a validated index never presents such an offset).
fn read_record_at(graph_bytes: &[u8], offset: u64) -> Option<GraphRecord> {
    let start = usize::try_from(offset).ok()?;
    if start >= graph_bytes.len() {
        return None;
    }
    let rest = &graph_bytes[start..];
    let end = rest.iter().position(|b| *b == b'\n').unwrap_or(rest.len());
    let mut content = &rest[..end];
    if content.last() == Some(&b'\r') {
        content = &content[..content.len() - 1];
    }
    let text = std::str::from_utf8(content).ok()?;
    match read_record_line(text) {
        Ok(RecordLineRead::Record(record)) => Some(*record),
        _ => None,
    }
}

/// Adds one record's contribution to the index maps at `offset`.
fn index_record(body: &mut GraphIndexBody, record: &GraphRecord, offset: u64) {
    body.by_id
        .entry(record.id().to_owned())
        .or_default()
        .push(offset);
    match record {
        GraphRecord::Node {
            id,
            kind,
            name,
            repo_relative_path,
            ..
        } => {
            body.by_kind
                .entry(node_kind_key(*kind).to_owned())
                .or_default()
                .push(offset);
            if let Some(name) = name {
                body.by_name.entry(name.clone()).or_default().push(offset);
            }
            if let Some(path) = repo_relative_path {
                body.by_path.entry(path.clone()).or_default().push(offset);
            }
            let _ = id;
        }
        GraphRecord::Edge { source, target, .. } => {
            body.adjacency
                .entry(source.clone())
                .or_default()
                .push(offset);
            if target != source {
                body.adjacency
                    .entry(target.clone())
                    .or_default()
                    .push(offset);
            }
        }
        GraphRecord::Tombstone { deleted_id, .. } => {
            body.by_deleted_id
                .entry(deleted_id.clone())
                .or_default()
                .push(offset);
        }
    }
}

/// The `by_kind` key for a node kind — its `PascalCase` serialization (e.g.
/// `Import`), matching `NodeKind`'s serde representation.
const fn node_kind_key(kind: NodeKind) -> &'static str {
    kind.as_str()
}

/// Convenience: read every record a cold scan would produce, for tests and the
/// fallback path parity checks.
///
/// # Errors
///
/// Propagates the cold loader's parse error.
pub fn cold_records(graph_bytes: &str) -> anyhow::Result<Vec<GraphRecord>> {
    records_from_jsonl(graph_bytes).map_err(|error| anyhow::anyhow!("{error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::similar_names)]

    use super::*;
    use crate::ir::{Graph, GraphRecord, NodeKind, SourceSpan, stable_id};

    const fn span(a: usize, b: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 100,
            start_line: a,
            end_line: b,
        }
    }

    fn file_id(path: &str) -> String {
        stable_id(&["node", "File", path])
    }

    fn sym_id(path: &str, name: &str) -> String {
        stable_id(&["node", "Symbol", path, name])
    }

    /// A small graph: repo → file → two symbols, one CALLS edge, one tombstone,
    /// and a second version of one symbol id (append-only supersession).
    fn sample_graph() -> Graph {
        let mut graph = Graph::new();
        let repo = stable_id(&["node", "Repository", "r"]);
        graph.push(GraphRecord::node(
            repo.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("r".to_owned()),
            "Repository r".to_owned(),
        ));
        let fid = file_id("src/a.rs");
        graph.push(GraphRecord::syntax_node(
            fid.clone(),
            NodeKind::File,
            "src/a.rs".to_owned(),
            span(1, 50),
            "a.rs".to_owned(),
            "rust",
            "file".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo,
            fid.clone(),
            None,
            "contains".to_owned(),
        ));
        let caller = sym_id("src/a.rs", "caller");
        graph.push(GraphRecord::syntax_node(
            caller.clone(),
            NodeKind::Symbol,
            "src/a.rs".to_owned(),
            span(5, 10),
            "caller".to_owned(),
            "rust",
            "fn caller".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            fid.clone(),
            caller.clone(),
            None,
            "defines".to_owned(),
        ));
        let callee = sym_id("src/a.rs", "callee");
        graph.push(GraphRecord::syntax_node(
            callee.clone(),
            NodeKind::Symbol,
            "src/a.rs".to_owned(),
            span(12, 20),
            "callee".to_owned(),
            "rust",
            "fn callee".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Defines,
            fid,
            callee.clone(),
            None,
            "defines".to_owned(),
        ));
        graph.push(GraphRecord::edge(
            EdgeLabel::Calls,
            caller.clone(),
            callee.clone(),
            Some("1.0".to_owned()),
            "calls".to_owned(),
        ));
        // A second physical version of `caller` (append-only supersession).
        graph.push(GraphRecord::syntax_node(
            caller,
            NodeKind::Symbol,
            "src/a.rs".to_owned(),
            span(5, 11),
            "caller".to_owned(),
            "rust",
            "fn caller v2".to_owned(),
        ));
        // A tombstone retracting `callee`.
        graph.push(GraphRecord::Tombstone {
            id: stable_id(&["tomb", &callee]),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: callee,
            summary: "deleted".to_owned(),
            producer: None,
        });
        graph
    }

    fn jsonl(graph: &Graph) -> String {
        graph.to_jsonl().expect("jsonl")
    }

    #[test]
    fn build_maps_contain_expected_keys() {
        let g = sample_graph();
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let caller = sym_id("src/a.rs", "caller");
        let callee = sym_id("src/a.rs", "callee");
        // caller has two physical versions → two offsets.
        assert_eq!(index.body.by_id.get(&caller).map(Vec::len), Some(2));
        // callee has a tombstone → by_deleted_id keyed on callee id.
        assert!(index.body.by_deleted_id.contains_key(&callee));
        // by_name resolves both symbol names.
        assert!(index.body.by_name.contains_key("caller"));
        assert!(index.body.by_name.contains_key("callee"));
        // by_path groups the file's nodes.
        assert!(index.body.by_path.contains_key("src/a.rs"));
        // by_kind uses the PascalCase kind string.
        assert!(index.body.by_kind.contains_key("Symbol"));
        assert!(index.body.by_kind.contains_key("File"));
        assert!(index.body.by_kind.contains_key("Repository"));
        // adjacency has the CALLS + DEFINES edges incident to caller.
        assert!(index.body.adjacency.contains_key(&caller));
    }

    #[test]
    fn offset_lists_are_sorted() {
        let g = sample_graph();
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        for offsets in index.body.by_id.values() {
            let mut sorted = offsets.clone();
            sorted.sort_unstable();
            assert_eq!(*offsets, sorted);
        }
    }

    #[test]
    fn serialized_bytes_are_deterministic() {
        let g = sample_graph();
        let text = jsonl(&g);
        let a = GraphIndex::build_from_bytes(text.as_bytes())
            .expect("build")
            .to_bytes()
            .expect("bytes");
        let b = GraphIndex::build_from_bytes(text.as_bytes())
            .expect("build")
            .to_bytes()
            .expect("bytes");
        assert_eq!(a, b);
    }

    #[test]
    fn round_trip_parse_matches_build() {
        let g = sample_graph();
        let text = jsonl(&g);
        let built = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let bytes = built.to_bytes().expect("bytes");
        let parsed = GraphIndex::parse(&bytes).expect("parse");
        assert_eq!(built, parsed);
    }

    #[test]
    fn parse_rejects_bad_version() {
        let g = sample_graph();
        let text = jsonl(&g);
        let mut bytes = GraphIndex::build_from_bytes(text.as_bytes())
            .expect("build")
            .to_bytes()
            .expect("bytes");
        // Overwrite the version field (bytes 4..8) with 999.
        bytes[4..8].copy_from_slice(&999u32.to_le_bytes());
        match GraphIndex::parse(&bytes) {
            Err(LoadError::VersionMismatch { found, expected }) => {
                assert_eq!(found, 999);
                assert_eq!(expected, INDEX_FORMAT_VERSION);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut bytes = vec![0u8; HEADER_LEN + 1];
        bytes[HEADER_LEN] = b'\n';
        assert!(matches!(
            GraphIndex::parse(&bytes),
            Err(LoadError::Corrupt(_))
        ));
    }

    #[test]
    fn hydrate_by_id_closure_matches_cold_records() {
        let g = sample_graph();
        let text = jsonl(&g);
        let cold = cold_records(&text).expect("cold");
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let caller = sym_id("src/a.rs", "caller");
        let offsets = index
            .closure_offsets(&Selector::ById(caller.clone()), text.as_bytes())
            .expect("offsets");
        // Hydrate the offsets by reading them out of the bytes.
        let hydrated: Vec<GraphRecord> = offsets
            .iter()
            .filter_map(|&o| read_record_at(text.as_bytes(), o))
            .collect();
        // Every hydrated record is a real cold record (a subset).
        for record in &hydrated {
            assert!(
                cold.iter().any(|c| c == record),
                "hydrated record not found in cold set: {record:?}"
            );
        }
        // The closure includes both versions of caller, the CALLS edge, the
        // callee (neighbour) + its tombstone, and the file/repo ancestry.
        let ids: BTreeSet<&str> = hydrated.iter().map(GraphRecord::id).collect();
        assert!(ids.contains(caller.as_str()));
        assert!(ids.contains(sym_id("src/a.rs", "callee").as_str()));
        assert!(ids.contains(file_id("src/a.rs").as_str()));
        assert!(ids.contains(stable_id(&["node", "Repository", "r"]).as_str()));
        // Both physical versions of caller are present.
        assert_eq!(
            hydrated.iter().filter(|r| r.id() == caller).count(),
            2,
            "both versions of caller must hydrate"
        );
        // The callee tombstone is present.
        assert!(
            hydrated
                .iter()
                .any(|r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if *deleted_id == sym_id("src/a.rs", "callee"))),
            "callee tombstone must hydrate"
        );
    }

    #[test]
    fn by_path_closure_includes_file_and_symbols_and_repo() {
        let g = sample_graph();
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let offsets = index
            .closure_offsets(&Selector::ByPath("src/a.rs".to_owned()), text.as_bytes())
            .expect("offsets");
        let ids: BTreeSet<String> = offsets
            .iter()
            .filter_map(|&o| read_record_at(text.as_bytes(), o))
            .map(|r| r.id().to_owned())
            .collect();
        assert!(ids.contains(&file_id("src/a.rs")));
        assert!(ids.contains(&sym_id("src/a.rs", "caller")));
        assert!(ids.contains(&sym_id("src/a.rs", "callee")));
        // Ancestry climb pulled in the repository for owner attribution.
        assert!(ids.contains(&stable_id(&["node", "Repository", "r"])));
    }

    #[test]
    fn by_kind_closure_includes_all_of_kind_plus_tombstones() {
        let g = sample_graph();
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let offsets = index
            .closure_offsets(&Selector::ByKind("Symbol".to_owned()), text.as_bytes())
            .expect("offsets");
        let recs: Vec<GraphRecord> = offsets
            .iter()
            .filter_map(|&o| read_record_at(text.as_bytes(), o))
            .collect();
        // Both symbols (all versions) plus the callee tombstone.
        assert!(recs.iter().any(|r| r.id() == sym_id("src/a.rs", "caller")));
        assert!(recs.iter().any(|r| r.id() == sym_id("src/a.rs", "callee")));
        assert!(recs.iter().any(
            |r| matches!(r, GraphRecord::Tombstone { deleted_id, .. } if *deleted_id == sym_id("src/a.rs", "callee"))
        ));
    }

    #[test]
    fn whole_selector_yields_no_offsets() {
        let g = sample_graph();
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        assert!(
            index
                .closure_offsets(&Selector::Whole, text.as_bytes())
                .is_none()
        );
    }

    #[test]
    fn empty_graph_builds_empty_index() {
        let index = GraphIndex::build_from_bytes(b"").expect("build");
        assert!(index.body.by_id.is_empty());
        assert_eq!(index.graph_len, 0);
    }

    #[test]
    fn blank_lines_are_skipped_like_cold() {
        let text = "\n   \n\n";
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        assert!(index.body.by_id.is_empty());
    }

    #[test]
    fn id_present_only_as_tombstone() {
        let mut g = Graph::new();
        g.push(GraphRecord::Tombstone {
            id: stable_id(&["tomb", "ghost"]),
            schema_version: crate::ir::SCHEMA_VERSION,
            deleted_id: "codegraph:v6:ghost".to_owned(),
            summary: "gone".to_owned(),
            producer: None,
        });
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        assert!(index.body.by_deleted_id.contains_key("codegraph:v6:ghost"));
        // ById on the deleted id still pulls the tombstone.
        let offsets = index
            .closure_offsets(
                &Selector::ById("codegraph:v6:ghost".to_owned()),
                text.as_bytes(),
            )
            .expect("offsets");
        assert_eq!(offsets.len(), 1);
    }
}
