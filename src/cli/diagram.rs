//! `eg query diagram` (issue #228).
//!
//! Renders one symbol's local call neighborhood — direct callers (`CALLS`
//! in-edges), direct callees (`CALLS` out-edges), and the containing
//! file/module (`DEFINES`/`CONTAINS`) — as a diagram in a standard text
//! format (Mermaid or Graphviz DOT). The diagram is a deterministic
//! projection of already-stored, citable edges: every node carries its stable
//! record ID plus repo-relative file/span, every edge is labeled with its
//! stored kind, and no edge appears that is not in the graph.
//!
//! Trust separation: the lane reads code-facts only (trust
//! `source_derived`), emits metadata (identifiers, paths, spans, edge kinds,
//! counts, redaction markers) — never record bodies or raw source text.

use super::*;
use std::fmt::Write as _;

/// Diagram output format for `eg query diagram`.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, clap::ValueEnum)]
pub(crate) enum DiagramFormat {
    /// Mermaid flowchart (default): paste into a PR, ADR, or `mmdc`.
    #[default]
    Mermaid,
    /// Graphviz DOT: pipe into `dot`.
    Dot,
    /// Machine-readable envelope (nodes/edges/truncation/diagnostics) with
    /// both rendered diagram texts embedded.
    Json,
    /// Human-readable node/edge listing with the same content and ordering.
    Text,
}

pub(crate) const DIAGRAM_DISCLAIMER: &str = "The diagram is a projection of stored graph edges, not proof of runtime \
     behavior. Every node cites its stable record ID; verify citations against \
     the store before acting.";

const ORDERING_DOC: &str =
    "nodes by ascending record ID; edges by (source, target, label, edge ID)";

/// Escape a node label for a Mermaid `["..."]` quoted label.
fn escape_mermaid_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        match ch {
            '"' => out.push_str("#quot;"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// Escape a node label for a DOT `"..."` quoted label.
fn escape_dot_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for ch in label.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push(' '),
            _ => out.push(ch),
        }
    }
    out
}

/// Display label for a node: its symbol/file name, else its record ID.
fn node_label(record: &GraphRecord) -> &str {
    match record {
        GraphRecord::Node {
            name: Some(name), ..
        } if !name.is_empty() => name.as_str(),
        _ => record.id(),
    }
}

/// Citable location string: `path:start-end`, with markers when absent.
fn node_location(record: &GraphRecord) -> String {
    match record {
        GraphRecord::Node {
            repo_relative_path: Some(path),
            span: Some(span),
            ..
        } => format!("{}:{}-{}", path, span.start_line, span.end_line),
        GraphRecord::Node {
            repo_relative_path: Some(path),
            ..
        } => path.clone(),
        _ => "(unknown location)".to_owned(),
    }
}

const fn node_kind_str(record: &GraphRecord) -> &'static str {
    match record {
        GraphRecord::Node { kind, .. } => kind.as_str(),
        _ => "edge",
    }
}

/// Legend line: `nK -> record ID path:span [kind name]`, with the name
/// escaped for the target renderer (legends live in renderer comments).
fn legend_line(key: &str, record: &GraphRecord, escape_name: fn(&str) -> String) -> String {
    format!(
        "{key} -> {} {} [{} {}]",
        record.id(),
        node_location(record),
        node_kind_str(record),
        escape_name(node_label(record)),
    )
}

/// Renders the neighborhood as a Mermaid flowchart. Deterministic: node keys
/// (`n0`, `n1`, …) are assigned in ascending record-ID order and edges are
/// emitted in the canonical edge order.
pub(crate) fn render_diagram_mermaid(hood: &query::Neighborhood<'_>) -> String {
    let anchor_id = hood.anchor.id();
    let anchor_name = node_label(hood.anchor);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "%% neighborhood of `{anchor_name}` ({anchor_id}) — `eg query diagram`"
    );
    let _ = writeln!(
        out,
        "%% depth={} max_nodes={}",
        hood.max_depth, hood.max_nodes
    );
    let _ = writeln!(out, "%% ordering: {ORDERING_DOC}");
    let _ = writeln!(
        out,
        "%% legend: node key -> stable record ID, repo-relative file/span"
    );
    for (index, node) in hood.nodes.iter().enumerate() {
        let _ = writeln!(
            out,
            "%%   {}",
            legend_line(&format!("n{index}"), node.record, escape_mermaid_label)
        );
    }
    if let Some(t) = &hood.depth_truncation {
        let _ = writeln!(
            out,
            "%% truncation: depth bound {} dropped {} node(s) beyond the bound",
            t.max_depth, t.dropped_total
        );
    }
    if let Some(t) = &hood.budget_truncation {
        let _ = writeln!(
            out,
            "%% truncation: node budget {} dropped {} node(s), {} edge(s)",
            t.max_nodes, t.dropped_nodes, t.dropped_edges
        );
    }
    out.push_str("flowchart LR\n");
    let anchor_key = hood
        .nodes
        .iter()
        .position(|n| n.record.id() == anchor_id)
        .map(|i| format!("n{i}"));
    for (index, node) in hood.nodes.iter().enumerate() {
        let _ = writeln!(
            out,
            "    n{index}[\"{}\"]",
            escape_mermaid_label(node_label(node.record))
        );
    }
    let _ = writeln!(out, "    classDef anchor stroke-width:3px");
    for edge in &hood.edges {
        let source_key = hood
            .nodes
            .iter()
            .position(|n| n.record.id() == edge.source_id)
            .map_or_else(|| "?".to_owned(), |i| format!("n{i}"));
        let target_key = hood
            .nodes
            .iter()
            .position(|n| n.record.id() == edge.target_id)
            .map_or_else(|| "?".to_owned(), |i| format!("n{i}"));
        let _ = writeln!(out, "    {source_key} -- {} --> {target_key}", edge.label);
    }
    if let Some(key) = anchor_key {
        let _ = writeln!(out, "    class {key} anchor");
    }
    out
}

/// Renders the neighborhood as Graphviz DOT. Same deterministic ordering as
/// the Mermaid renderer.
pub(crate) fn render_diagram_dot(hood: &query::Neighborhood<'_>) -> String {
    let anchor_id = hood.anchor.id();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "// neighborhood of {} ({anchor_id}) — `eg query diagram`",
        node_label(hood.anchor)
    );
    let _ = writeln!(
        out,
        "// depth={} max_nodes={}",
        hood.max_depth, hood.max_nodes
    );
    let _ = writeln!(out, "// ordering: {ORDERING_DOC}");
    let _ = writeln!(
        out,
        "// legend: node key -> stable record ID, repo-relative file/span"
    );
    for (index, node) in hood.nodes.iter().enumerate() {
        let _ = writeln!(
            out,
            "//   {}",
            legend_line(&format!("n{index}"), node.record, escape_dot_label)
        );
    }
    if let Some(t) = &hood.depth_truncation {
        let _ = writeln!(
            out,
            "// truncation: depth bound {} dropped {} node(s) beyond the bound",
            t.max_depth, t.dropped_total
        );
    }
    if let Some(t) = &hood.budget_truncation {
        let _ = writeln!(
            out,
            "// truncation: node budget {} dropped {} node(s), {} edge(s)",
            t.max_nodes, t.dropped_nodes, t.dropped_edges
        );
    }
    out.push_str("digraph \"neighborhood\" {\n");
    out.push_str("    rankdir=LR;\n");
    for (index, node) in hood.nodes.iter().enumerate() {
        let label = escape_dot_label(node_label(node.record));
        if node.record.id() == anchor_id {
            let _ = writeln!(
                out,
                "    n{index} [label=\"{label}\", shape=box, style=bold]; // anchor"
            );
        } else {
            let _ = writeln!(out, "    n{index} [label=\"{label}\"];");
        }
    }
    for edge in &hood.edges {
        let source_key = hood
            .nodes
            .iter()
            .position(|n| n.record.id() == edge.source_id)
            .map_or_else(|| "?".to_owned(), |i| format!("n{i}"));
        let target_key = hood
            .nodes
            .iter()
            .position(|n| n.record.id() == edge.target_id)
            .map_or_else(|| "?".to_owned(), |i| format!("n{i}"));
        let _ = writeln!(
            out,
            "    {source_key} -> {target_key} [label=\"{}\"];",
            edge.label
        );
    }
    out.push_str("}\n");
    out
}

/// Node keys (`n0`, …) in ascending record-ID order, matching the renderers.
fn node_keys(hood: &query::Neighborhood<'_>) -> Vec<String> {
    (0..hood.nodes.len()).map(|i| format!("n{i}")).collect()
}

fn node_json(key: &str, node: &query::NeighborhoodNode<'_>) -> serde_json::Value {
    let (kind, name, path, span) = match node.record {
        GraphRecord::Node {
            kind,
            name,
            repo_relative_path,
            span,
            ..
        } => (
            kind.as_str(),
            name.as_deref(),
            repo_relative_path.as_deref(),
            span,
        ),
        _ => ("edge", None, None, &None),
    };
    serde_json::json!({
        "key": key,
        "record_id": node.record.id(),
        "name": name,
        "kind": kind,
        "repo_relative_path": path,
        "span": span,
        "hop": node.hop,
    })
}

fn truncation_json(hood: &query::Neighborhood<'_>) -> serde_json::Value {
    serde_json::json!({
        "max_depth_truncated": hood.depth_truncation.as_ref().map(|t| serde_json::json!({
            "code": "max_depth_truncated",
            "max_depth": t.max_depth,
            "dropped_frontier": t.dropped_frontier.iter().map(|d| serde_json::json!({
                "depth": d.depth,
                "count": d.count,
            })).collect::<Vec<_>>(),
            "dropped_total": t.dropped_total,
        })),
        "node_budget_truncated": hood.budget_truncation.as_ref().map(|t| serde_json::json!({
            "code": "node_budget_truncated",
            "max_nodes": t.max_nodes,
            "dropped_nodes": t.dropped_nodes,
            "dropped_edges": t.dropped_edges,
        })),
    })
}

/// Human-readable listing with the same nodes, edges, ordering, and
/// truncation notices as the diagram renderers.
fn render_diagram_text(
    hood: &query::Neighborhood<'_>,
    handle: &str,
    diagnostics: &[AuditDiagnostic<'_>],
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "diagram for {} ({}) — depth={} max_nodes={}",
        node_label(hood.anchor),
        hood.anchor.id(),
        hood.max_depth,
        hood.max_nodes
    );
    let _ = writeln!(out, "ordering: {ORDERING_DOC}");
    let _ = writeln!(out, "nodes ({}):", hood.nodes.len());
    for (index, node) in hood.nodes.iter().enumerate() {
        let _ = writeln!(
            out,
            "  n{index} {} {} [{}] hop={}",
            node.record.id(),
            node_location(node.record),
            node_kind_str(node.record),
            node.hop
        );
    }
    let _ = writeln!(out, "edges ({}):", hood.edges.len());
    for edge in &hood.edges {
        let source_key = hood
            .nodes
            .iter()
            .position(|n| n.record.id() == edge.source_id)
            .map_or_else(|| "?".to_owned(), |i| format!("n{i}"));
        let target_key = hood
            .nodes
            .iter()
            .position(|n| n.record.id() == edge.target_id)
            .map_or_else(|| "?".to_owned(), |i| format!("n{i}"));
        let _ = writeln!(
            out,
            "  {source_key} -> {target_key} [{}] edge={}",
            edge.label, edge.edge_record_id
        );
    }
    if let Some(t) = &hood.depth_truncation {
        let _ = writeln!(
            out,
            "truncation: depth bound {} dropped {} node(s) beyond the bound",
            t.max_depth, t.dropped_total
        );
    }
    if let Some(t) = &hood.budget_truncation {
        let _ = writeln!(
            out,
            "truncation: node budget {} dropped {} node(s), {} edge(s)",
            t.max_nodes, t.dropped_nodes, t.dropped_edges
        );
    }
    for d in diagnostics {
        let _ = writeln!(out, "diagnostic {}: {}", d.code, d.source_record_id);
    }
    let _ = writeln!(out, "handle: {handle}");
    let _ = writeln!(out, "{DIAGRAM_DISCLAIMER}");
    out
}

/// Narrow `records` to one commit's snapshot view for `--at`/`--as-of`
/// (issues #8/#66). Returns the owned snapshot (when a temporal selector was
/// given) plus the resolved commit SHA.
fn narrow_to_commit_view(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    at: Option<&str>,
    as_of: Option<&str>,
) -> (Option<Vec<GraphRecord>>, Option<String>) {
    if at.is_none() && as_of.is_none() {
        return (None, None);
    }
    let sha = resolve_transitive_commit_view(records, index, repo_scope, at, as_of);
    let view: Vec<GraphRecord> = records
        .iter()
        .filter(|r| match r {
            GraphRecord::Node {
                temporal: Some(t), ..
            }
            | GraphRecord::Edge {
                temporal: Some(t), ..
            } => t.git_commit == sha,
            _ => false,
        })
        .cloned()
        .collect();
    (Some(view), Some(sha))
}

/// Resolve the CLI handle to exactly one live symbol's record ID, exiting
/// with the lane's documented diagnostics otherwise (never a partial
/// render): 1 for malformed/ambiguous/unsupported input, 2 for
/// unknown/stale/absent handles.
fn resolve_diagram_anchor_id(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
) -> String {
    let target = match query::resolve_failure_handle(records, handle, index, repo_scope) {
        Ok(t) => t,
        Err(
            err @ (query::FailureHandleError::Ambiguous { .. }
            | query::FailureHandleError::Unsupported { .. }),
        ) => {
            eprintln!("{}", serde_json::to_string(&err).unwrap_or_default());
            std::process::exit(1);
        }
    };

    if matches!(
        target.kind,
        query::FailureTargetKind::Task | query::FailureTargetKind::Source
    ) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {} target; diagram accepts only symbol handles",
                target.kind.as_str()
            ),
        };
        eprintln!("{}", serde_json::to_string(&err).unwrap_or_default());
        std::process::exit(1);
    }
    if matches!(target.kind, query::FailureTargetKind::File) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: "handle resolved to a file; diagram accepts only symbol handles".to_owned(),
        };
        eprintln!("{}", serde_json::to_string(&err).unwrap_or_default());
        std::process::exit(1);
    }
    if let Some(kind) = query::transitive_callers_non_symbol_anchor_kind(records, &target) {
        let err = query::FailureHandleError::Unsupported {
            handle: handle.to_owned(),
            message: format!(
                "handle resolved to a {kind:?} node; diagram accepts only symbol handles"
            ),
        };
        eprintln!("{}", serde_json::to_string(&err).unwrap_or_default());
        std::process::exit(1);
    }

    if target.is_empty() {
        let code = if target.stale {
            "stale_handle"
        } else {
            "no_match"
        };
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": code, "handle": handle },
        });
        println!("{envelope}");
        std::process::exit(2);
    }

    // One name resolving to several live symbols is ambiguous for a diagram:
    // rendering the union would bleed an unrelated same-name symbol's edges
    // into the picture (issues #67/#192).
    if target.anchor_ids.len() > 1 {
        let err = query::FailureHandleError::Ambiguous {
            handle: handle.to_owned(),
            candidates: target.anchor_ids.iter().cloned().collect(),
        };
        eprintln!("{}", serde_json::to_string(&err).unwrap_or_default());
        std::process::exit(1);
    }
    target
        .anchor_ids
        .iter()
        .next()
        .expect("non-empty target has an anchor")
        .clone()
}

/// Traversal diagnostics plus the redaction gate over the anchor and every
/// reached node, in canonical order.
fn diagram_diagnostics<'a>(hood: &'a query::Neighborhood<'a>) -> Vec<AuditDiagnostic<'a>> {
    let mut diagnostics: Vec<AuditDiagnostic<'_>> = hood
        .diagnostics
        .iter()
        .map(|d| AuditDiagnostic {
            code: &d.code,
            source_record_id: &d.source_record_id,
            target_handle: &d.target_handle,
            relation: &d.relation,
            target_domain: &d.target_domain,
        })
        .collect();
    protected_payload_diagnostics(hood.anchor, &mut diagnostics);
    for node in &hood.nodes {
        protected_payload_diagnostics(node.record, &mut diagnostics);
    }
    diagnostics.sort_by(|a, b| {
        a.code
            .cmp(b.code)
            .then_with(|| a.source_record_id.cmp(b.source_record_id))
            .then_with(|| a.target_handle.cmp(b.target_handle))
            .then_with(|| a.relation.cmp(b.relation))
    });
    diagnostics.dedup_by(|a, b| {
        a.code == b.code
            && a.source_record_id == b.source_record_id
            && a.target_handle == b.target_handle
            && a.relation == b.relation
    });
    diagnostics
}

/// Node key for a record id, or `"?"` for ids outside the admitted set
/// (unreachable: edges are induced over admitted nodes).
fn node_key<'k>(hood: &query::Neighborhood<'_>, keys: &'k [String], id: &str) -> &'k str {
    hood.nodes
        .iter()
        .position(|n| n.record.id() == id)
        .map_or("?", |i| keys[i].as_str())
}

/// The machine-readable JSON envelope: structured nodes/edges plus both
/// diagram strings, so agents get citations without re-parsing text.
fn render_diagram_json(
    hood: &query::Neighborhood<'_>,
    keys: &[String],
    handle: &str,
    as_of: Option<&str>,
    at_commit: Option<&String>,
    diagnostics: &[AuditDiagnostic<'_>],
) -> Result<String> {
    let (target_kind, target_name, target_path, target_span) = match hood.anchor {
        GraphRecord::Node {
            kind,
            name,
            repo_relative_path,
            span,
            ..
        } => (
            kind.as_str(),
            name.as_deref(),
            repo_relative_path.as_deref(),
            span,
        ),
        _ => ("edge", None, None, &None),
    };
    let response = serde_json::json!({
        "ok": true,
        "lane": "diagram",
        "handle": handle,
        "target": {
            "record_id": hood.anchor.id(),
            "name": target_name,
            "kind": target_kind,
            "repo_relative_path": target_path,
            "span": target_span,
        },
        "depth": hood.max_depth,
        "max_nodes": hood.max_nodes,
        "at_commit": at_commit,
        "as_of": as_of,
        "node_count": hood.nodes.len(),
        "edge_count": hood.edges.len(),
        "ordering": ORDERING_DOC,
        "nodes": hood.nodes.iter().enumerate().map(|(i, n)| node_json(&keys[i], n)).collect::<Vec<_>>(),
        "edges": hood.edges.iter().map(|e| serde_json::json!({
            "edge_record_id": e.edge_record_id,
            "label": e.label,
            "source_key": node_key(hood, keys, e.source_id),
            "source_record_id": e.source_id,
            "target_key": node_key(hood, keys, e.target_id),
            "target_record_id": e.target_id,
        })).collect::<Vec<_>>(),
        "truncation": truncation_json(hood),
        "diagnostics": diagnostics,
        "mermaid": render_diagram_mermaid(hood),
        "dot": render_diagram_dot(hood),
        "disclaimer": DIAGRAM_DISCLAIMER,
    });
    serde_json::to_string_pretty(&response).context("failed to serialize diagram envelope")
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn query_diagram_cmd(
    records: &[GraphRecord],
    handle: &str,
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    depth: usize,
    max_nodes: usize,
    at: Option<&str>,
    as_of: Option<&str>,
    format: DiagramFormat,
) -> Result<()> {
    // ── temporal narrowing: one commit's snapshot view (issues #8/#66) ────────
    let (filtered, at_commit) = narrow_to_commit_view(records, index, repo_scope, at, as_of);
    let records: &[GraphRecord] = filtered.as_deref().unwrap_or(records);

    // ── handle resolution (symbol record ID or exact symbol name only) ────────
    let anchor_id = resolve_diagram_anchor_id(records, handle, index, repo_scope);

    let Some(hood) = query::neighborhood(records, &anchor_id, depth, max_nodes) else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": { "code": "no_match", "handle": handle },
        });
        println!("{envelope}");
        std::process::exit(2);
    };

    // An anchor with zero neighbors is an explicit diagnostic, never a
    // lone-node diagram masquerading as "no dependencies" (issue #228).
    // Envelope on stdout, like the orientation lane's exit-3 diagnostic.
    if hood.nodes.len() <= 1 && hood.edges.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "empty_neighborhood",
                "handle": handle,
                "record_id": anchor_id,
                "message": "symbol resolved but has no callers, callees, or container in this view",
            },
        });
        println!("{envelope}");
        std::process::exit(3);
    }

    // ── diagnostics: traversal diagnostics + redaction gate over reached nodes
    let diagnostics = diagram_diagnostics(&hood);

    let keys = node_keys(&hood);

    match format {
        DiagramFormat::Mermaid => {
            print!("{}", render_diagram_mermaid(&hood));
        }
        DiagramFormat::Dot => {
            print!("{}", render_diagram_dot(&hood));
        }
        DiagramFormat::Json => {
            println!(
                "{}",
                render_diagram_json(
                    &hood,
                    &keys,
                    handle,
                    as_of,
                    at_commit.as_ref(),
                    &diagnostics
                )?
            );
        }
        DiagramFormat::Text => {
            print!("{}", render_diagram_text(&hood, handle, &diagnostics));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Unit tests for the diagram renderers: label escaping and deterministic
    //! structure on hand-built neighborhoods.
    use super::*;

    #[test]
    fn mermaid_escapes_double_quotes_in_labels() {
        assert_eq!(escape_mermaid_label("say_\"hi\""), "say_#quot;hi#quot;");
        assert_eq!(escape_mermaid_label("plain"), "plain");
    }

    #[test]
    fn dot_escapes_quotes_and_backslashes_in_labels() {
        assert_eq!(escape_dot_label("say_\"hi\""), "say_\\\"hi\\\"");
        assert_eq!(escape_dot_label("a\\b"), "a\\\\b");
    }

    #[test]
    fn node_location_formats_span() {
        let record = GraphRecord::node(
            "id".to_owned(),
            NodeKind::Symbol,
            Some("src/a.rs".to_owned()),
            Some(SourceSpan {
                start_byte: 0,
                end_byte: 10,
                start_line: 3,
                end_line: 9,
                start_column: None,
                end_column: None,
            }),
            Some("f".to_owned()),
            "symbol f".to_owned(),
        );
        assert_eq!(node_location(&record), "src/a.rs:3-9");
    }
}
