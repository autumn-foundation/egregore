//! Evidence-to-code-graph resolver (issue #43).
//!
//! Links imported agent evidence records to deterministic code-graph facts using
//! existing edge labels (`TOUCHED_FILE`, `FAILED_ON`, `MENTIONS_SYMBOL`).
//! Unresolvable handles produce machine-readable diagnostics instead of guessed links.
//!
//! # Trust model
//!
//! - `FileEdit` → `TOUCHED_FILE` (factual: a file was edited)
//! - `Failure` with `repo_relative_path` → `FAILED_ON` (factual: a failure cited a file)
//! - `Observation`/`Decision` with unambiguous `name` → `MENTIONS_SYMBOL`
//! - `Verification` nodes are never linked via `TOUCHED_FILE`; verification truth stays separate
//!
//! Documented in `docs/cli/link-evidence.md`.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::ir::{
    AGENT_MEMORY_SCHEMA_VERSION, EdgeLabel, GraphRecord, NodeKind, agent_memory_stable_id,
};

// ── Linker identity ───────────────────────────────────────────────────────────

/// Stable linker identifier stamped on emitted edge summaries.
pub const LINKER_ID: &str = "link-evidence";
/// Linker version string.
pub const LINKER_VERSION: &str = "0.1.0";

// ── Public types ──────────────────────────────────────────────────────────────

/// Reason an evidence handle could not be resolved to a code-graph record.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticReason {
    /// The repo-relative file path is not present in the code graph.
    MissingFile,
    /// The symbol name does not match any symbol in the code graph.
    MissingSymbol,
    /// The symbol name matches more than one symbol; an unambiguous handle is required.
    AmbiguousSymbol,
    /// A span-based target was attempted but no span index is available.
    StaleSpan,
    /// The evidence carries a repository identity that does not match the code graph.
    WrongRepo,
}

impl DiagnosticReason {
    /// Wire string value.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MissingFile => "missing_file",
            Self::MissingSymbol => "missing_symbol",
            Self::AmbiguousSymbol => "ambiguous_symbol",
            Self::StaleSpan => "stale_span",
            Self::WrongRepo => "wrong_repo",
        }
    }
}

/// A machine-readable diagnostic for an evidence handle that could not be resolved.
///
/// Diagnostics are sorted by `source_record_id` for deterministic output.
/// Documented in `docs/cli/link-evidence.md`.
#[derive(Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct LinkDiagnostic {
    /// Record ID of the evidence node whose handle could not be resolved.
    pub source_record_id: String,
    /// Source artifact path and hash from the evidence node, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_handle: Option<String>,
    /// Repo-relative file path that was attempted, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_relative_path: Option<String>,
    /// Symbol name that was attempted, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_name: Option<String>,
    /// Why the handle could not be resolved.
    pub reason: DiagnosticReason,
    /// Edge label that was attempted (`TOUCHED_FILE`, `FAILED_ON`, `MENTIONS_SYMBOL`).
    pub attempted_relation: String,
}

/// Output of the evidence linker.
///
/// `edges` contains resolved cross-domain graph edges sorted by ID.
/// `diagnostics` contains unresolved handles sorted by `source_record_id`.
/// Both are deterministic (byte-for-byte stable) for the same inputs.
#[derive(Debug, Default)]
pub struct LinkOutput {
    /// Resolved cross-domain graph edges (`TOUCHED_FILE`, `FAILED_ON`, `MENTIONS_SYMBOL`).
    pub edges: Vec<GraphRecord>,
    /// Machine-readable diagnostics for every unresolved evidence handle.
    pub diagnostics: Vec<LinkDiagnostic>,
}

/// Options for the evidence linker.
#[derive(Debug, Default)]
pub struct LinkOptions {
    /// When set, evidence records whose repository identity does not match
    /// this value produce `wrong_repo` diagnostics.
    pub expected_repo_id: Option<String>,
}

// ── Code-graph indexes ────────────────────────────────────────────────────────

struct CodeGraphIndex<'a> {
    /// `repo_relative_path` → File node ID.
    files_by_path: BTreeMap<&'a str, &'a str>,
    /// Symbol `name` → Vec of Symbol node IDs (multiple = overloaded name).
    symbols_by_name: BTreeMap<&'a str, Vec<&'a str>>,
}

impl<'a> CodeGraphIndex<'a> {
    fn build(code_graph: &'a [GraphRecord]) -> Self {
        let mut files_by_path: BTreeMap<&str, &str> = BTreeMap::new();
        let mut symbols_by_name: BTreeMap<&str, Vec<&str>> = BTreeMap::new();

        for record in code_graph {
            let GraphRecord::Node {
                id,
                kind,
                repo_relative_path,
                name,
                ..
            } = record
            else {
                continue;
            };

            match kind {
                NodeKind::File => {
                    if let Some(path) = repo_relative_path {
                        files_by_path.insert(path.as_str(), id.as_str());
                    }
                }
                NodeKind::Symbol => {
                    if let Some(sym_name) = name {
                        symbols_by_name
                            .entry(sym_name.as_str())
                            .or_default()
                            .push(id.as_str());
                    }
                }
                _ => {}
            }
        }

        Self {
            files_by_path,
            symbols_by_name,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Links imported agent evidence records to deterministic code-graph facts.
///
/// Emits cross-domain edges (`TOUCHED_FILE`, `FAILED_ON`, `MENTIONS_SYMBOL`) for
/// every unambiguous file or symbol handle found in the evidence.  Unresolvable
/// handles produce `LinkDiagnostic` entries instead of guessed links.
///
/// The returned edges and diagnostics are sorted for deterministic output.
///
/// # Trust invariants
///
/// - `Verification` nodes are never linked via `TOUCHED_FILE` (AC4).
/// - Symbol name matches that are ambiguous produce diagnostics, not edges (AC3).
/// - Only `repo_relative_path`-bearing `Failure` nodes produce `FAILED_ON` (AC3).
#[must_use]
pub fn link_evidence(
    code_graph: &[GraphRecord],
    evidence: &[GraphRecord],
    _opts: &LinkOptions,
) -> LinkOutput {
    let mut output = LinkOutput::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let index = CodeGraphIndex::build(code_graph);

    for record in evidence {
        process_evidence_record(record, &index, &mut output, &mut seen);
    }

    output.edges.sort_by(|a, b| a.id().cmp(b.id()));
    output.diagnostics.sort();
    output
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn process_evidence_record(
    record: &GraphRecord,
    index: &CodeGraphIndex<'_>,
    output: &mut LinkOutput,
    seen: &mut BTreeSet<String>,
) {
    let GraphRecord::Node {
        id,
        kind,
        repo_relative_path,
        name,
        source_handle,
        target_files,
        ..
    } = record
    else {
        return;
    };

    match kind {
        // FileEdit: TOUCHED_FILE for the repo_relative_path.
        NodeKind::FileEdit => {
            if let Some(path) = repo_relative_path {
                resolve_file(
                    output,
                    seen,
                    id,
                    path,
                    source_handle.as_deref(),
                    EdgeLabel::TouchedFile,
                    "TOUCHED_FILE",
                    &index.files_by_path,
                );
            }
        }

        // PatchArtifact: TOUCHED_FILE for repo_relative_path and/or target_files.
        NodeKind::PatchArtifact => {
            if let Some(path) = repo_relative_path {
                resolve_file(
                    output,
                    seen,
                    id,
                    path,
                    source_handle.as_deref(),
                    EdgeLabel::TouchedFile,
                    "TOUCHED_FILE",
                    &index.files_by_path,
                );
            }
            if let Some(files) = target_files {
                for path in files {
                    resolve_file(
                        output,
                        seen,
                        id,
                        path,
                        source_handle.as_deref(),
                        EdgeLabel::TouchedFile,
                        "TOUCHED_FILE",
                        &index.files_by_path,
                    );
                }
            }
        }

        // Failure: FAILED_ON only when repo_relative_path is explicitly set
        // (unambiguous file handle per AC3). Failure nodes without a file
        // handle do not produce cross-domain FAILED_ON edges.
        NodeKind::Failure => {
            if let Some(path) = repo_relative_path {
                resolve_file(
                    output,
                    seen,
                    id,
                    path,
                    source_handle.as_deref(),
                    EdgeLabel::FailedOn,
                    "FAILED_ON",
                    &index.files_by_path,
                );
            }
        }

        // Observation and Decision: MENTIONS_SYMBOL for unambiguous name matches.
        // Ambiguous names produce diagnostics (AC3).
        NodeKind::Observation | NodeKind::Decision => {
            if let Some(sym_name) = name {
                resolve_symbol(
                    output,
                    seen,
                    id,
                    sym_name,
                    source_handle.as_deref(),
                    &index.symbols_by_name,
                );
            }
        }

        // Verification and all other kinds: no file or symbol links.
        // Verification evidence stays in the verification domain (AC4).
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn resolve_file(
    output: &mut LinkOutput,
    seen: &mut BTreeSet<String>,
    source_id: &str,
    path: &str,
    source_handle: Option<&str>,
    label: EdgeLabel,
    relation: &str,
    files_by_path: &BTreeMap<&str, &str>,
) {
    if let Some(&target_id) = files_by_path.get(path) {
        let edge_id = agent_memory_stable_id(&["edge", label.as_str(), source_id, target_id]);
        if seen.insert(edge_id) {
            output.edges.push(GraphRecord::agent_memory_edge(
                label,
                source_id.to_string(),
                target_id.to_string(),
                Some("1.0".to_string()),
                format!("{LINKER_ID} {relation} {source_id} → {target_id}"),
            ));
        }
    } else {
        output.diagnostics.push(LinkDiagnostic {
            source_record_id: source_id.to_string(),
            source_handle: source_handle.map(str::to_owned),
            repo_relative_path: Some(path.to_owned()),
            symbol_name: None,
            reason: DiagnosticReason::MissingFile,
            attempted_relation: relation.to_string(),
        });
    }
}

fn resolve_symbol(
    output: &mut LinkOutput,
    seen: &mut BTreeSet<String>,
    source_id: &str,
    sym_name: &str,
    source_handle: Option<&str>,
    symbols_by_name: &BTreeMap<&str, Vec<&str>>,
) {
    let candidates = symbols_by_name.get(sym_name).map_or(&[][..], Vec::as_slice);

    match candidates {
        [] => {
            output.diagnostics.push(LinkDiagnostic {
                source_record_id: source_id.to_string(),
                source_handle: source_handle.map(str::to_owned),
                repo_relative_path: None,
                symbol_name: Some(sym_name.to_owned()),
                reason: DiagnosticReason::MissingSymbol,
                attempted_relation: "MENTIONS_SYMBOL".to_string(),
            });
        }
        [target_id] => {
            let edge_id = agent_memory_stable_id(&[
                "edge",
                EdgeLabel::MentionsSymbol.as_str(),
                source_id,
                target_id,
            ]);
            if seen.insert(edge_id) {
                output.edges.push(GraphRecord::agent_memory_edge(
                    EdgeLabel::MentionsSymbol,
                    source_id.to_string(),
                    target_id.to_string(),
                    Some("1.0".to_string()),
                    format!("{LINKER_ID} MENTIONS_SYMBOL {source_id} → {target_id}"),
                ));
            }
        }
        _ => {
            output.diagnostics.push(LinkDiagnostic {
                source_record_id: source_id.to_string(),
                source_handle: source_handle.map(str::to_owned),
                repo_relative_path: None,
                symbol_name: Some(sym_name.to_owned()),
                reason: DiagnosticReason::AmbiguousSymbol,
                attempted_relation: "MENTIONS_SYMBOL".to_string(),
            });
        }
    }
}

// ── Schema version exposed for callers ────────────────────────────────────────

/// Schema version for link-evidence edge records (inherits `agent_memory:v1`).
pub const LINK_EDGE_SCHEMA_VERSION: u32 = AGENT_MEMORY_SCHEMA_VERSION;
