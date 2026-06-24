use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::cli::OutputFormat;
use crate::ir::{GraphRecord, SourceSpan};

/// Target source byte and line span for expected labels.
#[derive(Debug, Clone, Deserialize)]
pub struct SpanSpec {
    /// Start byte of the span, inclusive.
    pub start_byte: usize,
    /// End byte of the span, exclusive.
    pub end_byte: usize,
    /// One-based start line.
    pub start_line: usize,
    /// One-based end line.
    pub end_line: usize,
}

/// An expected node in the labeled corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct LabeledNode {
    /// Local expected node identifier.
    pub id: String,
    /// Node kind (e.g. Symbol, Import, Module, etc.).
    pub kind: String,
    /// Name of the node.
    pub name: Option<String>,
    /// Repository-relative path of the file containing the node.
    pub repo_relative_path: Option<String>,
    /// Sub-category of a Symbol node (e.g. function, method).
    pub symbol_kind: Option<String>,
    /// Expected source span.
    pub span: Option<SpanSpec>,
}

/// An expected edge in the labeled corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct LabeledEdge {
    /// Edge relationship label.
    pub label: String,
    /// Source node local ID.
    pub source: String,
    /// Target node local ID.
    pub target: String,
}

/// Gated precision and recall target thresholds.
#[derive(Debug, Clone, Deserialize)]
pub struct MetricThreshold {
    /// Target minimum precision.
    pub precision: f64,
    /// Target minimum recall.
    pub recall: f64,
}

/// Precision and recall threshold configurations for nodes and edges.
#[derive(Debug, Clone, Deserialize)]
pub struct ThresholdsConfig {
    /// Target thresholds per node kind.
    pub nodes: Option<BTreeMap<String, MetricThreshold>>,
    /// Target thresholds per edge label.
    pub edges: Option<BTreeMap<String, MetricThreshold>>,
}

/// Complete labeled accuracy corpus document.
#[derive(Debug, Clone, Deserialize)]
pub struct AccuracyLabels {
    /// Targeted metric thresholds.
    pub thresholds: Option<ThresholdsConfig>,
    /// Expected nodes.
    pub nodes: Vec<LabeledNode>,
    /// Expected edges.
    pub edges: Vec<LabeledEdge>,
}

/// Computed precision, recall, and TP/FP/FN counts for a class.
#[derive(Debug, Clone, Serialize)]
pub struct MetricReport {
    /// True positive count.
    pub true_positives: usize,
    /// False positive count.
    pub false_positives: usize,
    /// False negative count.
    pub false_negatives: usize,
    /// Computed precision ratio.
    pub precision: f64,
    /// Computed recall ratio.
    pub recall: f64,
}

/// Serializable node description for accuracy audit reporting.
#[derive(Debug, Clone, Serialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct NodeInfo {
    /// Stable record ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Node kind.
    pub kind: String,
    /// Node name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Repository-relative path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Symbol specific kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_kind: Option<String>,
    /// Source span.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub span: Option<SourceSpan>,
}

/// Serializable edge description for accuracy audit reporting.
#[derive(Debug, Clone, Serialize, Ord, PartialOrd, Eq, PartialEq)]
pub struct EdgeInfo {
    /// Stable record ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Edge relation label.
    pub label: String,
    /// Source node name or ID.
    pub source: String,
    /// Target node name or ID.
    pub target: String,
}

/// Lists of false positives and false negatives for debugging.
#[derive(Debug, Clone, Serialize)]
pub struct FailureLists {
    /// Scanned entities that were not expected.
    pub false_positives: Vec<serde_json::Value>,
    /// Expected entities that were not found in the scan.
    pub false_negatives: Vec<serde_json::Value>,
}

/// Diagnostic details for a below-threshold metric.
#[derive(Debug, Clone, Serialize)]
pub struct AccuracyDiagnostic {
    /// Warning code.
    pub code: String,
    /// Inspected class/label.
    pub class: String,
    /// Failed metric name.
    pub metric: String,
    /// Observed value.
    pub observed: f64,
    /// Configured threshold.
    pub threshold: f64,
}

/// Complete extraction accuracy evaluation report.
#[derive(Debug, Clone, Serialize)]
pub struct AccuracyReport {
    /// True if all checked metrics meet their thresholds.
    pub ok: bool,
    /// Metric reports grouped by domain ("nodes" or "edges") and class.
    pub metrics: BTreeMap<String, BTreeMap<String, MetricReport>>,
    /// Detailed FP/FN failure lists grouped by domain and class.
    pub failures: BTreeMap<String, BTreeMap<String, FailureLists>>,
    /// Diagnosed threshold violations.
    pub diagnostics: Vec<AccuracyDiagnostic>,
}

const fn span_matches(actual: &SourceSpan, expected: &SpanSpec, tolerance: usize) -> bool {
    let start_diff = actual.start_line.abs_diff(expected.start_line);
    let end_diff = actual.end_line.abs_diff(expected.end_line);
    start_diff <= tolerance && end_diff <= tolerance
}

fn accuracy_exit(code: &str, path: &str, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({ "code": code, "path": path, "message": message })
    );
    std::process::exit(2);
}

/// Runs the extraction accuracy evaluation command.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub(crate) fn eval_accuracy_cmd(
    corpus_dir: &Path,
    labels_path: &Path,
    span_line_tolerance: usize,
    min_precision: Option<f64>,
    min_recall: Option<f64>,
    format: OutputFormat,
) -> Result<()> {
    // 1. Load labels file
    let labels_text = fs::read_to_string(labels_path).unwrap_or_else(|e| {
        accuracy_exit(
            "labels_load_failed",
            &labels_path.to_string_lossy(),
            &e.to_string(),
        )
    });
    let labels: AccuracyLabels = serde_json::from_str(&labels_text).unwrap_or_else(|e| {
        accuracy_exit(
            "labels_parse_failed",
            &labels_path.to_string_lossy(),
            &e.to_string(),
        )
    });

    // 2. Scan corpus
    let graph = crate::scan_repository_with_exclusions(corpus_dir, Some("accuracy-corpus"), &[])
        .unwrap_or_else(|e| {
            accuracy_exit(
                "corpus_scan_failed",
                &corpus_dir.to_string_lossy(),
                &e.to_string(),
            )
        });

    // 3. Separate actual nodes and edges
    let mut actual_nodes = Vec::new();
    let mut actual_edges = Vec::new();

    for record in graph.records() {
        match record {
            GraphRecord::Node {
                id,
                kind,
                repo_relative_path,
                span,
                name,
                symbol_kind,
                ..
            } => {
                actual_nodes.push((id, kind, repo_relative_path, span, name, symbol_kind));
            }
            GraphRecord::Edge {
                id,
                label,
                source,
                target,
                ..
            } => {
                actual_edges.push((id, label, source, target));
            }
            GraphRecord::Tombstone { .. } => {}
        }
    }

    // 4. Greedy 1-to-1 matching of expected nodes to actual nodes
    let mut matched_expected = HashSet::new(); // indices in labels.nodes
    let mut matched_actual = HashSet::new(); // IDs of actual nodes
    let mut actual_id_to_expected_idx = HashMap::new(); // actual node id -> index in labels.nodes

    for (expected_idx, exp) in labels.nodes.iter().enumerate() {
        for &(act_id, act_kind, act_path, act_span, act_name, act_symbol_kind) in &actual_nodes {
            if matched_actual.contains(act_id) {
                continue;
            }

            // A. Kind must match
            if act_kind.as_str() != exp.kind {
                continue;
            }

            // B. Repo-relative path must match (normalize slashes)
            let path_matches = match (act_path, &exp.repo_relative_path) {
                (Some(ap), Some(ep)) => {
                    let ap_norm = ap.replace('\\', "/");
                    let ep_norm = ep.replace('\\', "/");
                    ap_norm == ep_norm
                }
                (None, None) => true,
                _ => false,
            };
            if !path_matches {
                continue;
            }

            // C. Name must match
            let name_matches = match (act_name, &exp.name) {
                (Some(an), Some(en)) => an == en,
                (None, None) => true,
                _ => false,
            };
            if !name_matches {
                continue;
            }

            // D. Symbol kind must match if expected has it
            if let Some(exp_sk) = &exp.symbol_kind {
                match act_symbol_kind {
                    Some(ask) => {
                        if ask != exp_sk {
                            continue;
                        }
                    }
                    None => {
                        continue;
                    }
                }
            }

            // E. Span must match if expected has it
            if let Some(exp_span) = &exp.span {
                match act_span {
                    Some(aspan) => {
                        if !span_matches(aspan, exp_span, span_line_tolerance) {
                            continue;
                        }
                    }
                    None => {
                        continue;
                    }
                }
            }

            // Match found!
            matched_expected.insert(expected_idx);
            matched_actual.insert(act_id.clone());
            actual_id_to_expected_idx.insert(act_id.clone(), expected_idx);
            break;
        }
    }

    // 5. Node evaluation metrics
    let mut all_node_kinds = BTreeSet::new();
    for exp in &labels.nodes {
        all_node_kinds.insert(exp.kind.clone());
    }
    for &(_, act_kind, _, _, _, _) in &actual_nodes {
        all_node_kinds.insert(act_kind.as_str().to_string());
    }

    let mut node_metrics = BTreeMap::new();
    let mut node_failures = BTreeMap::new();

    for kind in all_node_kinds {
        let mut tp = 0;
        let mut false_positives_list = Vec::new();
        let mut false_negatives_list = Vec::new();

        // Check expected
        for (expected_idx, exp) in labels.nodes.iter().enumerate() {
            if exp.kind == kind {
                if matched_expected.contains(&expected_idx) {
                    tp += 1;
                } else {
                    false_negatives_list.push(NodeInfo {
                        id: Some(exp.id.clone()),
                        kind: exp.kind.clone(),
                        name: exp.name.clone(),
                        repo_relative_path: exp.repo_relative_path.clone(),
                        symbol_kind: exp.symbol_kind.clone(),
                        span: exp.span.as_ref().map(|s| SourceSpan {
                            start_byte: s.start_byte,
                            end_byte: s.end_byte,
                            start_line: s.start_line,
                            end_line: s.end_line,
                        }),
                    });
                }
            }
        }

        // Check actual
        for &(act_id, act_kind, act_path, act_span, act_name, act_symbol_kind) in &actual_nodes {
            if act_kind.as_str() == kind && !matched_actual.contains(act_id) {
                false_positives_list.push(NodeInfo {
                    id: Some(act_id.clone()),
                    kind: act_kind.as_str().to_string(),
                    name: act_name.clone(),
                    repo_relative_path: act_path.clone(),
                    symbol_kind: act_symbol_kind.clone(),
                    span: *act_span,
                });
            }
        }

        // Sort FPs and FNs for stable output
        false_positives_list.sort();
        false_negatives_list.sort();

        let fp = false_positives_list.len();
        let fn_count = false_negatives_list.len();

        let precision = if tp + fp > 0 {
            tp as f64 / (tp + fp) as f64
        } else {
            1.0
        };

        let recall = if tp + fn_count > 0 {
            tp as f64 / (tp + fn_count) as f64
        } else {
            1.0
        };

        node_metrics.insert(
            kind.clone(),
            MetricReport {
                true_positives: tp,
                false_positives: fp,
                false_negatives: fn_count,
                precision,
                recall,
            },
        );

        let false_positives_json = false_positives_list
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        let false_negatives_json = false_negatives_list
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        node_failures.insert(kind, (false_positives_json, false_negatives_json));
    }

    // 6. Match expected edges
    let mut matched_expected_edges = HashSet::new(); // indices in labels.edges
    let mut matched_actual_edges = HashSet::new(); // IDs of actual edges

    for &(act_id, act_label, act_source, act_target) in &actual_edges {
        let source_idx = actual_id_to_expected_idx.get(act_source);
        let target_idx = actual_id_to_expected_idx.get(act_target);

        if let (Some(&s_idx), Some(&t_idx)) = (source_idx, target_idx) {
            let exp_source_id = &labels.nodes[s_idx].id;
            let exp_target_id = &labels.nodes[t_idx].id;

            let mut found_match = false;
            for (exp_edge_idx, exp_edge) in labels.edges.iter().enumerate() {
                if matched_expected_edges.contains(&exp_edge_idx) {
                    continue;
                }
                if exp_edge.label == act_label.as_str()
                    && exp_edge.source == *exp_source_id
                    && exp_edge.target == *exp_target_id
                {
                    matched_expected_edges.insert(exp_edge_idx);
                    matched_actual_edges.insert(act_id.clone());
                    found_match = true;
                    break;
                }
            }
            if found_match {
                // Done
            }
        }
    }

    // 7. Edge evaluation metrics
    let mut all_edge_labels = BTreeSet::new();
    for exp in &labels.edges {
        all_edge_labels.insert(exp.label.clone());
    }
    for &(_, act_label, _, _) in &actual_edges {
        all_edge_labels.insert(act_label.as_str().to_string());
    }

    let mut edge_metrics = BTreeMap::new();
    let mut edge_failures = BTreeMap::new();

    for label in all_edge_labels {
        let mut tp = 0;
        let mut false_positives_list = Vec::new();
        let mut false_negatives_list = Vec::new();

        // Expected edges
        for (exp_idx, exp) in labels.edges.iter().enumerate() {
            if exp.label == label {
                if matched_expected_edges.contains(&exp_idx) {
                    tp += 1;
                } else {
                    false_negatives_list.push(EdgeInfo {
                        id: None,
                        label: exp.label.clone(),
                        source: exp.source.clone(),
                        target: exp.target.clone(),
                    });
                }
            }
        }

        // Actual edges
        for &(act_id, act_label, act_source, act_target) in &actual_edges {
            if act_label.as_str() == label && !matched_actual_edges.contains(act_id) {
                let source_display = actual_nodes
                    .iter()
                    .find(|t| t.0 == act_source)
                    .and_then(|t| t.4.as_ref())
                    .cloned()
                    .unwrap_or_else(|| act_source.clone());

                let target_display = actual_nodes
                    .iter()
                    .find(|t| t.0 == act_target)
                    .and_then(|t| t.4.as_ref())
                    .cloned()
                    .unwrap_or_else(|| act_target.clone());

                false_positives_list.push(EdgeInfo {
                    id: Some(act_id.clone()),
                    label: act_label.as_str().to_string(),
                    source: source_display,
                    target: target_display,
                });
            }
        }

        // Sort for stable output
        false_positives_list.sort();
        false_negatives_list.sort();

        let fp = false_positives_list.len();
        let fn_count = false_negatives_list.len();

        let precision = if tp + fp > 0 {
            tp as f64 / (tp + fp) as f64
        } else {
            1.0
        };

        let recall = if tp + fn_count > 0 {
            tp as f64 / (tp + fn_count) as f64
        } else {
            1.0
        };

        edge_metrics.insert(
            label.clone(),
            MetricReport {
                true_positives: tp,
                false_positives: fp,
                false_negatives: fn_count,
                precision,
                recall,
            },
        );

        let false_positives_json = false_positives_list
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        let false_negatives_json = false_negatives_list
            .iter()
            .map(|item| serde_json::to_value(item).unwrap())
            .collect();
        edge_failures.insert(label, (false_positives_json, false_negatives_json));
    }

    // 8. Apply precision/recall thresholds and generate diagnostics
    let get_precision_threshold = |class: &str, is_edge: bool| -> f64 {
        if let Some(p) = min_precision {
            return p;
        }
        if let Some(tc) = &labels.thresholds {
            let map = if is_edge { &tc.edges } else { &tc.nodes };
            if let Some(t) = map.as_ref().and_then(|m| m.get(class)) {
                return t.precision;
            }
        }
        if class == "Symbol" || class == "DEFINES" {
            0.98
        } else {
            0.0
        }
    };

    let get_recall_threshold = |class: &str, is_edge: bool| -> f64 {
        if let Some(r) = min_recall {
            return r;
        }
        if let Some(tc) = &labels.thresholds {
            let map = if is_edge { &tc.edges } else { &tc.nodes };
            if let Some(t) = map.as_ref().and_then(|m| m.get(class)) {
                return t.recall;
            }
        }
        if class == "Symbol" || class == "DEFINES" {
            0.95
        } else {
            0.0
        }
    };

    let mut ok = true;
    let mut diagnostics = Vec::new();

    for (kind, metric) in &node_metrics {
        let p_thresh = get_precision_threshold(kind, false);
        let r_thresh = get_recall_threshold(kind, false);

        if p_thresh > 0.0 && metric.precision < p_thresh {
            ok = false;
            diagnostics.push(AccuracyDiagnostic {
                code: "below_precision_threshold".to_string(),
                class: kind.clone(),
                metric: "precision".to_string(),
                observed: metric.precision,
                threshold: p_thresh,
            });
        }
        if r_thresh > 0.0 && metric.recall < r_thresh {
            ok = false;
            diagnostics.push(AccuracyDiagnostic {
                code: "below_recall_threshold".to_string(),
                class: kind.clone(),
                metric: "recall".to_string(),
                observed: metric.recall,
                threshold: r_thresh,
            });
        }
    }

    for (label, metric) in &edge_metrics {
        let p_thresh = get_precision_threshold(label, true);
        let r_thresh = get_recall_threshold(label, true);

        if p_thresh > 0.0 && metric.precision < p_thresh {
            ok = false;
            diagnostics.push(AccuracyDiagnostic {
                code: "below_precision_threshold".to_string(),
                class: label.clone(),
                metric: "precision".to_string(),
                observed: metric.precision,
                threshold: p_thresh,
            });
        }
        if r_thresh > 0.0 && metric.recall < r_thresh {
            ok = false;
            diagnostics.push(AccuracyDiagnostic {
                code: "below_recall_threshold".to_string(),
                class: label.clone(),
                metric: "recall".to_string(),
                observed: metric.recall,
                threshold: r_thresh,
            });
        }
    }

    // 9. Format output
    let report = AccuracyReport {
        ok,
        metrics: {
            let mut m = BTreeMap::new();
            m.insert("nodes".to_string(), node_metrics);
            m.insert("edges".to_string(), edge_metrics);
            m
        },
        failures: {
            let mut f = BTreeMap::new();
            let node_fails = node_failures
                .into_iter()
                .map(|(k, (fps, fns))| {
                    (
                        k,
                        FailureLists {
                            false_positives: fps,
                            false_negatives: fns,
                        },
                    )
                })
                .collect();
            let edge_fails = edge_failures
                .into_iter()
                .map(|(k, (fps, fns))| {
                    (
                        k,
                        FailureLists {
                            false_positives: fps,
                            false_negatives: fns,
                        },
                    )
                })
                .collect();
            f.insert("nodes".to_string(), node_fails);
            f.insert("edges".to_string(), edge_fails);
            f
        },
        diagnostics: diagnostics.clone(),
    };

    match format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        OutputFormat::Text => {
            println!("Extraction Accuracy Gate Passed: {}", report.ok);
            println!("\nNode Classes:");
            if let Some(nodes_map) = report.metrics.get("nodes") {
                for (kind, metric) in nodes_map {
                    println!(
                        "  {:<15} Precision: {:.3} ({}/{}), Recall: {:.3} ({}/{})",
                        kind,
                        metric.precision,
                        metric.true_positives,
                        metric.true_positives + metric.false_positives,
                        metric.recall,
                        metric.true_positives,
                        metric.true_positives + metric.false_negatives
                    );
                }
            }
            println!("\nEdge Classes:");
            if let Some(edges_map) = report.metrics.get("edges") {
                for (label, metric) in edges_map {
                    println!(
                        "  {:<15} Precision: {:.3} ({}/{}), Recall: {:.3} ({}/{})",
                        label,
                        metric.precision,
                        metric.true_positives,
                        metric.true_positives + metric.false_positives,
                        metric.recall,
                        metric.true_positives,
                        metric.true_positives + metric.false_negatives
                    );
                }
            }
        }
    }

    if !ok {
        for diag in &diagnostics {
            eprintln!(
                "Error: {} {} is {:.3}, which is below the threshold of {:.3}",
                diag.class, diag.metric, diag.observed, diag.threshold
            );
        }
        anyhow::bail!("accuracy thresholds gate failed")
    }

    Ok(())
}
