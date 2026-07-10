use super::*;

pub(crate) fn query_orient_cmd(
    records: &[GraphRecord],
    repo_id: Option<&str>,
    limit: usize,
    format: OutputFormat,
) -> Result<()> {
    match query::orientation_map(records, repo_id, limit) {
        Ok(map) => {
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": true,
                        "result": map,
                    });
                    println!("{}", serde_json::to_string_pretty(&envelope)?);
                }
                OutputFormat::Text => {
                    println!("Entry Points:");
                    for ep in &map.entry_points {
                        println!("- {} ({})", ep.repo_relative_path, ep.record_id);
                    }
                    println!("\nModule/File Tree:");
                    for node in &map.module_tree {
                        print_tree_node_text(node, 0);
                    }
                    println!("\nTop Referenced Symbols:");
                    for (i, sym) in map.top_referenced_symbols.iter().enumerate() {
                        let citation = sym.span.map_or_else(
                            || " [no_span_module_level]".to_string(),
                            |span| {
                                let path = sym.repo_relative_path.as_deref().unwrap_or("");
                                format!(" @ {path}:{}", span.start_line)
                            },
                        );
                        println!(
                            "{}. {} degree={}{} ({})",
                            i + 1,
                            sym.name,
                            sym.inbound_degree,
                            citation,
                            sym.record_id
                        );
                    }
                }
            }
            Ok(())
        }
        Err(query::OrientationError::EmptyGraph) => {
            let code = "empty_graph";
            let msg = "graph has zero code-graph nodes";
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": msg
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {msg}");
                }
            }
            std::process::exit(3);
        }
        Err(query::OrientationError::NoEntryPoints) => {
            let code = "no_entry_points";
            let msg = "no entry-point files found in the graph";
            match format {
                OutputFormat::Json => {
                    let envelope = serde_json::json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": msg
                        }
                    });
                    println!("{}", serde_json::to_string(&envelope)?);
                }
                OutputFormat::Text => {
                    eprintln!("Error: {msg}");
                }
            }
            std::process::exit(4);
        }
    }
}

pub(crate) fn print_tree_node_text(node: &query::ModuleTreeNode, indent: usize) {
    let indent_str = "  ".repeat(indent);
    let is_dir = matches!(node.kind, query::ModuleNodeKind::Directory);
    let suffix = if is_dir { "/" } else { "" };
    let citation = node.absent_handle_reason.as_ref().map_or_else(
        || {
            node.record_id.as_ref().map_or_else(
                || format!("@ {}", node.path),
                |id| format!("({id}) @ {}", node.path),
            )
        },
        |reason| {
            let reason_str = match reason {
                crate::citation_audit::AbsentHandleRule::NoSpanModuleLevel => {
                    "no_span_module_level"
                }
                crate::citation_audit::AbsentHandleRule::NoSpanDriftTargetUnresolved => {
                    "no_span_drift_target_unresolved"
                }
            };
            format!("[{reason_str}]")
        },
    );
    println!(
        "{}- {}{} {} ({} symbols)",
        indent_str, node.name, suffix, citation, node.symbol_count
    );
    for child in &node.children {
        print_tree_node_text(child, indent + 1);
    }
}
