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
//! caller transparently fall back to the cold scan. A query checks validity
//! cheaply first (length, modification time, sampled blocks) and reads the
//! index in place, so its cost does not grow with the graph (issue #120).
//! `eg index` is the only writer. This is a SEPARATE mechanism from the scan-time incremental cache
//! (`crate::incremental`); it shares that module's versioned self-invalidation
//! discipline but none of its code.

use std::{
    cell::Cell,
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
};

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
///
/// Version 2 replaced the JSON body with binary tables that a reader searches
/// in place, so a lookup costs `O(log n)` reads instead of an `O(n)` parse.
pub const INDEX_FORMAT_VERSION: u32 = 2;

/// File magic identifying an Egregore graph sidecar index.
const INDEX_MAGIC: &[u8; 4] = b"EGIX";

/// Fixed header width, 89 bytes: magic (4), version `u32` (4), `graph_len`
/// `u64` (8), `graph_blake3` (32), `graph_stat_stamp` `u64` (8), sample
/// fingerprint (32), history flag (1).
const HEADER_LEN: usize = 4 + 4 + 8 + 32 + 8 + 32 + 1;

/// One directory entry per table: absolute offset `u64` + length `u64`.
const DIR_ENTRY_LEN: usize = 16;

/// Number of tables (see [`Table`]).
const TABLE_COUNT: usize = 6;

/// Bytes before the first table: header, `\n` separator, table directory.
const PREAMBLE_LEN: usize = HEADER_LEN + 1 + TABLE_COUNT * DIR_ENTRY_LEN;

/// One table entry: key offset `u64`, key length `u32`, posting count `u32`,
/// posting offset `u64`. Offsets are relative to the table start.
const ENTRY_LEN: usize = 24;

/// Bytes hashed per sampled block of the graph file.
const SAMPLE_BLOCK: u64 = 4096;

/// Number of evenly spaced blocks in the sample fingerprint.
const SAMPLE_BLOCKS: u64 = 64;

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
/// path (see [`LazyGraphIndex::hydrate`]). Closed + minimal for v1.
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
    /// One record, its incident edges, and each neighbour's lineage. Smaller
    /// than [`Selector::ById`]: it does not pull the neighbours' own edges.
    Anchored(String),
    /// One record's versions, tombstones, and containment ancestry only.
    Lineage(String),
}

/// The in-memory form of a [`GraphIndex`]: sorted maps from lookup keys to
/// sorted line-start byte offsets.
///
/// `BTreeMap`/sorted `Vec`s make the serialized bytes deterministic
/// (byte-identical across builds of an unchanged graph).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
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
    /// Whether the indexed graph is a history / corpus store — any record
    /// carries temporal provenance, or a `Commit` node is present (issue #457
    /// composition). A targeted [`Selector`] closure hydrates only a bounded
    /// subset of records, but the #457 default HEAD-anchor gate
    /// (`query::non_head_current_record_ids`) and the other history-view lanes
    /// consume GLOBAL commit topology and every version of every record to
    /// decide what is current at HEAD; that global set cannot be soundly
    /// supplied by a closure, so the loader falls back to a cold whole-file scan
    /// for a history store (see `load_records_from_jsonl_selected`). A plain
    /// current-tree `scan` graph carries no temporal records, so this stays
    /// `false` and the #447 fast path applies.
    pub has_temporal_history: bool,
}

/// The six lookup tables, in on-disk order.
#[derive(Debug, Clone, Copy)]
enum Table {
    ById,
    DeletedId,
    Name,
    Path,
    Kind,
    Adjacency,
}

impl Table {
    const fn slot(self) -> usize {
        match self {
            Self::ById => 0,
            Self::DeletedId => 1,
            Self::Name => 2,
            Self::Path => 3,
            Self::Kind => 4,
            Self::Adjacency => 5,
        }
    }
}

impl GraphIndexBody {
    const fn map(&self, table: Table) -> &BTreeMap<String, Vec<u64>> {
        match table {
            Table::ById => &self.by_id,
            Table::DeletedId => &self.by_deleted_id,
            Table::Name => &self.by_name,
            Table::Path => &self.by_path,
            Table::Kind => &self.by_kind,
            Table::Adjacency => &self.adjacency,
        }
    }

    const fn map_mut(&mut self, table: Table) -> &mut BTreeMap<String, Vec<u64>> {
        match table {
            Table::ById => &mut self.by_id,
            Table::DeletedId => &mut self.by_deleted_id,
            Table::Name => &mut self.by_name,
            Table::Path => &mut self.by_path,
            Table::Kind => &mut self.by_kind,
            Table::Adjacency => &mut self.adjacency,
        }
    }
}

const TABLES: [Table; TABLE_COUNT] = [
    Table::ById,
    Table::DeletedId,
    Table::Name,
    Table::Path,
    Table::Kind,
    Table::Adjacency,
];

/// A parsed sidecar index: the content-addressing header plus the offset body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphIndex {
    /// Byte length of the graph file the index was built over.
    pub graph_len: u64,
    /// BLAKE3 of the graph file bytes the index was built over.
    pub graph_blake3: [u8; 32],
    /// Stamp of the graph file's times and identity when the index was built
    /// (`0` when unknown). See `stat_stamp`.
    pub graph_stat_stamp: u64,
    /// BLAKE3 over the length and a fixed sample of graph blocks.
    pub sample_fingerprint: [u8; 32],
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

/// The `(offset, length)` pairs of the byte ranges the sample fingerprint
/// covers for a graph of `len` bytes.
fn sample_ranges(len: u64) -> Vec<(u64, usize)> {
    let block = |offset: u64| -> (u64, usize) {
        let size = SAMPLE_BLOCK.min(len - offset);
        (offset, usize::try_from(size).unwrap_or(0))
    };
    let mut ranges = Vec::new();
    if len == 0 {
        return ranges;
    }
    ranges.push(block(0));
    for i in 1..SAMPLE_BLOCKS {
        let offset =
            u64::try_from(u128::from(len) * u128::from(i) / u128::from(SAMPLE_BLOCKS)).unwrap_or(0);
        ranges.push(block(offset.min(len - 1)));
    }
    ranges.push(block(len.saturating_sub(SAMPLE_BLOCK)));
    ranges
}

/// Sample fingerprint of in-memory graph bytes.
fn sample_fingerprint_bytes(bytes: &[u8]) -> [u8; 32] {
    let len = bytes.len() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());
    for (offset, size) in sample_ranges(len) {
        let start = usize::try_from(offset).unwrap_or(0);
        hasher.update(&offset.to_le_bytes());
        hasher.update(&bytes[start..start + size]);
    }
    *hasher.finalize().as_bytes()
}

/// Sample fingerprint of a graph file, by positional reads.
fn sample_fingerprint_file(file: &fs::File, len: u64) -> std::io::Result<[u8; 32]> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&len.to_le_bytes());
    let mut buf = vec![0u8; usize::try_from(SAMPLE_BLOCK).unwrap_or(4096)];
    for (offset, size) in sample_ranges(len) {
        hasher.update(&offset.to_le_bytes());
        read_exact_at(file, &mut buf[..size], offset)?;
        hasher.update(&buf[..size]);
    }
    Ok(*hasher.finalize().as_bytes())
}

/// A stamp of a file's identity and change times (`0` when the platform
/// reports none). On Unix it mixes the modification time, the change time,
/// the inode, and the device, so a copy that keeps the modification time
/// (`cp -p`, `rsync -t`) still gets a different stamp.
fn stat_stamp(meta: &fs::Metadata) -> u64 {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(0));
    if mtime == 0 {
        return 0;
    }
    #[cfg(unix)]
    let extra = {
        use std::os::unix::fs::MetadataExt;
        [
            meta.ctime().cast_unsigned(),
            meta.ctime_nsec().cast_unsigned(),
            meta.ino(),
            meta.dev(),
        ]
    };
    #[cfg(not(unix))]
    let extra = [0u64; 4];
    let mut hasher = blake3::Hasher::new();
    hasher.update(&mtime.to_le_bytes());
    for part in extra {
        hasher.update(&part.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&digest.as_bytes()[..8]);
    // `0` means "no stamp"; keep a real stamp non-zero.
    u64::from_le_bytes(raw).max(1)
}

/// Reads exactly `buf.len()` bytes at `offset` without moving a cursor.
#[cfg(unix)]
fn read_exact_at(file: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

/// Reads exactly `buf.len()` bytes at `offset` on a target with no
/// positional read. The index is then unusable, so callers cold-scan.
#[cfg(not(any(unix, windows)))]
fn read_exact_at(_file: &fs::File, _buf: &mut [u8], _offset: u64) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Reads exactly `buf.len()` bytes at `offset` without a shared cursor.
#[cfg(windows)]
fn read_exact_at(file: &fs::File, mut buf: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        let read = std::os::windows::fs::FileExt::seek_read(file, buf, offset)?;
        if read == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[read..];
        offset += read as u64;
    }
    Ok(())
}

/// Reads the record line that starts at `offset` in a graph file. Returns
/// `Ok(None)` when the line does not parse.
fn read_record_in_file(file: &fs::File, offset: u64) -> std::io::Result<Option<GraphRecord>> {
    const CHUNK: usize = 8192;
    let file_len = file.metadata()?.len();
    let mut line: Vec<u8> = Vec::new();
    let mut at = offset;
    loop {
        if at >= file_len {
            break;
        }
        let want = CHUNK.min(usize::try_from(file_len - at).unwrap_or(CHUNK));
        let mut chunk = vec![0u8; want];
        read_exact_at(file, &mut chunk, at)?;
        if let Some(end) = chunk.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&chunk[..end]);
            break;
        }
        line.extend_from_slice(&chunk);
        at += want as u64;
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let Ok(text) = std::str::from_utf8(&line) else {
        return Ok(None);
    };
    Ok(match read_record_line(text) {
        Ok(RecordLineRead::Record(record)) => Some(*record),
        _ => None,
    })
}

/// Encodes one lookup table: count, sorted entries, key blob, posting blob.
fn encode_table(map: &BTreeMap<String, Vec<u64>>) -> Vec<u8> {
    let head = 8 + map.len() * ENTRY_LEN;
    let keys_total: usize = map.keys().map(String::len).sum();
    let mut entries = Vec::with_capacity(head);
    entries.extend_from_slice(&(map.len() as u64).to_le_bytes());
    let mut keys = Vec::with_capacity(keys_total);
    let mut posts: Vec<u8> = Vec::new();
    for (key, offsets) in map {
        let key_off = (head + keys.len()) as u64;
        let post_off = (head + keys_total + posts.len()) as u64;
        entries.extend_from_slice(&key_off.to_le_bytes());
        entries.extend_from_slice(&u32::try_from(key.len()).unwrap_or(u32::MAX).to_le_bytes());
        entries.extend_from_slice(
            &u32::try_from(offsets.len())
                .unwrap_or(u32::MAX)
                .to_le_bytes(),
        );
        entries.extend_from_slice(&post_off.to_le_bytes());
        keys.extend_from_slice(key.as_bytes());
        for offset in offsets {
            posts.extend_from_slice(&offset.to_le_bytes());
        }
    }
    entries.extend_from_slice(&keys);
    entries.extend_from_slice(&posts);
    entries
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(raw)
}

fn corrupt(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.to_owned())
}

/// A decoded table entry.
struct Entry {
    key_off: u64,
    key_len: u32,
    post_cnt: u32,
    post_off: u64,
}

fn decode_entry(raw: &[u8; ENTRY_LEN]) -> Entry {
    Entry {
        key_off: u64_at(raw, 0),
        key_len: u32_at(raw, 8),
        post_cnt: u32_at(raw, 12),
        post_off: u64_at(raw, 16),
    }
}

/// Reads one table of an index file in place.
struct FileTable<'a> {
    file: &'a fs::File,
    start: u64,
    len: u64,
}

impl FileTable<'_> {
    fn count(&self) -> std::io::Result<u64> {
        let mut raw = [0u8; 8];
        if self.len < 8 {
            return Err(corrupt("table shorter than its count"));
        }
        read_exact_at(self.file, &mut raw, self.start)?;
        let count = u64::from_le_bytes(raw);
        let entries = count
            .checked_mul(ENTRY_LEN as u64)
            .and_then(|n| n.checked_add(8))
            .ok_or_else(|| corrupt("table count overflows"))?;
        if entries > self.len {
            return Err(corrupt("table entries exceed the table"));
        }
        Ok(count)
    }

    fn entry(&self, index: u64) -> std::io::Result<Entry> {
        let mut raw = [0u8; ENTRY_LEN];
        read_exact_at(
            self.file,
            &mut raw,
            self.start + 8 + index * ENTRY_LEN as u64,
        )?;
        let entry = decode_entry(&raw);
        let key_end = entry.key_off.checked_add(u64::from(entry.key_len));
        let post_end = u64::from(entry.post_cnt)
            .checked_mul(8)
            .and_then(|n| n.checked_add(entry.post_off));
        if key_end.is_none_or(|end| end > self.len) || post_end.is_none_or(|end| end > self.len) {
            return Err(corrupt("table entry points outside the table"));
        }
        Ok(entry)
    }

    fn key(&self, entry: &Entry) -> std::io::Result<Vec<u8>> {
        let mut key = vec![0u8; entry.key_len as usize];
        read_exact_at(self.file, &mut key, self.start + entry.key_off)?;
        Ok(key)
    }

    fn postings(&self, entry: &Entry) -> std::io::Result<Vec<u64>> {
        let mut raw = vec![0u8; entry.post_cnt as usize * 8];
        read_exact_at(self.file, &mut raw, self.start + entry.post_off)?;
        Ok(raw.chunks_exact(8).map(|c| u64_at(c, 0)).collect())
    }

    /// Binary search for `key`; the postings of a hit, else empty.
    fn lookup(&self, key: &str) -> std::io::Result<Vec<u64>> {
        let (mut lo, mut hi) = (0u64, self.count()?);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = self.entry(mid)?;
            match self.key(&entry)?.as_slice().cmp(key.as_bytes()) {
                std::cmp::Ordering::Equal => return self.postings(&entry),
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
            }
        }
        Ok(Vec::new())
    }

    fn keys(&self) -> std::io::Result<Vec<String>> {
        (0..self.count()?)
            .map(|i| {
                let entry = self.entry(i)?;
                String::from_utf8(self.key(&entry)?).map_err(|_| corrupt("table key is not UTF-8"))
            })
            .collect()
    }
}

/// What the closure walk needs from an index: posting lists and record reads.
trait IndexView {
    fn postings(&self, table: Table, key: &str) -> Vec<u64>;
    fn record_at(&self, offset: u64) -> Option<GraphRecord>;
}

/// An index body plus the graph bytes, both in memory.
struct MemView<'a> {
    body: &'a GraphIndexBody,
    bytes: &'a [u8],
}

impl IndexView for MemView<'_> {
    fn postings(&self, table: Table, key: &str) -> Vec<u64> {
        self.body.map(table).get(key).cloned().unwrap_or_default()
    }

    fn record_at(&self, offset: u64) -> Option<GraphRecord> {
        read_record_at(self.bytes, offset)
    }
}

/// An index file and graph file read in place. A failed read sets `failed`
/// so the caller falls back to the cold scan instead of using a partial answer.
struct FileView {
    idx: fs::File,
    graph: fs::File,
    tables: [(u64, u64); TABLE_COUNT],
    failed: Cell<bool>,
}

impl FileView {
    const fn table(&self, table: Table) -> FileTable<'_> {
        let (start, len) = self.tables[table.slot()];
        FileTable {
            file: &self.idx,
            start,
            len,
        }
    }
}

impl IndexView for FileView {
    fn postings(&self, table: Table, key: &str) -> Vec<u64> {
        self.table(table).lookup(key).unwrap_or_else(|_| {
            self.failed.set(true);
            Vec::new()
        })
    }

    fn record_at(&self, offset: u64) -> Option<GraphRecord> {
        let record = read_record_in_file(&self.graph, offset).unwrap_or(None);
        if record.is_none() {
            // A validated index only points at lines that parsed at build
            // time, so a miss means damage or a changed graph.
            self.failed.set(true);
        }
        record
    }
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
        let read_error = |source| BuildError::Read {
            path: graph.display().to_string(),
            source,
        };
        // Take the modification time BEFORE reading: a write that lands during
        // the build then leaves a stale time, never a fresh-looking one.
        let mtime = stat_stamp(&fs::metadata(graph).map_err(read_error)?);
        let bytes = fs::read(graph).map_err(read_error)?;
        let mut index = Self::build_from_bytes(&bytes)?;
        index.graph_stat_stamp = mtime;
        Ok(index)
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
        let sample_fingerprint = sample_fingerprint_bytes(bytes);
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
            if record_is_history_signal(&record) {
                body.has_temporal_history = true;
            }
            index_record(&mut body, &record, start);
        }

        // Every offset list is sorted for determinism.
        for table in TABLES {
            for offsets in body.map_mut(table).values_mut() {
                offsets.sort_unstable();
                offsets.dedup();
            }
        }

        Ok(Self {
            graph_len,
            graph_blake3,
            graph_stat_stamp: 0,
            sample_fingerprint,
            body,
        })
    }

    /// Serializes the index to its on-disk byte form: header, `\n`, table
    /// directory, then the six binary tables.
    ///
    /// # Errors
    ///
    /// Never fails today; the `Result` keeps the writer's signature stable.
    pub fn to_bytes(&self) -> Result<Vec<u8>, std::io::Error> {
        let tables: Vec<Vec<u8>> = TABLES
            .iter()
            .map(|table| encode_table(self.body.map(*table)))
            .collect();
        let mut out = Vec::new();
        out.extend_from_slice(INDEX_MAGIC);
        out.extend_from_slice(&INDEX_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&self.graph_len.to_le_bytes());
        out.extend_from_slice(&self.graph_blake3);
        out.extend_from_slice(&self.graph_stat_stamp.to_le_bytes());
        out.extend_from_slice(&self.sample_fingerprint);
        out.push(u8::from(self.body.has_temporal_history));
        out.push(b'\n');
        let mut offset = PREAMBLE_LEN as u64;
        for table in &tables {
            out.extend_from_slice(&offset.to_le_bytes());
            out.extend_from_slice(&(table.len() as u64).to_le_bytes());
            offset += table.len() as u64;
        }
        for table in &tables {
            out.extend_from_slice(table);
        }
        Ok(out)
    }

    /// Writes the index to `index_path` atomically (`.tmp` + rename), so a reader
    /// never observes a torn index. `eg index` is the only caller.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization, the temp write, or the rename fails.
    pub fn write_atomic(&self, index_path: &Path) -> std::io::Result<()> {
        let bytes = self.to_bytes()?;
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

    /// Parses the header + tables from raw index bytes into memory WITHOUT
    /// validating them against a graph file. Query lanes use
    /// [`LazyGraphIndex::open`], which validates and reads in place.
    ///
    /// # Errors
    ///
    /// Returns [`LoadError::Corrupt`] / [`LoadError::VersionMismatch`] on a bad
    /// header or table.
    pub fn parse(bytes: &[u8]) -> Result<Self, LoadError> {
        let header = parse_header(bytes)?;
        let mut body = GraphIndexBody {
            has_temporal_history: header.has_temporal_history,
            ..GraphIndexBody::default()
        };
        for table in TABLES {
            let (start, len) = header.tables[table.slot()];
            let range = usize::try_from(start)
                .ok()
                .zip(usize::try_from(len).ok())
                .and_then(|(s, l)| bytes.get(s..s.checked_add(l)?))
                .ok_or_else(|| LoadError::Corrupt("table outside the file".to_owned()))?;
            *body.map_mut(table) = decode_table(range)
                .map_err(|error| LoadError::Corrupt(format!("table: {error}")))?;
        }
        Ok(Self {
            graph_len: header.graph_len,
            graph_blake3: header.graph_blake3,
            graph_stat_stamp: header.graph_stat_stamp,
            sample_fingerprint: header.sample_fingerprint,
            body,
        })
    }

    /// Computes the sorted set of line-start byte offsets the selector's closure needs.
    ///
    /// `Whole` returns `None` (the caller cold-scans). Every other selector
    /// returns the byte offsets to hydrate; see the module SPEC for the closure
    /// definition. Offsets are returned ascending, i.e. file order.
    #[must_use]
    pub fn closure_offsets(&self, selector: &Selector, graph_bytes: &[u8]) -> Option<Vec<u64>> {
        let view = MemView {
            body: &self.body,
            bytes: graph_bytes,
        };
        closure_offsets_in(&view, selector)
    }
}

/// The fixed header fields of an index file.
struct Header {
    graph_len: u64,
    graph_blake3: [u8; 32],
    graph_stat_stamp: u64,
    sample_fingerprint: [u8; 32],
    has_temporal_history: bool,
    tables: [(u64, u64); TABLE_COUNT],
}

/// Checks the magic/version and reads the header plus table directory from
/// the first [`PREAMBLE_LEN`] bytes of an index.
fn parse_header(bytes: &[u8]) -> Result<Header, LoadError> {
    if bytes.len() < 8 || &bytes[0..4] != INDEX_MAGIC {
        return Err(LoadError::Corrupt(if bytes.len() < 8 {
            "file shorter than header".to_owned()
        } else {
            "bad magic".to_owned()
        }));
    }
    let version = u32_at(bytes, 4);
    if version != INDEX_FORMAT_VERSION {
        return Err(LoadError::VersionMismatch {
            found: version,
            expected: INDEX_FORMAT_VERSION,
        });
    }
    if bytes.len() < PREAMBLE_LEN {
        return Err(LoadError::Corrupt("file shorter than header".to_owned()));
    }
    if bytes[HEADER_LEN] != b'\n' {
        return Err(LoadError::Corrupt("missing header separator".to_owned()));
    }
    let mut graph_blake3 = [0u8; 32];
    graph_blake3.copy_from_slice(&bytes[16..48]);
    let mut sample_fingerprint = [0u8; 32];
    sample_fingerprint.copy_from_slice(&bytes[56..88]);
    let mut tables = [(0u64, 0u64); TABLE_COUNT];
    for (slot, entry) in tables.iter_mut().enumerate() {
        let at = HEADER_LEN + 1 + slot * DIR_ENTRY_LEN;
        *entry = (u64_at(bytes, at), u64_at(bytes, at + 8));
    }
    Ok(Header {
        graph_len: u64_at(bytes, 8),
        graph_blake3,
        graph_stat_stamp: u64_at(bytes, 48),
        sample_fingerprint,
        has_temporal_history: bytes[88] != 0,
        tables,
    })
}

/// Decodes a whole table into a map (the in-memory parse path).
fn decode_table(raw: &[u8]) -> std::io::Result<BTreeMap<String, Vec<u64>>> {
    if raw.len() < 8 {
        return Err(corrupt("table shorter than its count"));
    }
    let count = usize::try_from(u64_at(raw, 0)).map_err(|_| corrupt("count overflows"))?;
    let head = count
        .checked_mul(ENTRY_LEN)
        .and_then(|n| n.checked_add(8))
        .filter(|head| *head <= raw.len())
        .ok_or_else(|| corrupt("table entries exceed the table"))?;
    let _ = head;
    let mut map = BTreeMap::new();
    for i in 0..count {
        let at = 8 + i * ENTRY_LEN;
        let mut entry_raw = [0u8; ENTRY_LEN];
        entry_raw.copy_from_slice(&raw[at..at + ENTRY_LEN]);
        let entry = decode_entry(&entry_raw);
        let key_start = usize::try_from(entry.key_off).map_err(|_| corrupt("key offset"))?;
        let key = key_start
            .checked_add(entry.key_len as usize)
            .and_then(|end| raw.get(key_start..end))
            .ok_or_else(|| corrupt("key outside the table"))?;
        let key = String::from_utf8(key.to_vec()).map_err(|_| corrupt("key is not UTF-8"))?;
        let post_start = usize::try_from(entry.post_off).map_err(|_| corrupt("post offset"))?;
        let posts = (entry.post_cnt as usize)
            .checked_mul(8)
            .and_then(|n| post_start.checked_add(n))
            .and_then(|end| raw.get(post_start..end))
            .ok_or_else(|| corrupt("postings outside the table"))?;
        map.insert(key, posts.chunks_exact(8).map(|c| u64_at(c, 0)).collect());
    }
    Ok(map)
}

/// A validated sidecar index read in place.
///
/// There is no whole-file read and no parse of the tables. A lookup costs
/// `O(log n)` positional reads, so a targeted query's index cost does not grow
/// with the graph (issue #120).
pub struct LazyGraphIndex {
    view: FileView,
    has_temporal_history: bool,
}

impl LazyGraphIndex {
    /// Opens and validates the sidecar index for a graph file.
    ///
    /// Valid means present, well-formed, the current format version, and
    /// matching the graph. Matching is checked cheaply first: same length,
    /// same modification time, and same sample fingerprint. When only the
    /// time differs (a copy or a touch), the full BLAKE3 decides, so a changed
    /// graph is never served from a stale index.
    ///
    /// # Errors
    ///
    /// Returns a [`LoadError`] for every reason to fall back to a cold scan.
    /// Callers treat any error as "cold-scan"; none is a user-facing failure.
    pub fn open(graph: &Path) -> Result<Self, LoadError> {
        let idx = match fs::File::open(index_path_for(graph)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(LoadError::Absent);
            }
            Err(error) => return Err(LoadError::Read(error)),
        };
        let idx_len = idx.metadata().map_err(LoadError::Read)?.len();
        let take = usize::try_from(idx_len.min(PREAMBLE_LEN as u64)).unwrap_or(0);
        let mut preamble = vec![0u8; take];
        read_exact_at(&idx, &mut preamble, 0).map_err(LoadError::Read)?;
        let header = parse_header(&preamble)?;
        for (start, len) in header.tables {
            if start.checked_add(len).is_none_or(|end| end > idx_len) {
                return Err(LoadError::Corrupt("table outside the file".to_owned()));
            }
        }

        let graph_file = fs::File::open(graph).map_err(LoadError::Read)?;
        let meta = graph_file.metadata().map_err(LoadError::Read)?;
        if meta.len() != header.graph_len {
            return Err(LoadError::Stale);
        }
        let quick = header.graph_stat_stamp != 0
            && stat_stamp(&meta) == header.graph_stat_stamp
            && sample_fingerprint_file(&graph_file, meta.len()).map_err(LoadError::Read)?
                == header.sample_fingerprint;
        if !quick {
            let mut hasher = blake3::Hasher::new();
            hasher.update_reader(&graph_file).map_err(LoadError::Read)?;
            if *hasher.finalize().as_bytes() != header.graph_blake3 {
                return Err(LoadError::Stale);
            }
        }
        Ok(Self {
            view: FileView {
                idx,
                graph: graph_file,
                tables: header.tables,
                failed: Cell::new(false),
            },
            has_temporal_history: header.has_temporal_history,
        })
    }

    /// Whether the indexed graph is a history / corpus store (see
    /// [`GraphIndexBody::has_temporal_history`]).
    #[must_use]
    pub const fn has_temporal_history(&self) -> bool {
        self.has_temporal_history
    }

    /// The node-kind keys present in the graph, sorted.
    ///
    /// # Errors
    ///
    /// Returns an error if the kind table cannot be read.
    pub fn kind_keys(&self) -> std::io::Result<Vec<String>> {
        self.view.table(Table::Kind).keys()
    }

    /// Hydrates the closure for a selector by reading exactly one line per
    /// offset, reproducing the records the cold path would produce.
    ///
    /// Returns `None` for [`Selector::Whole`] (the caller cold-scans).
    ///
    /// # Errors
    ///
    /// Returns an error if any read fails or a hydrated line does not parse
    /// (the graph changed under a validated index, or the index is damaged).
    pub fn hydrate(&self, selector: &Selector) -> std::io::Result<Option<Vec<GraphRecord>>> {
        self.hydrate_many(std::slice::from_ref(selector))
    }

    /// Hydrates the union of several selectors' closures, each record once, in
    /// file order. Returns `None` when any selector is [`Selector::Whole`].
    ///
    /// # Errors
    ///
    /// Same as [`hydrate`](Self::hydrate).
    pub fn hydrate_many(
        &self,
        selectors: &[Selector],
    ) -> std::io::Result<Option<Vec<GraphRecord>>> {
        let mut offsets: BTreeSet<u64> = BTreeSet::new();
        for selector in selectors {
            let Some(closure) = closure_offsets_in(&self.view, selector) else {
                return Ok(None);
            };
            offsets.extend(closure);
        }
        if self.view.failed.get() {
            return Err(corrupt("index read failed"));
        }
        let mut records = Vec::with_capacity(offsets.len());
        for offset in offsets {
            match read_record_in_file(&self.view.graph, offset)? {
                Some(record) => records.push(record),
                None => {
                    return Err(corrupt(&format!(
                        "hydrated line at offset {offset} did not parse"
                    )));
                }
            }
        }
        Ok(Some(records))
    }
}

/// Computes the closure offsets for a selector over any [`IndexView`].
fn closure_offsets_in<V: IndexView>(view: &V, selector: &Selector) -> Option<Vec<u64>> {
    let mut walk = Closure {
        view,
        offsets: BTreeSet::new(),
        expanded: BTreeSet::new(),
        climbed: BTreeSet::new(),
    };
    match selector {
        Selector::Whole => return None,
        Selector::ById(id) => walk.expand_by_id(id),
        Selector::ByName(name) => {
            for id in walk.ids_at(Table::Name, name) {
                walk.expand_by_id(&id);
            }
        }
        Selector::ByPath(path) => {
            let ids = walk.ids_at(Table::Path, path);
            for id in &ids {
                walk.add_id_versions(id);
            }
            // Ancestry climb so RepositoryIndex owner/display attribution
            // reproduces the cold answer (at/locate/file emit repository_id).
            for id in &ids {
                walk.climb_ancestry(id);
            }
        }
        Selector::ByKind(kind) => {
            for id in walk.ids_at(Table::Kind, kind) {
                walk.add_id_versions(&id);
            }
        }
        Selector::Anchored(id) => walk.anchor(id),
        Selector::Lineage(id) => {
            walk.add_id_versions(id);
            walk.climb_ancestry(id);
        }
    }
    Some(walk.offsets.into_iter().collect())
}

/// The state of one closure walk.
struct Closure<'v, V: IndexView> {
    view: &'v V,
    offsets: BTreeSet<u64>,
    /// Ids already fully expanded, so cycles and shared neighbours do not loop.
    expanded: BTreeSet<String>,
    /// Ids already climbed for ancestry.
    climbed: BTreeSet<String>,
}

impl<V: IndexView> Closure<'_, V> {
    /// Reads the ids present at a lookup key's offsets (by reading each
    /// record's `id()`). Deterministic (a `BTreeSet`).
    fn ids_at(&self, table: Table, key: &str) -> Vec<String> {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for offset in self.view.postings(table, key) {
            if let Some(record) = self.view.record_at(offset) {
                ids.insert(record.id().to_owned());
            }
        }
        ids.into_iter().collect()
    }

    /// Adds an edge record's offset and every tombstone that retracts it.
    fn add_edge(&mut self, offset: u64, edge_id: &str) {
        self.offsets.insert(offset);
        self.offsets
            .extend(self.view.postings(Table::DeletedId, edge_id));
    }

    /// Adds a single id's every physical version plus every tombstone that
    /// retracts it.
    fn add_id_versions(&mut self, id: &str) {
        self.offsets.extend(self.view.postings(Table::ById, id));
        self.offsets
            .extend(self.view.postings(Table::DeletedId, id));
    }

    /// Expands one id into its neighbourhood closure: its versions+tombstones,
    /// its incident edges, each neighbour's versions+tombstones and incident
    /// edges, then a containment-ancestry climb from the id and each neighbour.
    fn expand_by_id(&mut self, id: &str) {
        if !self.expanded.insert(id.to_owned()) {
            return;
        }
        self.add_id_versions(id);
        // Incident edges + one hop of neighbours (with their incident edges).
        for edge_offset in self.view.postings(Table::Adjacency, id) {
            self.offsets.insert(edge_offset);
            if let Some(GraphRecord::Edge {
                id: edge_id,
                source,
                target,
                ..
            }) = self.view.record_at(edge_offset)
            {
                self.add_edge(edge_offset, &edge_id);
                let other = if source == id { &target } else { &source };
                self.add_id_versions(other);
                self.offsets
                    .extend(self.view.postings(Table::Adjacency, other));
                self.climb_ancestry(other);
            }
        }
        self.climb_ancestry(id);
    }

    /// Adds `id`, its incident edges, and each neighbour's versions and
    /// ancestry (see [`Selector::Anchored`]).
    fn anchor(&mut self, id: &str) {
        self.add_id_versions(id);
        for edge_offset in self.view.postings(Table::Adjacency, id) {
            self.offsets.insert(edge_offset);
            if let Some(GraphRecord::Edge {
                id: edge_id,
                source,
                target,
                ..
            }) = self.view.record_at(edge_offset)
            {
                self.add_edge(edge_offset, &edge_id);
                let other = if source == id { target } else { source };
                self.add_id_versions(&other);
                self.climb_ancestry(&other);
            }
        }
        self.climb_ancestry(id);
    }

    /// `true` when `id` names an `Import` node.
    ///
    /// Read from the index's own `by_id` postings: any parseable version of
    /// the node decides, since a node's kind never changes across versions. An
    /// unreadable or absent node reads as `false` (fail closed — the edge is
    /// simply not climbed).
    fn node_is_import_kind(&self, id: &str) -> bool {
        self.view
            .postings(Table::ById, id)
            .into_iter()
            .filter_map(|o| self.view.record_at(o))
            .any(|record| {
                matches!(
                    record,
                    GraphRecord::Node {
                        kind: NodeKind::Import,
                        ..
                    }
                )
            })
    }

    /// Climbs the containment ancestry (`CONTAINS`/`DEFINES` edges, plus the
    /// containment-shaped `IMPORTS` edge, where the node is the target) to the
    /// repository root, adding each ancestor's versions and incident edges so
    /// `RepositoryIndex` can reconstruct owner/display attribution for `id`.
    ///
    /// The issue-#444 `File —IMPORTS→ Module|File` target edge is NOT climbed:
    /// it is a dependency edge, not containment — climbing it would
    /// misattribute the imported module to the importing file. Only the
    /// extractor's `File —IMPORTS→ Import` shape (the import declaration owned
    /// by its file) counts as a parent edge.
    fn climb_ancestry(&mut self, id: &str) {
        let mut stack = vec![id.to_owned()];
        while let Some(node) = stack.pop() {
            if !self.climbed.insert(node.clone()) {
                continue;
            }
            for edge_offset in self.view.postings(Table::Adjacency, &node) {
                let Some(GraphRecord::Edge {
                    id: edge_id,
                    label,
                    source,
                    target,
                    ..
                }) = self.view.record_at(edge_offset)
                else {
                    continue;
                };
                // A parent edge points AT this node with a containment label. An
                // `IMPORTS` edge is a parent edge only in its containment
                // shape (`File —IMPORTS→ Import`); the issue-#444 module-target
                // edge (`File —IMPORTS→ Module|File`) is a dependency edge and
                // must not climb.
                let is_parent_edge = target == node
                    && match label {
                        EdgeLabel::Contains | EdgeLabel::Defines => true,
                        EdgeLabel::Imports => self.node_is_import_kind(&node),
                        _ => false,
                    };
                if is_parent_edge {
                    self.add_edge(edge_offset, &edge_id);
                    self.add_id_versions(&source);
                    self.offsets
                        .extend(self.view.postings(Table::Adjacency, &source));
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

/// Whether a record marks the graph as a history / corpus store (issue #457
/// composition): it carries temporal provenance (a history-replayed version) or
/// it is a `Commit` node. Either signals that the #457 default HEAD-anchor gate
/// and the other history-view lanes are in effect, so a targeted [`Selector`]
/// closure cannot reproduce the cold answer and the loader must fall back to a
/// whole-file scan. A plain current-tree `scan` graph trips neither predicate.
const fn record_is_history_signal(record: &GraphRecord) -> bool {
    match record {
        GraphRecord::Node { kind, temporal, .. } => {
            temporal.is_some() || matches!(kind, NodeKind::Commit)
        }
        GraphRecord::Edge { temporal, .. } => temporal.is_some(),
        GraphRecord::Tombstone { .. } => false,
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
            start_column: None,
            end_column: None,
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
    fn plain_graph_has_no_temporal_history_flag() {
        // The keep-last current-tree fixture carries no temporal provenance and
        // no Commit node, so the fast path stays enabled (issue #457 composition).
        let g = sample_graph();
        let index = GraphIndex::build_from_bytes(jsonl(&g).as_bytes()).expect("build");
        assert!(!index.body.has_temporal_history);
    }

    #[test]
    fn temporal_record_sets_history_flag() {
        use crate::ir::TemporalMetadata;
        let mut g = Graph::new();
        let repo = stable_id(&["node", "Repository", "r"]);
        g.push(GraphRecord::node(
            repo,
            NodeKind::Repository,
            None,
            None,
            Some("r".to_owned()),
            "Repository r".to_owned(),
        ));
        g.push(
            GraphRecord::syntax_node(
                sym_id("src/a.rs", "s"),
                NodeKind::Symbol,
                "src/a.rs".to_owned(),
                span(1, 3),
                "s".to_owned(),
                "rust",
                "fn s".to_owned(),
            )
            .with_temporal(TemporalMetadata {
                git_commit: "aaaa1111".to_owned(),
                git_parent_commits: vec![],
                valid_time: "2026-01-01T00:00:00Z".to_owned(),
                author_time: None,
                observed_at: "2026-01-01T00:00:00Z".to_owned(),
                valid_time_source: Some("git_commit_committer_date".to_owned()),
            }),
        );
        let index = GraphIndex::build_from_bytes(jsonl(&g).as_bytes()).expect("build");
        assert!(
            index.body.has_temporal_history,
            "a temporal-provenance record marks the graph a history store"
        );
    }

    #[test]
    fn commit_node_sets_history_flag() {
        let mut g = Graph::new();
        g.push(GraphRecord::node(
            stable_id(&["node", "commit", "r", "aaaa1111"]),
            NodeKind::Commit,
            None,
            None,
            Some("aaaa1111".to_owned()),
            "Commit aaaa1111".to_owned(),
        ));
        let index = GraphIndex::build_from_bytes(jsonl(&g).as_bytes()).expect("build");
        assert!(
            index.body.has_temporal_history,
            "a Commit node marks the graph a history store"
        );
    }

    #[test]
    fn empty_graph_builds_empty_index() {
        let index = GraphIndex::build_from_bytes(b"").expect("build");
        assert!(index.body.by_id.is_empty());
        assert_eq!(index.graph_len, 0);
        assert!(!index.body.has_temporal_history);
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

    #[test]
    fn ancestry_does_not_climb_target_side_imports_edge() {
        // Issue #444: a `File —IMPORTS→ Module` target edge is a dependency
        // edge, not containment. Climbing it would misattribute the imported
        // module to the importing file's tree.
        let mut g = Graph::new();
        let repo = stable_id(&["node", "Repository", "r"]);
        g.push(GraphRecord::node(
            repo.clone(),
            NodeKind::Repository,
            None,
            None,
            Some("r".to_owned()),
            "Repository r".to_owned(),
        ));
        // Importing file.
        let importer = file_id("src/importer.rs");
        g.push(GraphRecord::syntax_node(
            importer.clone(),
            NodeKind::File,
            "src/importer.rs".to_owned(),
            span(1, 10),
            "importer.rs".to_owned(),
            "rust",
            "file".to_owned(),
        ));
        g.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo.clone(),
            importer.clone(),
            None,
            "contains".to_owned(),
        ));
        // Imported module (in a different file).
        let module_file = file_id("src/imported.rs");
        g.push(GraphRecord::syntax_node(
            module_file.clone(),
            NodeKind::File,
            "src/imported.rs".to_owned(),
            span(1, 10),
            "imported.rs".to_owned(),
            "rust",
            "file".to_owned(),
        ));
        g.push(GraphRecord::edge(
            EdgeLabel::Contains,
            repo,
            module_file.clone(),
            None,
            "contains".to_owned(),
        ));
        let module_id = stable_id(&["node", "Module", "src/imported.rs", "imported"]);
        g.push(GraphRecord::node(
            module_id.clone(),
            NodeKind::Module,
            Some("src/imported.rs".to_owned()),
            None,
            Some("imported".to_owned()),
            "Module imported".to_owned(),
        ));
        g.push(GraphRecord::edge(
            EdgeLabel::Defines,
            module_file.clone(),
            module_id.clone(),
            None,
            "defines".to_owned(),
        ));
        // The target-side IMPORTS edge: importer file → imported module.
        g.push(GraphRecord::edge(
            EdgeLabel::Imports,
            importer.clone(),
            module_id.clone(),
            Some("1.0".to_owned()),
            "src/importer.rs imports imported".to_owned(),
        ));
        let text = jsonl(&g);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        // Climb from the module: must reach its defining file (via DEFINES)
        // but must NOT cross the IMPORTS edge to the importing file.
        let view = MemView {
            body: &index.body,
            bytes: text.as_bytes(),
        };
        let mut walk = Closure {
            view: &view,
            offsets: BTreeSet::new(),
            expanded: BTreeSet::new(),
            climbed: BTreeSet::new(),
        };
        walk.climb_ancestry(&module_id);
        let offsets = walk.offsets;
        let importer_offsets = index.body.by_id.get(&importer).cloned().unwrap_or_default();
        for off in &importer_offsets {
            assert!(
                !offsets.contains(off),
                "climb_ancestry must not cross File —IMPORTS→ Module to the importer"
            );
        }
        // Sanity: the module's own defining file IS reached (via DEFINES).
        let module_file_offsets = index
            .body
            .by_id
            .get(&module_file)
            .cloned()
            .unwrap_or_default();
        assert!(
            module_file_offsets.iter().any(|off| offsets.contains(off)),
            "module's defining file must be reached via DEFINES"
        );
    }

    /// Writes the sample graph and its index to a temp dir.
    fn write_indexed(graph: &Graph) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("g.jsonl");
        fs::write(&path, jsonl(graph)).expect("write graph");
        GraphIndex::build(&path)
            .expect("build")
            .write_atomic(&index_path_for(&path))
            .expect("write idx");
        (dir, path)
    }

    #[test]
    fn lazy_hydrate_matches_in_memory_closure() {
        let (_dir, path) = write_indexed(&sample_graph());
        let text = fs::read_to_string(&path).expect("read");
        let eager = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let lazy = LazyGraphIndex::open(&path).expect("open");
        let name = Selector::ByName("caller".to_owned());
        let want: Vec<GraphRecord> = eager
            .closure_offsets(&name, text.as_bytes())
            .expect("offsets")
            .into_iter()
            .filter_map(|o| read_record_at(text.as_bytes(), o))
            .collect();
        assert_eq!(lazy.hydrate(&name).expect("hydrate"), Some(want));
        assert_eq!(lazy.hydrate(&Selector::Whole).expect("whole"), None);
    }

    #[test]
    fn touched_file_with_same_bytes_stays_valid() {
        let (_dir, path) = write_indexed(&sample_graph());
        let bytes = fs::read(&path).expect("read");
        fs::write(&path, &bytes).expect("rewrite same bytes");
        assert!(LazyGraphIndex::open(&path).is_ok());
    }

    #[test]
    fn same_length_edit_is_stale() {
        let (_dir, path) = write_indexed(&sample_graph());
        let mut bytes = fs::read(&path).expect("read");
        let at = bytes.len() / 2;
        bytes[at] = if bytes[at] == b'a' { b'b' } else { b'a' };
        fs::write(&path, &bytes).expect("edit in place");
        assert!(matches!(LazyGraphIndex::open(&path), Err(LoadError::Stale)));
    }

    #[test]
    fn damaged_table_fails_hydrate_instead_of_answering() {
        let (_dir, path) = write_indexed(&sample_graph());
        let idx = index_path_for(&path);
        let mut bytes = fs::read(&idx).expect("read idx");
        // Claim an impossible entry count in the by-id table.
        bytes[PREAMBLE_LEN..PREAMBLE_LEN + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        fs::write(&idx, &bytes).expect("damage idx");
        let lazy = LazyGraphIndex::open(&path).expect("header is intact");
        assert!(lazy.hydrate(&Selector::ById("x".to_owned())).is_err());
    }

    #[test]
    fn lean_selectors_are_smaller_than_by_id() {
        let graph = sample_graph();
        let text = jsonl(&graph);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let caller = sym_id("src/a.rs", "caller");
        let count = |selector: Selector| {
            index
                .closure_offsets(&selector, text.as_bytes())
                .expect("offsets")
        };
        let full = count(Selector::ById(caller.clone()));
        let anchored = count(Selector::Anchored(caller.clone()));
        let lineage = count(Selector::Lineage(caller.clone()));
        assert!(lineage.len() <= anchored.len());
        assert!(anchored.len() <= full.len());
        assert!(lineage.iter().all(|o| anchored.contains(o)));
        assert!(anchored.iter().all(|o| full.contains(o)));
        // Both versions of the symbol are in even the smallest closure.
        let versions = index.body.by_id.get(&caller).cloned().unwrap_or_default();
        assert!(versions.iter().all(|o| lineage.contains(o)));
    }

    #[test]
    fn sample_fingerprint_of_file_matches_bytes() {
        let bytes: Vec<u8> = (0..100_000u32).flat_map(u32::to_le_bytes).collect();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("f");
        fs::write(&path, &bytes).expect("write");
        let file = fs::File::open(&path).expect("open");
        assert_eq!(
            sample_fingerprint_file(&file, bytes.len() as u64).expect("fp"),
            sample_fingerprint_bytes(&bytes)
        );
    }

    #[test]
    fn climbed_containment_edge_pulls_its_tombstone() {
        let mut graph = sample_graph();
        let caller = sym_id("src/a.rs", "caller");
        let defines_id = GraphRecord::edge(
            EdgeLabel::Defines,
            file_id("src/a.rs"),
            caller.clone(),
            None,
            "defines".to_owned(),
        )
        .id()
        .to_owned();
        graph.push(GraphRecord::Tombstone {
            id: format!("tomb:{defines_id}"),
            schema_version: 1,
            deleted_id: defines_id.clone(),
            summary: "edge retracted".to_owned(),
            producer: None,
        });
        let text = jsonl(&graph);
        let index = GraphIndex::build_from_bytes(text.as_bytes()).expect("build");
        let tombstone_offsets = index
            .body
            .by_deleted_id
            .get(&defines_id)
            .cloned()
            .expect("tombstone indexed");
        for selector in [Selector::Lineage(caller.clone()), Selector::ById(caller)] {
            let closure = index
                .closure_offsets(&selector, text.as_bytes())
                .expect("offsets");
            assert!(
                tombstone_offsets.iter().all(|o| closure.contains(o)),
                "{selector:?} must carry the edge tombstone"
            );
        }
    }

    /// A graph over 400 KB, so some bytes lie outside every sampled block.
    fn large_graph() -> Graph {
        let mut graph = Graph::new();
        for n in 0..3000 {
            let name = format!("symbol_number_{n:05}");
            graph.push(GraphRecord::syntax_node(
                sym_id("src/big.rs", &name),
                NodeKind::Symbol,
                "src/big.rs".to_owned(),
                span(n + 1, n + 2),
                name.clone(),
                "rust",
                format!("fn {name}"),
            ));
        }
        graph
    }

    #[cfg(unix)]
    #[test]
    fn same_length_edit_with_restored_mtime_is_stale() {
        let (_dir, path) = write_indexed(&large_graph());
        let meta = fs::metadata(&path).expect("meta");
        let mtime = meta.modified().expect("mtime");
        let mut bytes = fs::read(&path).expect("read");
        let ranges = sample_ranges(bytes.len() as u64);
        let at = (bytes.len() / 3..bytes.len())
            .find(|p| {
                let pos = *p as u64;
                ranges
                    .iter()
                    .all(|(o, n)| pos < *o || pos >= *o + *n as u64)
                    && bytes[*p].is_ascii_digit()
            })
            .expect("an unsampled digit");
        bytes[at] = if bytes[at] == b'7' { b'8' } else { b'7' };
        fs::write(&path, &bytes).expect("edit in place");
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open")
            .set_modified(mtime)
            .expect("restore mtime");
        assert!(matches!(LazyGraphIndex::open(&path), Err(LoadError::Stale)));
    }
}
