//! Backtrace stack-frame resolution to code-graph symbols (`resolve-frames`,
//! issue #322).
//!
//! Given a log graph carrying `ErrorSignature` records with structured,
//! redaction-safe backtrace frames (issue #322 scan-time capture) and a code
//! graph of `File`/`Module`/`Symbol` records, this resolver binds each frame to
//! a code-graph target and emits a `FRAME_RESOLVES_TO` edge labeled with a
//! [`FrameResolution`] from the closed set `{resolved, ambiguous, path_only,
//! unresolved}` plus the zero-based `frame_index`.
//!
//! The resolution ladder mirrors the [`CallResolution`](crate::ir::CallResolution)
//! precedent (issues #152/#134): a frame handle is never silently bound to an
//! invented target.
//!
//! 1. **Frame `file:line`** → the smallest enclosing `Symbol`
//!    ([`location_context`]). A hit is **resolved** (edge → that symbol); a file
//!    that exists with no enclosing symbol (optimized-out / macro-generated
//!    frame) is **path_only** (edge → the `File` node).
//! 2. **Module-path name only** (no usable `file:line`) → exact `Symbol`-name
//!    lookup. Exactly one match is **resolved**; two or more is **ambiguous**,
//!    and *every* candidate gets its own edge — no candidate is silently chosen.
//! 3. **A repo-relative path absent from the resolved view** (deleted / renamed
//!    since the log) is **unresolved** → the edge targets a `Diagnostic` node
//!    carrying the redacted frame text, never an invented symbol.
//! 4. **A standard-library or dependency frame** is **external**: tallied per
//!    signature and minting **no** edge.
//!
//! Every emitted edge is mirrored by an [`EvidenceLink`] appended to the
//! `ErrorSignature` node so the two representations agree at write time. Output
//! is deterministic and byte-identical across runs; raw log payload text never
//! enters the graph.
//!
//! A frame binding proves only that the frame *names* the symbol — never that
//! the symbol is at fault.

use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{
    EdgeLabel, EvidenceLink, FrameResolution, GraphRecord, LOG_SCHEMA_VERSION, LogPayload,
    NodeKind, SourceSpan, StackFrame, log_stable_id,
};
use crate::query::{RepositoryIndex, location_context};
use crate::schema_version::domain_from_record_id;

/// Standard-library / compiler crate roots. A frame whose module path begins
/// with one of these is classified `external` and mints no edge.
const STDLIB_MODULE_ROOTS: [&str; 6] = ["std", "core", "alloc", "proc_macro", "test", "backtrace"];

/// Normalized external-toolchain file-path prefixes (produced by
/// `log_graph::normalize_frame_path`). A frame file under one of these is
/// classified `external`.
const EXTERNAL_PATH_PREFIXES: [&str; 5] = ["rustc/", "registry/", ".cargo/", ".rustup/", "git/"];

/// Per-signature resolution tally (deterministic; used in the CLI envelope).
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct SignatureTally {
    /// Stable `ErrorSignature` record ID.
    pub signature_id: String,
    /// Frames resolved to exactly one symbol.
    pub resolved: u64,
    /// Frames resolving to two or more candidate symbols.
    pub ambiguous: u64,
    /// Frames bound to a `File` node (no enclosing symbol).
    pub path_only: u64,
    /// Frames bound to a `Diagnostic` marker (absent repo path).
    pub unresolved: u64,
    /// Frames into the standard library or a dependency (no edge minted).
    pub external: u64,
}

/// Aggregate totals across all resolved signatures.
#[derive(Debug, Clone, Eq, PartialEq, serde::Serialize)]
pub struct ResolveTotals {
    /// Number of signatures that carried at least one backtrace frame.
    pub signatures_with_frames: u64,
    /// Total resolved frames.
    pub resolved: u64,
    /// Total ambiguous frames.
    pub ambiguous: u64,
    /// Total path-only frames.
    pub path_only: u64,
    /// Total unresolved frames.
    pub unresolved: u64,
    /// Total external frames (no edge minted).
    pub external: u64,
}

/// Result of a frame-resolution pass.
#[derive(Debug, Clone)]
pub struct ResolveFramesResult {
    /// Enriched log-domain records (updated `ErrorSignature` nodes, the
    /// original log records, new `Diagnostic` markers, and new
    /// `FRAME_RESOLVES_TO` edges) in canonical order. Never re-emits code-graph
    /// records.
    pub records: Vec<GraphRecord>,
    /// Per-signature tallies, ordered by signature ID.
    pub signatures: Vec<SignatureTally>,
    /// Aggregate totals.
    pub totals: ResolveTotals,
}

/// One pending edge + its mirror evidence link, before canonical assembly.
struct PendingEdge {
    signature_id: String,
    target_id: String,
    frame_index: u32,
    resolution: FrameResolution,
    confidence: &'static str,
    target_domain: String,
    target_path: Option<String>,
    target_span: Option<SourceSpan>,
    target_commit: Option<String>,
}

/// Resolves every backtrace frame on every `ErrorSignature` in `records`
/// against the code-graph records in the same slice.
///
/// `at_commit` selects the code-graph view (a resolved commit SHA), matching
/// [`location_context`]'s valid-time pin; pass `None` for the current-state
/// view.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn resolve_frames(records: &[GraphRecord], at_commit: Option<&str>) -> ResolveFramesResult {
    let index = RepositoryIndex::build(records);

    // Symbol-name → set of live symbol record IDs (for name-only frames).
    let mut symbols_by_name: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    let tombstoned: BTreeSet<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Tombstone { deleted_id, .. } => Some(deleted_id.as_str()),
            _ => None,
        })
        .collect();
    for r in records {
        if let GraphRecord::Node {
            id,
            kind: NodeKind::Symbol,
            name: Some(name),
            ..
        } = r
            && !tombstoned.contains(id.as_str())
        {
            symbols_by_name.entry(name).or_default().insert(id);
        }
    }

    // Collect signatures deterministically by ID.
    let mut signature_ids: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::ErrorSignature,
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    signature_ids.sort_unstable();
    signature_ids.dedup();

    let mut pending: Vec<PendingEdge> = Vec::new();
    let mut diagnostics: BTreeMap<String, GraphRecord> = BTreeMap::new();
    let mut tallies: Vec<SignatureTally> = Vec::new();
    let mut totals = ResolveTotals {
        signatures_with_frames: 0,
        resolved: 0,
        ambiguous: 0,
        path_only: 0,
        unresolved: 0,
        external: 0,
    };

    for sig_id in &signature_ids {
        let Some(frames) = frames_of(records, sig_id) else {
            continue;
        };
        if frames.is_empty() {
            continue;
        }
        totals.signatures_with_frames += 1;
        let mut tally = SignatureTally {
            signature_id: (*sig_id).to_owned(),
            resolved: 0,
            ambiguous: 0,
            path_only: 0,
            unresolved: 0,
            external: 0,
        };

        for frame in frames {
            match classify_frame(records, &index, &symbols_by_name, frame, at_commit) {
                FrameOutcome::External => tally.external += 1,
                FrameOutcome::Resolved {
                    target,
                    path,
                    span,
                    commit,
                } => {
                    tally.resolved += 1;
                    pending.push(PendingEdge {
                        signature_id: (*sig_id).to_owned(),
                        target_id: target.clone(),
                        frame_index: frame.frame_index,
                        resolution: FrameResolution::Resolved,
                        confidence: "1.0",
                        target_domain: domain_of(&target),
                        target_path: path,
                        target_span: span,
                        target_commit: commit,
                    });
                }
                FrameOutcome::PathOnly {
                    target,
                    path,
                    commit,
                } => {
                    tally.path_only += 1;
                    pending.push(PendingEdge {
                        signature_id: (*sig_id).to_owned(),
                        target_id: target.clone(),
                        frame_index: frame.frame_index,
                        resolution: FrameResolution::PathOnly,
                        confidence: "0.5",
                        target_domain: domain_of(&target),
                        target_path: path,
                        target_span: None,
                        target_commit: commit,
                    });
                }
                FrameOutcome::Ambiguous(candidates) => {
                    tally.ambiguous += 1;
                    for (target, path, span, commit) in candidates {
                        pending.push(PendingEdge {
                            signature_id: (*sig_id).to_owned(),
                            target_id: target.clone(),
                            frame_index: frame.frame_index,
                            resolution: FrameResolution::Ambiguous,
                            confidence: "0.5",
                            target_domain: domain_of(&target),
                            target_path: path,
                            target_span: span,
                            target_commit: commit,
                        });
                    }
                }
                FrameOutcome::Unresolved => {
                    tally.unresolved += 1;
                    let diag = unresolved_diagnostic(frame);
                    let diag_id = diag.id().to_owned();
                    diagnostics.entry(diag_id.clone()).or_insert(diag);
                    pending.push(PendingEdge {
                        signature_id: (*sig_id).to_owned(),
                        target_id: diag_id,
                        frame_index: frame.frame_index,
                        resolution: FrameResolution::Unresolved,
                        confidence: "0.0",
                        target_domain: "log".to_owned(),
                        target_path: frame.file_path.clone(),
                        target_span: None,
                        target_commit: None,
                    });
                }
            }
        }

        totals.resolved += tally.resolved;
        totals.ambiguous += tally.ambiguous;
        totals.path_only += tally.path_only;
        totals.unresolved += tally.unresolved;
        totals.external += tally.external;
        tallies.push(tally);
    }

    // ── Assemble output: enriched log nodes + diagnostics + edges ────────────
    // Group evidence links per signature so node and edge agree at write time.
    let mut links_by_sig: BTreeMap<&str, Vec<EvidenceLink>> = BTreeMap::new();
    let mut edges: Vec<GraphRecord> = Vec::new();
    for p in &pending {
        let edge_id = log_stable_id(&[
            "edge",
            EdgeLabel::FrameResolvesTo.as_str(),
            &p.signature_id,
            &p.frame_index.to_string(),
            &p.target_id,
            p.resolution.as_str(),
        ]);
        edges.push(GraphRecord::Edge {
            id: edge_id,
            schema_version: LOG_SCHEMA_VERSION,
            label: EdgeLabel::FrameResolvesTo,
            source: p.signature_id.clone(),
            target: p.target_id.clone(),
            confidence: Some(p.confidence.to_owned()),
            resolution: None,
            frame_resolution: Some(p.resolution),
            frame_index: Some(p.frame_index),
            temporal: None,
            summary: format!(
                "backtrace frame {} of {} resolves ({}) to {}",
                p.frame_index,
                p.signature_id,
                p.resolution.as_str(),
                p.target_id
            ),
            producer: None,
        });
        links_by_sig
            .entry(p.signature_id.as_str())
            .or_default()
            .push(EvidenceLink {
                target_record_id: Some(p.target_id.clone()),
                target_domain: p.target_domain.clone(),
                relation: EdgeLabel::FrameResolvesTo.as_str().to_owned(),
                confidence: p.confidence.to_owned(),
                as_of_commit: at_commit.map(str::to_owned),
                target_repo_relative_path: p.target_path.clone(),
                target_span: p.target_span,
                target_git_commit: p.target_commit.clone(),
            });
    }

    // Emit only the log-domain input records, enriching ErrorSignature nodes
    // with their evidence links. Code-graph records are never re-emitted.
    let mut nodes: Vec<GraphRecord> = Vec::new();
    for r in records {
        if domain_from_record_id(r.id()).as_deref() != Some("log") {
            continue;
        }
        match r {
            GraphRecord::Node {
                kind: NodeKind::ErrorSignature,
                id,
                ..
            } => {
                let mut node = r.clone();
                if let Some(links) = links_by_sig.get(id.as_str()) {
                    let mut sorted = links.clone();
                    sort_evidence_links(&mut sorted);
                    node = node.with_evidence_links(sorted);
                }
                nodes.push(node);
            }
            _ => nodes.push(r.clone()),
        }
    }
    nodes.extend(diagnostics.into_values());
    nodes.sort_by(|a, b| a.id().cmp(b.id()));

    edges.sort_by_key(edge_sort_key);

    let mut out = nodes;
    out.extend(edges);

    ResolveFramesResult {
        records: out,
        signatures: tallies,
        totals,
    }
}

/// A resolved candidate target: `(record_id, repo_relative_path, span, commit)`.
type Candidate = (String, Option<String>, Option<SourceSpan>, Option<String>);

/// Outcome of classifying a single frame.
enum FrameOutcome {
    /// Standard-library / dependency frame: tally only, no edge.
    External,
    /// Bound to exactly one symbol.
    Resolved {
        target: String,
        path: Option<String>,
        span: Option<SourceSpan>,
        commit: Option<String>,
    },
    /// Bound to a `File` node (no enclosing symbol).
    PathOnly {
        target: String,
        path: Option<String>,
        commit: Option<String>,
    },
    /// Bound to two or more candidate symbols.
    Ambiguous(Vec<Candidate>),
    /// Named a repo path/name absent from the resolved view.
    Unresolved,
}

fn classify_frame(
    records: &[GraphRecord],
    index: &RepositoryIndex,
    symbols_by_name: &BTreeMap<&str, BTreeSet<&str>>,
    frame: &StackFrame,
    at_commit: Option<&str>,
) -> FrameOutcome {
    if is_external(frame) {
        return FrameOutcome::External;
    }

    // Ladder step 1: usable file:line → smallest enclosing symbol.
    if let (Some(path), Some(line)) = (frame.file_path.as_deref(), frame.line) {
        let ctx = location_context(records, path, line as usize, at_commit, index, None);
        if let Some(primary) = ctx.primary {
            let (span, commit) = span_and_commit(primary);
            return FrameOutcome::Resolved {
                target: primary.id().to_owned(),
                path: Some(path.to_owned()),
                span,
                commit,
            };
        }
        if let Some(file) = ctx.file_record {
            let (_, commit) = span_and_commit(file);
            return FrameOutcome::PathOnly {
                target: file.id().to_owned(),
                path: Some(path.to_owned()),
                commit,
            };
        }
        // File named but absent from the view → unresolved.
        return FrameOutcome::Unresolved;
    }

    // Ladder step 2: module-path name only → exact symbol-name lookup.
    if let Some(simple) = simple_name(frame)
        && let Some(ids) = symbols_by_name.get(simple)
    {
        match ids.len() {
            0 => {}
            1 => {
                let target = (*ids.iter().next().expect("len==1")).to_owned();
                let (path, span, commit) = candidate_handles(records, &target);
                return FrameOutcome::Resolved {
                    target,
                    path,
                    span,
                    commit,
                };
            }
            _ => {
                let candidates = ids
                    .iter()
                    .map(|id| {
                        let (path, span, commit) = candidate_handles(records, id);
                        ((*id).to_owned(), path, span, commit)
                    })
                    .collect();
                return FrameOutcome::Ambiguous(candidates);
            }
        }
    }

    FrameOutcome::Unresolved
}

/// Returns the `(path, span, commit)` handles for a record ID, all `None` when
/// the record is absent.
fn candidate_handles(
    records: &[GraphRecord],
    id: &str,
) -> (Option<String>, Option<SourceSpan>, Option<String>) {
    record_by_id(records, id).map_or((None, None, None), |r| {
        let (span, commit) = span_and_commit(r);
        (path_of(r), span, commit)
    })
}

/// True when the frame is a standard-library or dependency frame.
fn is_external(frame: &StackFrame) -> bool {
    if let Some(module) = frame.module_path.as_deref() {
        let root = module.split("::").next().unwrap_or(module);
        if STDLIB_MODULE_ROOTS.contains(&root) {
            return true;
        }
    }
    if let Some(path) = frame.file_path.as_deref()
        && (EXTERNAL_PATH_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
            || path.contains("/registry/")
            || path.contains("/rustc/"))
    {
        return true;
    }
    false
}

/// The last `::`-delimited segment of a frame's module path (the simple symbol
/// name), stripped of any trailing hash disambiguator (`::h1a2b3c`).
fn simple_name(frame: &StackFrame) -> Option<&str> {
    let module = frame.module_path.as_deref()?;
    let last = module.rsplit("::").next()?;
    if last.is_empty() { None } else { Some(last) }
}

/// Returns the parsed frames of an `ErrorSignature` by ID, if any.
fn frames_of<'a>(records: &'a [GraphRecord], signature_id: &str) -> Option<&'a [StackFrame]> {
    for r in records {
        if r.id() == signature_id
            && let Some(LogPayload::ErrorSignature(payload)) = r.log_payload()
        {
            return payload.frames.as_deref();
        }
    }
    None
}

fn record_by_id<'a>(records: &'a [GraphRecord], id: &str) -> Option<&'a GraphRecord> {
    records.iter().find(|r| r.id() == id)
}

fn path_of(record: &GraphRecord) -> Option<String> {
    match record {
        GraphRecord::Node {
            repo_relative_path, ..
        } => repo_relative_path.clone(),
        _ => None,
    }
}

fn span_and_commit(record: &GraphRecord) -> (Option<SourceSpan>, Option<String>) {
    match record {
        GraphRecord::Node { span, temporal, .. } => {
            (*span, temporal.as_ref().map(|t| t.git_commit.clone()))
        }
        _ => (None, None),
    }
}

fn domain_of(id: &str) -> String {
    domain_from_record_id(id).unwrap_or_else(|| "codegraph".to_owned())
}

/// Builds the log-domain `Diagnostic` node for an unresolved frame, carrying
/// the redacted frame text (never an invented symbol). Content-addressed on the
/// frame text so identical unresolved frames share one marker. The
/// `repo_relative_path` is the (redaction-safe) frame path, a query handle.
fn unresolved_diagnostic(frame: &StackFrame) -> GraphRecord {
    let text = frame_display(frame);
    let id = log_stable_id(&["diagnostic", "unresolved-frame", &text]);
    GraphRecord::node(
        id,
        NodeKind::Diagnostic,
        frame.file_path.clone(),
        None,
        Some(text.clone()),
        format!("unresolved backtrace frame ({text}): no in-repo definition"),
    )
    .with_domain("log", LOG_SCHEMA_VERSION)
}

/// A redaction-safe single-line display of a frame for a diagnostic marker.
fn frame_display(frame: &StackFrame) -> String {
    match (
        frame.module_path.as_deref(),
        frame.file_path.as_deref(),
        frame.line,
    ) {
        (Some(m), Some(f), Some(l)) => format!("{m} at {f}:{l}"),
        (Some(m), Some(f), None) => format!("{m} at {f}"),
        (Some(m), None, _) => m.to_owned(),
        (None, Some(f), Some(l)) => format!("{f}:{l}"),
        (None, Some(f), None) => f.to_owned(),
        (None, None, _) => format!("frame#{}", frame.frame_index),
    }
}

fn edge_sort_key(edge: &GraphRecord) -> (String, u32, String, String) {
    match edge {
        GraphRecord::Edge {
            source,
            target,
            frame_index,
            id,
            ..
        } => (
            source.clone(),
            frame_index.unwrap_or(0),
            target.clone(),
            id.clone(),
        ),
        _ => (String::new(), 0, String::new(), edge.id().to_owned()),
    }
}

/// Sorts evidence links deterministically (target ID, then relation).
fn sort_evidence_links(links: &mut [EvidenceLink]) {
    links.sort_by(|a, b| {
        a.target_record_id
            .cmp(&b.target_record_id)
            .then_with(|| a.relation.cmp(&b.relation))
    });
}
