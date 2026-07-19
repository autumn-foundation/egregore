//! SCIP code-intelligence export (issue #233).
//!
//! STUB (RED phase). Real implementation lands next.

use crate::ir::{GraphRecord, NodeKind, SourceSpan};
use anyhow::Result;
// The extern crate is `::scip` — this module is also named `scip`, so the
// leading `::` disambiguates from `crate::scip`.
use ::scip::symbol::format_symbol;
use ::scip::types::{
    descriptor, symbol_information, Descriptor, Document, Index, Metadata, Occurrence, Package,
    Signature, Symbol, SymbolInformation, SymbolRole, TextEncoding, ToolInfo,
};
use protobuf::MessageField;
use std::collections::BTreeMap;

/// SCIP moniker scheme identifying Egregore as the indexer.
const SCHEME: &str = "scip-egregore";
/// Package manager component of the moniker (Rust → cargo).
const MANAGER: &str = "cargo";
/// Fixed placeholder version component. The graph does not retain the scanned
/// crate's own `[package] version`, so a stable sentinel keeps monikers
/// byte-identical across runs (issue #233 AC#5); a real version join is a
/// future refinement.
const VERSION_PLACEHOLDER: &str = "0.0.0";
/// Fallback package name when no `Repository` node names the crate.
const DEFAULT_PACKAGE: &str = "egregore";

/// Per-reason tally of definition-candidate nodes dropped from the export.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SkipTally {
    /// `Symbol` / `Module` nodes with no `span`.
    pub no_span: usize,
    /// Anonymous `impl`-block symbol nodes.
    pub impl_block: usize,
    /// `Diagnostic` stub/marker nodes.
    pub diagnostic: usize,
}

/// A built SCIP index plus the AC#7 export/skip accounting.
#[derive(Debug)]
pub struct ScipExport {
    /// The assembled SCIP index.
    pub index: Index,
    /// Number of emitted `Document`s.
    pub document_count: usize,
    /// Number of emitted definition occurrences (== `SymbolInformation` count).
    pub definition_count: usize,
    /// Per-reason skip tally.
    pub skipped: SkipTally,
}

/// One resolved definition, paired with its sort key, before grouping.
struct Definition {
    path: String,
    start_line: usize,
    name: String,
    record_id: String,
    info: SymbolInformation,
    occurrence: Occurrence,
}

/// Builds a definitions-only SCIP `Index` from graph records (issue #233).
///
/// Deterministic and filesystem-local: it reads only the in-memory records
/// (spans, names, visibility/signature/doc) and never re-reads source. Every
/// span-bearing `Symbol` and `Module` node becomes one `SymbolInformation`
/// plus one `SymbolRole::Definition` `Occurrence`; positionless edges,
/// `Diagnostic` stubs, span-less nodes, and anonymous `impl` blocks are dropped
/// and tallied (AC#7). Output ordering is fixed (documents by path; per-document
/// entries by `(start_line, name, record_id)`), so encoding is byte-identical
/// across runs and independent of insertion order.
#[must_use]
pub fn build_index(records: &[GraphRecord], project_root: &str, tool_version: &str) -> ScipExport {
    let package = package_name(records);
    let mut skipped = SkipTally::default();

    // Every File node contributes a Document, even when it defines no symbols.
    let mut doc_paths: BTreeMap<String, ()> = BTreeMap::new();
    let mut definitions: Vec<Definition> = Vec::new();

    for record in records {
        if let GraphRecord::Node {
            kind,
            repo_relative_path,
            span,
            name,
            symbol_kind,
            disambiguator,
            visibility,
            signature,
            doc,
            ..
        } = record
        {
            match kind {
                NodeKind::File => {
                    if let Some(path) = repo_relative_path {
                        doc_paths.insert(path.clone(), ());
                    }
                }
                NodeKind::Diagnostic => skipped.diagnostic += 1,
                NodeKind::Symbol | NodeKind::Module => {
                    let kind_str = definition_kind_str(*kind, symbol_kind.as_deref());
                    if kind_str == "impl" {
                        skipped.impl_block += 1;
                        continue;
                    }
                    let (Some(path), Some(span), Some(name)) =
                        (repo_relative_path, span, name)
                    else {
                        skipped.no_span += 1;
                        continue;
                    };
                    let disamb = disambiguator.unwrap_or(0);
                    let moniker = build_moniker(&package, name, kind_str, disamb);
                    let info = build_symbol_information(
                        &moniker,
                        name,
                        kind_str,
                        visibility.as_deref(),
                        signature.as_deref(),
                        doc.as_deref(),
                    );
                    let occurrence = build_occurrence(&moniker, span);
                    doc_paths.insert(path.clone(), ());
                    definitions.push(Definition {
                        path: path.clone(),
                        start_line: span.start_line,
                        name: name.clone(),
                        record_id: record.id().to_string(),
                        info,
                        occurrence,
                    });
                }
                _ => {}
            }
        }
        // Edges and tombstones are positionless / non-definitional — dropped.
    }

    // Group definitions by document path.
    let mut by_path: BTreeMap<String, Vec<Definition>> = BTreeMap::new();
    for path in doc_paths.keys() {
        by_path.entry(path.clone()).or_default();
    }
    let definition_count = definitions.len();
    for def in definitions {
        by_path.entry(def.path.clone()).or_default().push(def);
    }

    let mut documents = Vec::with_capacity(by_path.len());
    for (path, mut defs) in by_path {
        // Deterministic per-document ordering.
        defs.sort_by(|a, b| {
            (a.start_line, &a.name, &a.record_id).cmp(&(b.start_line, &b.name, &b.record_id))
        });
        let mut symbols = Vec::with_capacity(defs.len());
        let mut occurrences = Vec::with_capacity(defs.len());
        for def in defs {
            symbols.push(def.info);
            occurrences.push(def.occurrence);
        }
        documents.push(Document {
            language: language_for_path(&path).to_string(),
            relative_path: path,
            occurrences,
            symbols,
            ..Default::default()
        });
    }
    let document_count = documents.len();

    let metadata = Metadata {
        tool_info: MessageField::some(ToolInfo {
            name: DEFAULT_PACKAGE.to_string(),
            version: tool_version.to_string(),
            ..Default::default()
        }),
        project_root: to_file_uri(project_root),
        text_document_encoding: TextEncoding::UTF8.into(),
        ..Default::default()
    };

    let index = Index {
        metadata: MessageField::some(metadata),
        documents,
        ..Default::default()
    };

    ScipExport {
        index,
        document_count,
        definition_count,
        skipped,
    }
}

/// Derives the SCIP package name from the `Repository` node's display name,
/// falling back to a fixed default when no repository is present.
#[must_use]
pub fn package_name(records: &[GraphRecord]) -> String {
    for record in records {
        if let GraphRecord::Node {
            kind: NodeKind::Repository,
            name: Some(name),
            ..
        } = record
        {
            if !name.is_empty() {
                return name.clone();
            }
        }
    }
    DEFAULT_PACKAGE.to_string()
}

/// Normalizes a `Symbol`/`Module` node into a stable kind token used for both
/// the SCIP descriptor suffix and `SymbolInformation.kind`.
fn definition_kind_str(kind: NodeKind, symbol_kind: Option<&str>) -> &'static str {
    if kind == NodeKind::Module {
        return "module";
    }
    match symbol_kind {
        Some("struct") => "struct",
        Some("enum") => "enum",
        Some("trait") => "trait",
        Some("type_alias") => "type_alias",
        Some("function") => "function",
        Some("method") => "method",
        Some("const") => "const",
        Some("static") => "static",
        Some("mod" | "module") => "module",
        Some("impl") => "impl",
        _ => "other",
    }
}

/// Builds a grammar-valid global SCIP moniker from ADR-0004 identity.
///
/// The `qualified_name` is split on `::`: every non-final segment is a
/// `Namespace` descriptor; the final segment takes the suffix its kind dictates.
/// The ADR-0004 source-order `disambiguator` feeds the SCIP method-disambiguator
/// slot for callables (and a trailing `Meta` descriptor for other kinds) so the
/// moniker is unique exactly where the Egregore identity is.
fn build_moniker(package: &str, qualified_name: &str, kind_str: &str, disambiguator: u64) -> String {
    let segments: Vec<&str> = qualified_name.split("::").filter(|s| !s.is_empty()).collect();
    let mut descriptors: Vec<Descriptor> = Vec::new();
    let last = segments.len().saturating_sub(1);
    for (i, seg) in segments.iter().enumerate() {
        if i != last {
            descriptors.push(descriptor(seg, descriptor::Suffix::Namespace, ""));
            continue;
        }
        let (suffix, is_method) = suffix_for_kind(kind_str);
        if is_method {
            let disamb = if disambiguator > 0 {
                disambiguator.to_string()
            } else {
                String::new()
            };
            descriptors.push(descriptor(seg, suffix, &disamb));
        } else {
            descriptors.push(descriptor(seg, suffix, ""));
            if disambiguator > 0 {
                // Preserve uniqueness for the rare same-name/kind/file collision.
                descriptors.push(descriptor(
                    &disambiguator.to_string(),
                    descriptor::Suffix::Meta,
                    "",
                ));
            }
        }
    }
    let symbol = Symbol {
        scheme: SCHEME.to_string(),
        package: MessageField::some(Package {
            manager: MANAGER.to_string(),
            name: package.to_string(),
            version: VERSION_PLACEHOLDER.to_string(),
            ..Default::default()
        }),
        descriptors,
        ..Default::default()
    };
    format_symbol(symbol)
}

/// Constructs one SCIP descriptor.
fn descriptor(name: &str, suffix: descriptor::Suffix, disambiguator: &str) -> Descriptor {
    Descriptor {
        name: name.to_string(),
        disambiguator: disambiguator.to_string(),
        suffix: suffix.into(),
        ..Default::default()
    }
}

/// Maps a kind token to its SCIP descriptor suffix and whether it is a callable
/// (which owns the method-disambiguator slot).
fn suffix_for_kind(kind_str: &str) -> (descriptor::Suffix, bool) {
    match kind_str {
        "module" => (descriptor::Suffix::Namespace, false),
        "struct" | "enum" | "trait" | "type_alias" => (descriptor::Suffix::Type, false),
        "function" | "method" => (descriptor::Suffix::Method, true),
        // const/static and any other term-like kind.
        _ => (descriptor::Suffix::Term, false),
    }
}

/// Maps a kind token to the SCIP `SymbolInformation.kind` enum.
fn scip_symbol_kind(kind_str: &str) -> symbol_information::Kind {
    use symbol_information::Kind;
    match kind_str {
        "module" => Kind::Module,
        "struct" => Kind::Struct,
        "enum" => Kind::Enum,
        "trait" => Kind::Trait,
        "type_alias" => Kind::TypeAlias,
        "function" => Kind::Function,
        "method" => Kind::Method,
        "const" => Kind::Constant,
        "static" => Kind::Variable,
        _ => Kind::UnspecifiedKind,
    }
}

/// Builds a `SymbolInformation`, carrying visibility/doc into `documentation`
/// and the declaration header into `signature_documentation` (AC#4).
fn build_symbol_information(
    moniker: &str,
    name: &str,
    kind_str: &str,
    visibility: Option<&str>,
    signature: Option<&str>,
    doc: Option<&str>,
) -> SymbolInformation {
    let mut documentation: Vec<String> = Vec::new();
    if let Some(vis) = visibility {
        documentation.push(format!("visibility: {vis}"));
    }
    if let Some(doc) = doc {
        documentation.push(doc.to_string());
    }
    let signature_documentation = match signature {
        Some(sig) => MessageField::some(Signature {
            language: "rust".to_string(),
            text: sig.to_string(),
            ..Default::default()
        }),
        None => MessageField::none(),
    };
    SymbolInformation {
        symbol: moniker.to_string(),
        display_name: name.to_string(),
        documentation,
        signature_documentation,
        kind: scip_symbol_kind(kind_str).into(),
        ..Default::default()
    }
}

/// Builds a whole-line definition `Occurrence` from a 1-based `SourceSpan`.
///
/// SCIP ranges are 0-based; the range is `[start_line-1, 0, end_line, 0]`
/// (column 0, no source re-read — column precision is a follow-up).
fn build_occurrence(moniker: &str, span: &SourceSpan) -> Occurrence {
    let start_line = i32::try_from(span.start_line.saturating_sub(1)).unwrap_or(i32::MAX);
    let end_line = i32::try_from(span.end_line).unwrap_or(i32::MAX);
    Occurrence {
        range: vec![start_line, 0, end_line, 0],
        symbol: moniker.to_string(),
        symbol_roles: SymbolRole::Definition as i32,
        ..Default::default()
    }
}

/// Maps a repo-relative path's extension to the SCIP `Document.language` string.
fn language_for_path(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("rs") => "Rust",
        Some("py") => "Python",
        Some("ts" | "tsx") => "TypeScript",
        Some("go") => "Go",
        _ => "",
    }
}

/// Wraps a project-root string as a `file://` URI (idempotent).
fn to_file_uri(root: &str) -> String {
    if root.starts_with("file://") {
        root.to_string()
    } else if let Some(rest) = root.strip_prefix('/') {
        format!("file:///{rest}")
    } else {
        format!("file:///{root}")
    }
}

/// Serializes a SCIP `Index` to protobuf bytes.
///
/// # Errors
/// Returns an error if protobuf encoding fails.
pub fn encode_index(index: &Index) -> Result<Vec<u8>> {
    use protobuf::Message;
    index
        .write_to_bytes()
        .map_err(|error| anyhow::anyhow!("failed to encode SCIP index: {error}"))
}

/// Parses protobuf bytes back into a SCIP `Index` (in-process round-trip
/// validation; no external `scip` CLI required).
///
/// # Errors
/// Returns an error if the bytes are not a valid SCIP `Index` message.
pub fn decode_index(bytes: &[u8]) -> Result<Index> {
    use protobuf::Message;
    Index::parse_from_bytes(bytes)
        .map_err(|error| anyhow::anyhow!("failed to decode SCIP index: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{EdgeLabel, NodeKind, SourceSpan};
    use protobuf::Message;

    fn span(start_line: usize, end_line: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 1,
            start_line,
            end_line,
        }
    }

    fn sym(id: &str, kind: &str, name: &str, span: SourceSpan) -> GraphRecord {
        GraphRecord::syntax_symbol(
            id.to_string(),
            kind,
            "src/lib.rs".to_string(),
            span,
            name.to_string(),
            "rust",
            0,
            format!("Rust {kind} {name}"),
        )
    }

    /// A fixture graph with real span-bearing definitions plus every drop class.
    fn fixture() -> Vec<GraphRecord> {
        vec![
            GraphRecord::node(
                "repo-1".to_string(),
                NodeKind::Repository,
                None,
                None,
                Some("widget".to_string()),
                "Repository widget".to_string(),
            ),
            GraphRecord::node(
                "file-1".to_string(),
                NodeKind::File,
                Some("src/lib.rs".to_string()),
                None,
                Some("src/lib.rs".to_string()),
                "File src/lib.rs".to_string(),
            ),
            sym("s-widget", "struct", "Widget", span(3, 10)),
            sym("s-render", "method", "Widget::render", span(5, 8))
                .with_declaration_surface(
                    Some("public".to_string()),
                    Some("fn render(&self)".to_string()),
                    Some("Renders the widget.".to_string()),
                ),
            sym("s-helper", "function", "helper", span(12, 14)),
            // A span-bearing module lands as a namespace definition.
            GraphRecord::node(
                "m-inner".to_string(),
                NodeKind::Module,
                Some("src/lib.rs".to_string()),
                Some(span(20, 25)),
                Some("inner".to_string()),
                "Rust module inner".to_string(),
            ),
            // ── Drop classes ──────────────────────────────────────────────
            // Anonymous impl block.
            sym("s-impl", "impl", "Widget", span(3, 10)),
            // Diagnostic stub.
            GraphRecord::node(
                "d-1".to_string(),
                NodeKind::Diagnostic,
                Some("src/lib.rs".to_string()),
                Some(span(30, 31)),
                Some("macro_bang".to_string()),
                "Diagnostic".to_string(),
            ),
            // Span-less symbol.
            GraphRecord::node(
                "s-nospan".to_string(),
                NodeKind::Symbol,
                Some("src/lib.rs".to_string()),
                None,
                Some("Ghost".to_string()),
                "Rust struct Ghost".to_string(),
            ),
            // A positionless edge — never an occurrence.
            GraphRecord::edge(
                EdgeLabel::Defines,
                "file-1".to_string(),
                "s-widget".to_string(),
                Some("1.0".to_string()),
                "File defines Widget".to_string(),
            ),
        ]
    }

    fn parse_back(bytes: &[u8]) -> ::scip::types::Index {
        ::scip::types::Index::parse_from_bytes(bytes).expect("emitted SCIP index must parse back")
    }

    #[test]
    fn builds_and_roundtrips_definitions_only() {
        let export = build_index(&fixture(), "widget", "9.9.9");
        assert_eq!(export.document_count, 1);
        assert_eq!(export.definition_count, 4);
        assert_eq!(export.skipped.impl_block, 1);
        assert_eq!(export.skipped.diagnostic, 1);
        assert_eq!(export.skipped.no_span, 1);

        let bytes = encode_index(&export.index).expect("encode");
        let parsed = parse_back(&bytes);

        assert_eq!(parsed.documents.len(), 1);
        let doc = &parsed.documents[0];
        assert_eq!(doc.relative_path, "src/lib.rs");
        assert_eq!(doc.language, "Rust");
        assert_eq!(doc.symbols.len(), 4);
        assert_eq!(doc.occurrences.len(), 4);
        // Every occurrence is a definition; no reference occurrences.
        for occ in &doc.occurrences {
            assert_eq!(occ.symbol_roles, ::scip::types::SymbolRole::Definition as i32);
        }
        // Metadata: tool info + UTF8 encoding.
        let meta = parsed.metadata.as_ref().expect("metadata");
        let tool = meta.tool_info.as_ref().expect("tool info");
        assert_eq!(tool.name, "egregore");
        assert_eq!(tool.version, "9.9.9");
        assert_eq!(
            meta.text_document_encoding.enum_value_or_default(),
            ::scip::types::TextEncoding::UTF8
        );
        assert!(meta.project_root.starts_with("file://"));
    }

    #[test]
    fn definition_ranges_are_line_span_zero_based() {
        let export = build_index(&fixture(), "widget", "0.0.0");
        let doc = &export.index.documents[0];
        // Widget: lines 3..=10 → [2, 0, 10, 0].
        let widget = doc
            .occurrences
            .iter()
            .find(|o| o.symbol.contains("Widget#"))
            .expect("Widget occurrence");
        assert_eq!(widget.range, vec![2, 0, 10, 0]);
    }

    #[test]
    fn definition_set_matches_file_defined_symbols() {
        let export = build_index(&fixture(), "widget", "0.0.0");
        let doc = &export.index.documents[0];
        let mut names: Vec<&str> = doc.symbols.iter().map(|s| s.display_name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Widget", "Widget::render", "helper", "inner"]);
    }

    #[test]
    fn documentation_and_signature_populated() {
        let export = build_index(&fixture(), "widget", "0.0.0");
        let doc = &export.index.documents[0];
        let render = doc
            .symbols
            .iter()
            .find(|s| s.display_name == "Widget::render")
            .expect("render symbol");
        let sig = render.signature_documentation.as_ref().expect("signature");
        assert_eq!(sig.text, "fn render(&self)");
        assert!(render.documentation.iter().any(|d| d.contains("public")));
        assert!(render
            .documentation
            .iter()
            .any(|d| d.contains("Renders the widget.")));
    }

    #[test]
    fn moniker_scheme_is_global_and_grammar_valid() {
        let export = build_index(&fixture(), "widget", "0.0.0");
        let doc = &export.index.documents[0];
        let widget = doc
            .symbols
            .iter()
            .find(|s| s.display_name == "Widget")
            .expect("Widget symbol");
        assert_eq!(widget.symbol, "scip-egregore cargo widget 0.0.0 Widget#");
        // Every emitted moniker parses under the SCIP symbol grammar.
        for s in &doc.symbols {
            assert!(
                ::scip::symbol::parse_symbol(&s.symbol).is_ok(),
                "moniker not grammar-valid: {}",
                s.symbol
            );
            assert!(::scip::symbol::is_global_symbol(&s.symbol));
        }
    }

    #[test]
    fn deterministic_across_runs_and_insertion_order() {
        let records = fixture();
        let a = encode_index(&build_index(&records, "widget", "0.0.0").index).expect("a");
        let b = encode_index(&build_index(&records, "widget", "0.0.0").index).expect("b");
        assert_eq!(a, b, "repeated builds must be byte-identical");

        let mut shuffled = records.clone();
        shuffled.reverse();
        let c = encode_index(&build_index(&shuffled, "widget", "0.0.0").index).expect("c");
        assert_eq!(a, c, "insertion order must not affect bytes");
    }

    #[test]
    fn fabrication_guard_emits_nothing_for_non_definitions() {
        // Only a File node plus drop-class records — no real definitions.
        let records = vec![
            GraphRecord::node(
                "file-1".to_string(),
                NodeKind::File,
                Some("src/lib.rs".to_string()),
                None,
                Some("src/lib.rs".to_string()),
                "File src/lib.rs".to_string(),
            ),
            GraphRecord::node(
                "d-1".to_string(),
                NodeKind::Diagnostic,
                Some("src/lib.rs".to_string()),
                Some(span(1, 2)),
                Some("macro".to_string()),
                "Diagnostic".to_string(),
            ),
            GraphRecord::node(
                "s-nospan".to_string(),
                NodeKind::Symbol,
                Some("src/lib.rs".to_string()),
                None,
                Some("Ghost".to_string()),
                "Rust struct Ghost".to_string(),
            ),
            sym("s-impl", "impl", "Widget", span(3, 10)),
        ];
        let export = build_index(&records, "widget", "0.0.0");
        assert_eq!(export.definition_count, 0);
        let total_occ: usize = export.index.documents.iter().map(|d| d.occurrences.len()).sum();
        let total_sym: usize = export.index.documents.iter().map(|d| d.symbols.len()).sum();
        assert_eq!(total_occ, 0);
        assert_eq!(total_sym, 0);
        assert_eq!(export.skipped.diagnostic, 1);
        assert_eq!(export.skipped.no_span, 1);
        assert_eq!(export.skipped.impl_block, 1);
    }

    #[test]
    fn package_name_from_repository_node() {
        assert_eq!(package_name(&fixture()), "widget");
        assert_eq!(package_name(&[]), "egregore");
    }
}
