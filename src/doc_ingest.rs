//! Repo-local design-doc (ADR/PRD/Plan) importer (issue #149).
//!
//! Scans the documented repo-relative doc roots (`docs/adr/`, `docs/prd/`,
//! `docs/plans/`), materializes one artifact-domain node per markdown document
//! using the reserved `ADR` / `PRD` / `PlanDoc` shapes from
//! `docs/schema/agent-actions.md` §10, and links each document to code ONLY
//! through explicit literal references (repo-relative file paths and
//! fully-qualified symbol names) via the existing `RELATES_TO` /
//! `MENTIONS_SYMBOL` edges. No new graph domain or edge vocabulary.
//!
//! Design rules (see `docs/schema/doc-ingest.md`):
//!
//! * Deterministic and filesystem-local: recursive scan of the three exact
//!   roots, sorted by normalized repo-relative path. No network, no crawling.
//! * Kind comes ONLY from the normalized root path or an explicit documented
//!   `kind:` front-matter field. A front-matter/root conflict is a
//!   `kind_mismatch` diagnostic; the path rule wins. Nothing is guessed.
//! * References are explicit literal repo-relative paths and fully-qualified
//!   symbol names, taken from the documented front-matter `references:` /
//!   `symbols:` lists and from backtick-delimited literals in the body.
//!   Unresolvable or ambiguous handles are diagnostics, never invented edges.
//! * Document facts stay artifact trust: the importer never writes codegraph
//!   records and never modifies the supplied code graph.
//! * Record identity is content-addressed: the node id binds kind +
//!   repo-relative path + BLAKE3 content hash, so an unchanged re-import is
//!   byte-identical (given a fixed transaction time) while a body change
//!   yields a new record id and hash, leaving the previous record addressable
//!   as history.
//! * No raw body text ever leaves this module: diagnostics and summaries carry
//!   at most the bounded, redacted title.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::ir::{
    ARTIFACT_SCHEMA_VERSION, EdgeLabel, GraphRecord, NodeKind, Producer, ProducerKind,
    artifact_stable_id,
};
use crate::redaction::redact_value;

// ── Public contract ──────────────────────────────────────────────────────────

/// Documented doc roots in canonical order: (repo-relative root, kind).
///
/// The ONLY roots the importer scans by default, and the ONLY source of a
/// document's kind besides an explicit `kind:` front-matter field.
pub const DOC_ROOTS: [(&str, DocKind); 3] = [
    ("docs/adr", DocKind::Adr),
    ("docs/prd", DocKind::Prd),
    ("docs/plans", DocKind::Plan),
];

/// Maximum title length in characters. Longer titles are truncated at a
/// character boundary and suffixed with `…`.
pub const TITLE_CHAR_BOUND: usize = 200;

/// `source_format_version` stamped on the [`ProducerKind::DocImporter`]
/// envelope.
pub const DOC_SOURCE_FORMAT: &str = "doc-ingest/v1";

/// Design-doc kind. The mapping from root path is total and closed: a doc
/// under `docs/adr/` is an ADR, full stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKind {
    /// Architecture decision record (`docs/adr/` → `NodeKind::Adr`).
    Adr,
    /// Product requirements document (`docs/prd/` → `NodeKind::Prd`).
    Prd,
    /// Plan document (`docs/plans/` → `NodeKind::PlanDoc`).
    Plan,
}

impl DocKind {
    /// Filesystem slug used in stable IDs (`adr`, `prd`, `plan`).
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Adr => "adr",
            Self::Prd => "prd",
            Self::Plan => "plan",
        }
    }

    /// Wire name of the reserved artifact shape (`ADR`, `PRD`, `PlanDoc`).
    ///
    /// `PlanDoc` — not `Plan` — because `Plan` is already the project-domain
    /// task-plan kind; the wire name disambiguates on the artifact domain.
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Adr => "ADR",
            Self::Prd => "PRD",
            Self::Plan => "PlanDoc",
        }
    }

    /// The [`NodeKind`] this doc kind materializes as.
    #[must_use]
    pub const fn node_kind(self) -> NodeKind {
        match self {
            Self::Adr => NodeKind::Adr,
            Self::Prd => NodeKind::Prd,
            Self::Plan => NodeKind::PlanDoc,
        }
    }

    /// Parses the documented `kind:` front-matter values (`adr`/`ADR`,
    /// `prd`/`PRD`, `plan`/`Plan`). Anything else is `None` — and therefore a
    /// `malformed_document` diagnostic, never a guess.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "adr" => Some(Self::Adr),
            "prd" => Some(Self::Prd),
            "plan" => Some(Self::Plan),
            _ => None,
        }
    }
}

/// The documented default roots as owned strings.
#[must_use]
pub fn default_roots() -> Vec<String> {
    DOC_ROOTS.iter().map(|(r, _)| (*r).to_owned()).collect()
}

/// Options controlling the doc import.
#[derive(Debug, Clone, Default)]
pub struct DocIngestOptions {
    /// Repository root the doc roots are resolved against.
    pub repo_root: PathBuf,
    /// Repo-relative doc roots to scan. Empty means [`default_roots`].
    /// Explicit roots outside the documented set, or not resolving to a
    /// directory, yield `unknown_root` diagnostics. Absent *default* roots
    /// are skipped silently: most repos do not carry all three.
    pub roots: Vec<String>,
    /// Code-graph records used to resolve explicit references. Only
    /// `File` (by `repo_relative_path`) and `Symbol` (by `name`) nodes are
    /// indexed; everything else is ignored. The graph is never mutated.
    pub code_graph: Vec<GraphRecord>,
    /// Fixed RFC 3339 transaction timestamp for deterministic output.
    /// `None` leaves records unstamped.
    pub transaction_time: Option<String>,
}

/// A stable machine-readable diagnostic. Carries at most the bounded,
/// redacted title — never raw body text.
#[derive(Debug, Clone, Serialize)]
pub struct DocIngestDiagnostic {
    /// Stable code: `unknown_root`, `empty_document`, `malformed_document`,
    /// `kind_mismatch`, `unresolved_reference`, `ambiguous_reference`,
    /// `zero_resolvable_references`.
    pub code: &'static str,
    /// Repo-relative doc path (`""` when the diagnostic is not about a doc,
    /// e.g. `unknown_root`).
    pub doc: String,
    /// The offending reference handle, when the diagnostic is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// Bounded, redacted doc title, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Human-readable explanation. Contains no body text.
    pub message: String,
}

/// Output of [`import_docs`]: artifact records plus diagnostics.
#[derive(Debug, Default)]
pub struct DocIngestResult {
    /// One node per ingested doc plus one edge per resolved explicit
    /// reference, in deterministic order (docs sorted by repo-relative path;
    /// edges grouped per doc, file refs before symbol refs, handles sorted).
    pub records: Vec<GraphRecord>,
    /// Stable machine-readable diagnostics in emission order.
    pub diagnostics: Vec<DocIngestDiagnostic>,
}

/// One explicitly-linked design doc surfaced by a symbol or subsystem query.
///
/// This is a view over already-imported records: it carries the artifact
/// record id, kind, repo-relative path, and BLAKE3 content hash — never body
/// text. An empty input yields an empty vec (honest empty, never fabricated).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesignDocLink<'a> {
    /// Stable artifact-domain record id of the doc node.
    pub record_id: &'a str,
    /// Wire kind name (`ADR`, `PRD`, `PlanDoc`).
    pub doc_kind: &'static str,
    /// Bounded, redacted title, if the doc has one.
    pub title: Option<&'a str>,
    /// Repo-relative doc path.
    pub repo_relative_path: &'a str,
    /// BLAKE3 hex of the raw doc bytes at import time.
    pub content_hash: &'a str,
    /// `MENTIONS_SYMBOL` or `RELATES_TO`.
    pub relation: &'static str,
}

// ── Import ───────────────────────────────────────────────────────────────────

/// Process-start timestamp stamped on the producer envelope when no fixed
/// transaction time is supplied (mirrors `local_project`).
static IMPORTER_STARTED_AT: LazyLock<String> =
    LazyLock::new(|| chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));

fn doc_importer_producer(transaction_time: Option<&str>) -> Producer {
    Producer {
        egregore_version: env!("CARGO_PKG_VERSION").to_owned(),
        egregore_git: None,
        producer_kind: ProducerKind::DocImporter,
        producer_components: BTreeMap::from([
            (
                "importer_schema_version".to_owned(),
                ARTIFACT_SCHEMA_VERSION.to_string(),
            ),
            (
                "source_format_version".to_owned(),
                DOC_SOURCE_FORMAT.to_owned(),
            ),
        ]),
        producer_started_at: transaction_time
            .map_or_else(|| IMPORTER_STARTED_AT.clone(), str::to_owned),
    }
}

/// Imports design docs per [`DocIngestOptions`].
///
/// Pure with respect to the graph: reads the filesystem under `repo_root`,
/// never the network, never mutates `code_graph`.
///
/// # Errors
///
/// Returns an error when a doc root cannot be listed, a doc file cannot be
/// read, or an explicit `--root` names a path outside the documented roots.
pub fn import_docs(opts: &DocIngestOptions) -> Result<DocIngestResult> {
    let mut result = DocIngestResult::default();
    let producer = doc_importer_producer(opts.transaction_time.as_deref());

    let file_index = build_file_index(&opts.code_graph);
    let symbol_index = build_symbol_index(&opts.code_graph);

    // Deterministic root order: sorted, deduped. An empty `roots` means the
    // documented defaults; absent default roots are skipped silently below.
    let using_defaults = opts.roots.is_empty();
    let mut roots: Vec<&str> = if using_defaults {
        DOC_ROOTS.iter().map(|(root, _)| *root).collect()
    } else {
        opts.roots.iter().map(String::as_str).collect()
    };
    roots.sort_unstable();
    roots.dedup();

    for root in roots {
        let Some(kind) = DOC_ROOTS
            .iter()
            .find(|(documented, _)| *documented == root)
            .map(|(_, kind)| *kind)
        else {
            result.diagnostics.push(DocIngestDiagnostic {
                code: "unknown_root",
                doc: root.to_owned(),
                handle: None,
                title: None,
                message: format!(
                    "unknown doc root '{root}'; documented roots are docs/adr, docs/prd, docs/plans"
                ),
            });
            continue;
        };
        let root_dir = opts.repo_root.join(root);
        if !root_dir.is_dir() {
            if using_defaults {
                continue;
            }
            result.diagnostics.push(DocIngestDiagnostic {
                code: "unknown_root",
                doc: root.to_owned(),
                handle: None,
                title: None,
                message: format!("doc root '{root}' does not resolve to a directory in this repo"),
            });
            continue;
        }
        let mut rel_paths = Vec::new();
        collect_markdown(&root_dir, &opts.repo_root, &mut rel_paths)
            .with_context(|| format!("failed to scan doc root '{root}'"))?;
        rel_paths.sort();
        for rel in rel_paths {
            ingest_one(
                opts,
                &producer,
                kind,
                &rel,
                &file_index,
                &symbol_index,
                &mut result,
            )
            .with_context(|| format!("failed to ingest doc '{rel}'"))?;
        }
    }
    Ok(result)
}

/// Builds the exact file-path index: repo-relative path → codegraph file ids.
fn build_file_index(code_graph: &[GraphRecord]) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in code_graph {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::File,
            repo_relative_path: Some(path),
            ..
        } = record
        {
            index.entry(path.clone()).or_default().push(id.clone());
        }
    }
    index
}

/// Builds the exact symbol-name index: fully-qualified name → symbol ids.
fn build_symbol_index(code_graph: &[GraphRecord]) -> BTreeMap<String, Vec<String>> {
    let mut index: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for record in code_graph {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: Some(name),
            ..
        } = record
        {
            index.entry(name.clone()).or_default().push(id.clone());
        }
    }
    index
}

/// Recursively collects `.md` files under `dir` as normalized repo-relative
/// paths (forward slashes). Directory entries are visited in sorted order so
/// the scan is deterministic.
fn collect_markdown(dir: &Path, repo_root: &Path, out: &mut Vec<String>) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("failed to read directory {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("failed to list directory {}", dir.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        // Inspect the entry WITHOUT following links: a symlinked directory
        // could import Markdown from outside the repository under a synthetic
        // repo-relative path, or loop on a link to an ancestor. Symlinks are
        // skipped entirely (directories and files alike).
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to stat {}", path.display()))?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_markdown(&path, repo_root, out)?;
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "md")
        {
            let rel = path
                .strip_prefix(repo_root)
                .with_context(|| format!("doc path {} is not under repo root", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            out.push(rel);
        }
    }
    Ok(())
}

// `ingest_one` is long because each malformed shape is an explicit,
// documented diagnostic branch; splitting it would scatter the
// per-document contract. (Codebase convention: `#[allow]` over refactor.)
#[allow(clippy::too_many_lines)]
fn ingest_one(
    opts: &DocIngestOptions,
    producer: &Producer,
    kind: DocKind,
    rel_path: &str,
    file_index: &BTreeMap<String, Vec<String>>,
    symbol_index: &BTreeMap<String, Vec<String>>,
    result: &mut DocIngestResult,
) -> Result<()> {
    let raw = fs::read(opts.repo_root.join(rel_path))
        .with_context(|| format!("failed to read doc '{rel_path}'"))?;
    let content_hash = blake3::hash(&raw).to_hex().to_string();

    let Ok(text) = std::str::from_utf8(&raw) else {
        result.diagnostics.push(DocIngestDiagnostic {
            code: "malformed_document",
            doc: rel_path.to_owned(),
            handle: None,
            title: None,
            message: format!("doc '{rel_path}' is not valid UTF-8"),
        });
        return Ok(());
    };
    if text.trim().is_empty() {
        result.diagnostics.push(DocIngestDiagnostic {
            code: "empty_document",
            doc: rel_path.to_owned(),
            handle: None,
            title: None,
            message: format!("doc '{rel_path}' is empty or whitespace-only"),
        });
        return Ok(());
    }
    let parsed = match parse_doc(text) {
        Ok(parsed) => parsed,
        Err(reason) => {
            result.diagnostics.push(DocIngestDiagnostic {
                code: "malformed_document",
                doc: rel_path.to_owned(),
                handle: None,
                title: None,
                message: format!("doc '{rel_path}' has malformed front matter: {reason}"),
            });
            return Ok(());
        }
    };

    // Kind comes from the path; an explicit front-matter kind that disagrees
    // is a diagnostic, never an override.
    if let Some(declared) = parsed.kind
        && declared != kind
    {
        result.diagnostics.push(DocIngestDiagnostic {
            code: "kind_mismatch",
            doc: rel_path.to_owned(),
            handle: None,
            title: None,
            message: format!(
                "doc '{rel_path}' declares kind '{}' but lives under the '{}' root; path rule wins",
                declared.slug(),
                kind.slug(),
            ),
        });
    }

    let title = parsed
        .title
        .or_else(|| first_h1(&parsed.body))
        .map(|t| bound_title(&redact_value(t.trim())))
        .filter(|t| !t.is_empty());

    let node_id = artifact_stable_id(&["doc", kind.slug(), rel_path, content_hash.as_str()]);
    let summary = format!(
        "{} {}",
        kind.wire_name(),
        title.as_deref().unwrap_or("(untitled)")
    );
    // The node and its edges are staged in `pending` and only emitted when at
    // least one explicit reference resolves: a document with zero resolvable
    // references is a diagnostic and emits no record (issue #149).
    let mut pending: Vec<GraphRecord> = Vec::new();
    let mut node = GraphRecord::node(
        node_id.clone(),
        kind.node_kind(),
        Some(rel_path.to_owned()),
        None,
        title.clone(),
        summary,
    )
    .with_domain("artifact", ARTIFACT_SCHEMA_VERSION)
    .with_source_artifact_hash(&content_hash)
    .with_producer(producer.clone());
    if let Some(t) = title.clone() {
        node = node.with_title(t);
    }
    if let Some(tx) = opts.transaction_time.as_deref() {
        node = node.with_transaction_time(tx);
    }
    pending.push(node);

    // Explicit literal references only: front-matter lists plus backtick
    // literals in the body. Deduped and sorted for determinism.
    let mut file_handles: BTreeSet<String> = parsed.file_refs.into_iter().collect();
    let mut symbol_handles: BTreeSet<String> = parsed.symbol_refs.into_iter().collect();
    let (body_files, body_symbols) = body_refs(&parsed.body);
    file_handles.extend(body_files);
    symbol_handles.extend(body_symbols);

    let mut resolved = 0usize;
    let mut link = |handle: &str,
                    index: &BTreeMap<String, Vec<String>>,
                    label: EdgeLabel,
                    pending: &mut Vec<GraphRecord>,
                    result: &mut DocIngestResult| {
        match index.get(handle).map(Vec::as_slice) {
            Some([target]) => {
                let mut edge = GraphRecord::artifact_edge(
                    label,
                    node_id.clone(),
                    target.clone(),
                    None,
                    format!(
                        "doc '{rel_path}' explicitly references '{handle}' ({}:{})",
                        label.as_str(),
                        kind.wire_name(),
                    ),
                )
                .with_producer(producer.clone());
                if let Some(tx) = opts.transaction_time.as_deref() {
                    edge = edge.with_transaction_time(tx);
                }
                pending.push(edge);
                resolved += 1;
            }
            Some(_) => {
                result.diagnostics.push(DocIngestDiagnostic {
                    code: "ambiguous_reference",
                    doc: rel_path.to_owned(),
                    handle: Some(handle.to_owned()),
                    title: title.clone(),
                    message: format!(
                        "reference '{handle}' in doc '{rel_path}' matches multiple codegraph records; not linked"
                    ),
                });
            }
            None => {
                result.diagnostics.push(DocIngestDiagnostic {
                    code: "unresolved_reference",
                    doc: rel_path.to_owned(),
                    handle: Some(handle.to_owned()),
                    title: title.clone(),
                    message: format!(
                        "reference '{handle}' in doc '{rel_path}' matches no codegraph record; not linked"
                    ),
                });
            }
        }
    };
    for handle in &file_handles {
        link(
            handle,
            file_index,
            EdgeLabel::RelatesTo,
            &mut pending,
            result,
        );
    }
    for handle in &symbol_handles {
        link(
            handle,
            symbol_index,
            EdgeLabel::MentionsSymbol,
            &mut pending,
            result,
        );
    }

    if resolved == 0 {
        result.diagnostics.push(DocIngestDiagnostic {
            code: "zero_resolvable_references",
            doc: rel_path.to_owned(),
            handle: None,
            title: title.clone(),
            message: format!(
                "doc '{rel_path}' has no resolvable explicit references to the code graph"
            ),
        });
    } else {
        result.records.extend(pending);
    }
    Ok(())
}

// ── Front matter & body parsing ──────────────────────────────────────────────

struct ParsedDoc {
    title: Option<String>,
    kind: Option<DocKind>,
    file_refs: Vec<String>,
    symbol_refs: Vec<String>,
    body: String,
}

/// Splits optional `---`-delimited front matter off the head of the doc.
///
/// Returns `(front_matter, body)`; `front_matter` is `None` when the doc does
/// not start with a `---` line. An unterminated block is an error.
fn split_front_matter(text: &str) -> Result<(Option<&str>, &str), &'static str> {
    let first_end = text.find('\n').map_or(text.len(), |i| i + 1);
    if text[..first_end].trim() != "---" {
        return Ok((None, text));
    }
    let mut offset = first_end;
    for line in text[first_end..].lines() {
        // `lines()` strips the trailing '\n'; guard the EOF case.
        let line_end = (offset + line.len() + 1).min(text.len());
        if line.trim() == "---" {
            return Ok((Some(&text[first_end..offset]), &text[line_end..]));
        }
        offset = line_end;
    }
    Err("unterminated front matter")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListTarget {
    Files,
    Symbols,
}

/// Strips one layer of matching single/double quotes.
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')))
    {
        s[1..s.len() - 1].to_owned()
    } else {
        s.to_owned()
    }
}

/// Parses the documented front-matter subset:
///
/// ```yaml
/// ---
/// title: Human title
/// kind: adr        # adr | prd | plan; must agree with the root path
/// references:      # literal repo-relative file paths
///   - src/ir.rs
/// symbols:         # fully-qualified symbol names
///   - ir::GraphRecord
/// ---
/// ```
///
/// Unknown keys are ignored (forward-compatible). Any other shape is an
/// error — the importer never guesses.
fn parse_front_matter(fm: &str) -> Result<ParsedDoc, &'static str> {
    let mut title: Option<String> = None;
    let mut kind: Option<DocKind> = None;
    let mut file_refs = Vec::new();
    let mut symbol_refs = Vec::new();
    let mut list_target: Option<ListTarget> = None;

    for raw_line in fm.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(item) = line.strip_prefix('-').map(str::trim) {
            let item = unquote(item);
            if item.is_empty() {
                return Err("empty list item");
            }
            match list_target {
                Some(ListTarget::Files) => file_refs.push(item),
                Some(ListTarget::Symbols) => symbol_refs.push(item),
                None => return Err("list item outside a references:/symbols: list"),
            }
            continue;
        }
        list_target = None;
        let (key, value) = raw_line.split_once(':').ok_or("line without ':'")?;
        match key.trim() {
            "title" => {
                let value = unquote(value);
                if value.is_empty() {
                    return Err("empty title");
                }
                title = Some(value);
            }
            "kind" => {
                kind = Some(DocKind::parse(value).ok_or("unknown kind value")?);
            }
            "references" => {
                let value = unquote(value);
                if value.is_empty() {
                    list_target = Some(ListTarget::Files);
                } else {
                    file_refs.push(value);
                }
            }
            "symbols" => {
                let value = unquote(value);
                if value.is_empty() {
                    list_target = Some(ListTarget::Symbols);
                } else {
                    symbol_refs.push(value);
                }
            }
            _ => {}
        }
    }
    Ok(ParsedDoc {
        title,
        kind,
        file_refs,
        symbol_refs,
        body: String::new(),
    })
}

fn parse_doc(text: &str) -> Result<ParsedDoc, &'static str> {
    let (fm, body) = split_front_matter(text)?;
    let mut parsed = match fm {
        Some(fm) => parse_front_matter(fm)?,
        None => ParsedDoc {
            title: None,
            kind: None,
            file_refs: Vec::new(),
            symbol_refs: Vec::new(),
            body: String::new(),
        },
    };
    (*body).clone_into(&mut parsed.body);
    Ok(parsed)
}

/// A backtick literal is a file-path reference when it looks like a path
/// (contains `/` or `.`); a symbol reference when it is fully qualified
/// (contains `::`). Anything else is prose, not a reference.
fn classify_literal(token: &str) -> Option<ListTarget> {
    if token.contains("::") {
        Some(ListTarget::Symbols)
    } else if token.contains('/') || token.contains('.') {
        Some(ListTarget::Files)
    } else {
        None
    }
}

/// Normalizes a path handle for exact index lookup: forward slashes, no
/// leading `./` or `/`. `..` segments are NOT resolved — such a handle simply
/// matches nothing and becomes an `unresolved_reference` diagnostic.
fn normalize_path_handle(token: &str) -> String {
    let mut handle = token.replace('\\', "/");
    while let Some(rest) = handle.strip_prefix("./") {
        handle = rest.to_owned();
    }
    if let Some(stripped) = handle.strip_prefix('/') {
        handle = stripped.to_owned();
    }
    handle
}

/// Extracts explicit literal references from backtick-delimited spans in the
/// body. Returns `(file_handles, symbol_handles)`, each sorted and deduped.
fn body_refs(body: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut files = BTreeSet::new();
    let mut symbols = BTreeSet::new();
    let mut in_code = false;
    for segment in body.split('`') {
        if in_code {
            let token = segment.trim();
            if !token.is_empty() {
                match classify_literal(token) {
                    Some(ListTarget::Files) => {
                        let handle = normalize_path_handle(token);
                        if !handle.is_empty() {
                            files.insert(handle);
                        }
                    }
                    Some(ListTarget::Symbols) => {
                        symbols.insert(token.to_owned());
                    }
                    None => {}
                }
            }
        }
        in_code = !in_code;
    }
    (files, symbols)
}

/// First `# ` heading of the body, if any.
fn first_h1(body: &str) -> Option<String> {
    for line in body.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#')
            && (rest.starts_with(' ') || rest.is_empty())
        {
            let heading = rest.trim();
            if !heading.is_empty() {
                return Some(heading.to_owned());
            }
        }
    }
    None
}

/// Bounds a title to [`TITLE_CHAR_BOUND`] characters at a char boundary.
fn bound_title(title: &str) -> String {
    if title.chars().count() <= TITLE_CHAR_BOUND {
        return title.to_owned();
    }
    let truncated: String = title.chars().take(TITLE_CHAR_BOUND).collect();
    format!("{truncated}…")
}

// ── Query helper ─────────────────────────────────────────────────────────────

/// Surfaces explicitly-linked design docs for a symbol or subsystem query.
///
/// Scans `records` for `ADR`/`PRD`/`PlanDoc` nodes and keeps the
/// `MENTIONS_SYMBOL` / `RELATES_TO` edges whose target is in `symbol_ids` /
/// `file_ids` respectively. Each returned row carries the artifact record id,
/// repo-relative path, and BLAKE3 content hash — never body text. With no
/// governing docs the result is empty: honest empty, never fabricated.
#[must_use]
pub fn linked_design_docs<'a>(
    records: &'a [GraphRecord],
    symbol_ids: &BTreeSet<&str>,
    file_ids: &BTreeSet<&str>,
) -> Vec<DesignDocLink<'a>> {
    struct DocNode<'a> {
        kind: NodeKind,
        title: Option<&'a str>,
        repo_relative_path: Option<&'a str>,
        content_hash: Option<&'a str>,
    }
    let mut docs: BTreeMap<&'a str, DocNode<'a>> = BTreeMap::new();
    for record in records {
        if let GraphRecord::Node {
            id,
            kind,
            title,
            repo_relative_path,
            source_artifact_hash,
            ..
        } = record
            && matches!(kind, NodeKind::Adr | NodeKind::Prd | NodeKind::PlanDoc)
        {
            docs.insert(
                id.as_str(),
                DocNode {
                    kind: *kind,
                    title: title.as_deref(),
                    repo_relative_path: repo_relative_path.as_deref(),
                    content_hash: source_artifact_hash.as_deref(),
                },
            );
        }
    }

    let mut out = Vec::new();
    for record in records {
        if let GraphRecord::Edge {
            label,
            source,
            target,
            ..
        } = record
        {
            let wanted = match label {
                EdgeLabel::MentionsSymbol => symbol_ids.contains(target.as_str()),
                EdgeLabel::RelatesTo => file_ids.contains(target.as_str()),
                _ => false,
            };
            if !wanted {
                continue;
            }
            if let Some(doc) = docs.get(source.as_str()) {
                out.push(DesignDocLink {
                    record_id: source.as_str(),
                    doc_kind: doc.kind.as_str(),
                    title: doc.title,
                    repo_relative_path: doc.repo_relative_path.unwrap_or(""),
                    content_hash: doc.content_hash.unwrap_or(""),
                    relation: label.as_str(),
                });
            }
        }
    }
    out.sort_by(|a, b| (a.record_id, a.relation).cmp(&(b.record_id, b.relation)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const TX_TIME: &str = "2026-09-28T12:00:00Z";

    fn file_node(id: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::File,
            Some(path.to_owned()),
            None,
            Some(path.to_owned()),
            format!("file {path}"),
        )
    }

    fn symbol_node(id: &str, name: &str, path: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Symbol,
            Some(path.to_owned()),
            None,
            Some(name.to_owned()),
            format!("symbol {name}"),
        )
    }

    fn code_graph() -> Vec<GraphRecord> {
        vec![
            file_node("codegraph:v9:file:src/ir.rs", "src/ir.rs"),
            symbol_node(
                "codegraph:v9:sym:ir::GraphRecord",
                "ir::GraphRecord",
                "src/ir.rs",
            ),
        ]
    }

    fn write_doc(repo: &TempDir, rel: &str, body: &str) {
        let path = repo.path().join(rel);
        fs::create_dir_all(path.parent().expect("doc has a parent dir"))
            .expect("create doc parent dirs");
        fs::write(&path, body).expect("write fixture doc");
    }

    fn default_opts(repo: &TempDir) -> DocIngestOptions {
        DocIngestOptions {
            repo_root: repo.path().to_path_buf(),
            roots: Vec::new(),
            code_graph: code_graph(),
            transaction_time: Some(TX_TIME.to_owned()),
        }
    }

    fn node_of(result: &DocIngestResult) -> Vec<&GraphRecord> {
        result
            .records
            .iter()
            .filter(|r| matches!(r, GraphRecord::Node { .. }))
            .collect()
    }

    fn edges_of(result: &DocIngestResult) -> Vec<&GraphRecord> {
        result
            .records
            .iter()
            .filter(|r| matches!(r, GraphRecord::Edge { .. }))
            .collect()
    }

    #[test]
    fn ingests_adr_with_explicit_references() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/adr/0001-use-blake3.md",
            "---\ntitle: Use BLAKE3 for hashing\nreferences:\n  - src/ir.rs\nsymbols:\n  - ir::GraphRecord\n---\n\n# Use BLAKE3 for hashing\n\nWe hash in `src/ir.rs` via `ir::GraphRecord`.\n",
        );

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(
            result.diagnostics.is_empty(),
            "unexpected diagnostics: {:?}",
            result.diagnostics
        );
        let nodes = node_of(&result);
        let edges = edges_of(&result);
        assert_eq!(nodes.len(), 1, "one doc node");
        // Front-matter and body references dedupe to one edge per target.
        assert_eq!(edges.len(), 2, "one RELATES_TO + one MENTIONS_SYMBOL");

        let GraphRecord::Node {
            id,
            kind,
            schema_version,
            repo_relative_path,
            title,
            source_artifact_hash,
            producer,
            ..
        } = nodes[0]
        else {
            panic!("expected a node record");
        };
        assert_eq!(*kind, NodeKind::Adr);
        assert!(
            id.starts_with("artifact:v1:"),
            "stable artifact-domain id, got {id}"
        );
        assert_eq!(*schema_version, ARTIFACT_SCHEMA_VERSION);
        assert_eq!(
            repo_relative_path.as_deref(),
            Some("docs/adr/0001-use-blake3.md")
        );
        assert_eq!(title.as_deref(), Some("Use BLAKE3 for hashing"));
        let raw = fs::read(repo.path().join("docs/adr/0001-use-blake3.md")).expect("read back");
        let expected_hash = blake3::hash(&raw).to_hex().to_string();
        assert_eq!(
            source_artifact_hash.as_deref(),
            Some(expected_hash.as_str()),
            "content hash is BLAKE3 of raw bytes"
        );
        let producer = producer.as_ref().expect("producer envelope");
        assert_eq!(producer.producer_kind, ProducerKind::DocImporter);

        // Wire form carries domain: artifact.
        let wire: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(nodes[0]).expect("serialize"))
                .expect("wire json");
        assert_eq!(wire["domain"], "artifact");
        assert_eq!(wire["kind"], "ADR");

        let mut labels: Vec<&str> = edges
            .iter()
            .filter_map(|e| match e {
                GraphRecord::Edge { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        labels.sort_unstable();
        assert_eq!(labels, ["MENTIONS_SYMBOL", "RELATES_TO"]);
    }

    #[test]
    fn reingest_is_byte_identical_noop() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/prd/0001-vision.md",
            "# Vision\n\nSee `src/ir.rs`.\n",
        );

        let first = import_docs(&default_opts(&repo)).expect("first import");
        let second = import_docs(&default_opts(&repo)).expect("second import");

        let to_jsonl = |r: &DocIngestResult| {
            r.records
                .iter()
                .map(|rec| serde_json::to_string(rec).expect("serialize"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(
            to_jsonl(&first),
            to_jsonl(&second),
            "re-ingest over unchanged docs is a byte-identical no-op"
        );
        let mut ids: Vec<&str> = first.records.iter().map(GraphRecord::id).collect();
        ids.sort_unstable();
        let deduped = {
            let mut v = ids.clone();
            v.dedup();
            v
        };
        assert_eq!(ids, deduped, "no duplicate record ids");
    }

    #[test]
    fn changed_body_yields_new_id_and_hash_history_remains() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/adr/0001-use-blake3.md",
            "# Title\n\nSee `src/ir.rs`.\n",
        );
        let before = import_docs(&default_opts(&repo)).expect("first import");

        write_doc(
            &repo,
            "docs/adr/0001-use-blake3.md",
            "# Title\n\nSee `src/ir.rs`.\n\nAn extra paragraph.\n",
        );
        let after = import_docs(&default_opts(&repo)).expect("second import");

        let hash_of = |r: &DocIngestResult| match &r.records[0] {
            GraphRecord::Node {
                id,
                source_artifact_hash,
                ..
            } => (id.clone(), source_artifact_hash.clone().unwrap_or_default()),
            _ => panic!("expected node"),
        };
        let (id_before, hash_before) = hash_of(&before);
        let (id_after, hash_after) = hash_of(&after);
        assert_ne!(id_before, id_after, "changed content yields a new record");
        assert_ne!(hash_before, hash_after, "changed content yields a new hash");
        // History remains: both versions coexist addressably.
        let mut combined = before.records;
        combined.extend(after.records);
        let ids: BTreeSet<&str> = combined.iter().map(GraphRecord::id).collect();
        assert!(ids.contains(id_before.as_str()), "old record remains");
        assert!(ids.contains(id_after.as_str()), "new record present");
    }

    #[test]
    fn unresolved_reference_yields_diagnostic_not_edge() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/adr/0001-ghost.md",
            "# Ghost\n\nSee `src/does-not-exist.rs`.\n",
        );

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(edges_of(&result).is_empty(), "no invented edges");
        assert!(
            result.records.is_empty(),
            "doc with no resolvable refs emits no record: {:?}",
            result.records
        );
        let codes: Vec<&str> = result.diagnostics.iter().map(|d| d.code).collect();
        assert!(
            codes.contains(&"unresolved_reference"),
            "unresolved handle is a diagnostic, got {codes:?}"
        );
        let diag = result
            .diagnostics
            .iter()
            .find(|d| d.code == "unresolved_reference")
            .expect("diagnostic present");
        assert_eq!(diag.handle.as_deref(), Some("src/does-not-exist.rs"));
        assert_eq!(diag.doc, "docs/adr/0001-ghost.md");
    }

    #[test]
    fn ambiguous_symbol_reference_yields_diagnostic_not_edge() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/adr/0001-amb.md",
            "# Ambiguous\n\nSee `dup::Thing`.\n",
        );
        let mut opts = default_opts(&repo);
        opts.code_graph.push(symbol_node(
            "codegraph:v9:sym:a::dup::Thing",
            "dup::Thing",
            "src/a.rs",
        ));
        opts.code_graph.push(symbol_node(
            "codegraph:v9:sym:b::dup::Thing",
            "dup::Thing",
            "src/b.rs",
        ));
        // NOTE: the fixture name is `dup::Thing` in both; resolution keys on
        // the symbol `name` field, so both collide.
        let result = import_docs(&opts).expect("import succeeds");

        assert!(edges_of(&result).is_empty(), "ambiguous refs never link");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "ambiguous_reference"),
            "ambiguity is a diagnostic: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn empty_doc_yields_diagnostic() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(&repo, "docs/adr/0001-empty.md", "");

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(result.records.is_empty(), "empty doc emits no record");
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "empty_document"),
            "empty doc is a diagnostic: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn malformed_front_matter_yields_diagnostic() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(&repo, "docs/adr/0001-bad.md", "---\ntitle: Never closed\n");

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(result.records.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "malformed_document"),
            "unterminated front matter is malformed: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn unknown_root_yields_diagnostic() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(&repo, "docs/adr/0001-x.md", "# X\n");

        let mut opts = default_opts(&repo);
        opts.roots = vec!["docs/nope".to_owned()];

        let result = import_docs(&opts).expect("import succeeds");

        assert!(result.records.is_empty());
        assert!(
            result.diagnostics.iter().any(|d| d.code == "unknown_root"),
            "unknown root is a diagnostic: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn absent_default_roots_are_skipped_silently() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(&repo, "docs/adr/0001-x.md", "# X\n\nRef `src/ir.rs`.\n");
        // docs/prd and docs/plans are absent; defaults must not diagnose.

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(
            result.diagnostics.is_empty(),
            "absent default roots stay silent: {:?}",
            result.diagnostics
        );
        assert_eq!(result.records.len(), 2, "node + one RELATES_TO edge");
    }

    #[test]
    fn zero_resolvable_references_yields_diagnostic() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/plans/0001-lonely.md",
            "# Lonely plan\n\nNo links.\n",
        );

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(
            result.records.is_empty(),
            "doc with zero resolvable refs emits no record: {:?}",
            result.records
        );
        assert!(edges_of(&result).is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "zero_resolvable_references"),
            "doc with zero resolvable refs is a diagnostic: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn kind_comes_from_path_never_guessed() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(
            &repo,
            "docs/prd/0001-thing.md",
            "---\nkind: adr\n---\n\n# Thing\n\nSee `src/ir.rs`.\n",
        );

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        let nodes = node_of(&result);
        assert_eq!(nodes.len(), 1);
        let GraphRecord::Node { kind, .. } = nodes[0] else {
            panic!("expected node");
        };
        assert_eq!(*kind, NodeKind::Prd, "path rule wins over front matter");
        assert!(
            result.diagnostics.iter().any(|d| d.code == "kind_mismatch"),
            "conflict is a diagnostic: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn invalid_front_matter_kind_is_malformed() {
        let repo = TempDir::new().expect("temp repo");
        write_doc(&repo, "docs/adr/0001-x.md", "---\nkind: rfc\n---\n\n# X\n");

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        assert!(result.records.is_empty());
        assert!(
            result
                .diagnostics
                .iter()
                .any(|d| d.code == "malformed_document"),
            "unknown kind value is malformed: {:?}",
            result.diagnostics
        );
    }

    #[test]
    fn linked_design_docs_surfaces_rows() {
        let doc_id = "artifact:v1:doc:adr:docs/adr/0001-x.md";
        let sym_id = "codegraph:v9:sym:ir::GraphRecord";
        let file_id = "codegraph:v9:file:src/ir.rs";
        let mut records = vec![
            GraphRecord::node(
                doc_id.to_owned(),
                NodeKind::Adr,
                Some("docs/adr/0001-x.md".to_owned()),
                None,
                Some("Doc title".to_owned()),
                "ADR Doc title".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::MentionsSymbol,
                doc_id.to_owned(),
                sym_id.to_owned(),
                None,
                "doc mentions symbol".to_owned(),
            ),
            GraphRecord::edge(
                EdgeLabel::RelatesTo,
                doc_id.to_owned(),
                file_id.to_owned(),
                None,
                "doc relates to file".to_owned(),
            ),
        ];
        // Stamp the content hash the importer would have computed.
        for r in &mut records {
            if let GraphRecord::Node {
                source_artifact_hash,
                ..
            } = r
            {
                *source_artifact_hash = Some("abc123hash".to_owned());
            }
        }

        let rows = linked_design_docs(
            &records,
            &BTreeSet::from([sym_id]),
            &BTreeSet::from([file_id]),
        );
        assert_eq!(rows.len(), 2, "one row per explicit edge");
        for row in &rows {
            assert_eq!(row.record_id, doc_id);
            assert_eq!(row.repo_relative_path, "docs/adr/0001-x.md");
            assert_eq!(row.content_hash, "abc123hash");
        }
        let relations: BTreeSet<&str> = rows.iter().map(|r| r.relation).collect();
        assert_eq!(relations, BTreeSet::from(["MENTIONS_SYMBOL", "RELATES_TO"]));

        // No governing doc: honest empty result, never fabricated.
        let empty = linked_design_docs(&records, &BTreeSet::new(), &BTreeSet::new());
        assert!(empty.is_empty());
    }

    #[test]
    fn diagnostics_never_carry_raw_body() {
        let repo = TempDir::new().expect("temp repo");
        let body = "# Secret plan\n\nThe password is hunter2-hunter2.\n\nSee `src/nope.rs`.\n";
        write_doc(&repo, "docs/plans/0001-secret.md", body);

        let result = import_docs(&default_opts(&repo)).expect("import succeeds");

        let serialized = serde_json::to_string(&result.diagnostics).expect("serialize");
        assert!(
            !serialized.contains("hunter2"),
            "diagnostics must never echo body text: {serialized}"
        );
        // The bounded title may appear; the body must not.
        assert!(
            !serialized.contains("The password is"),
            "diagnostics must never echo body text: {serialized}"
        );
    }
}
