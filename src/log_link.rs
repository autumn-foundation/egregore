//! Error-signature → agent-run / command correlation linker (`link-logs`,
//! issue #323).
//!
//! Given a union graph of log-domain records (issue #320 `LogSource` /
//! `ErrorSignature` / …), agent-memory & verification records (`AgentRun`,
//! `AgentTurn`, `CommandRun`, …), and project `Task` records, this linker binds
//! each `ErrorSignature` to the agent runs / commands that plausibly produced it
//! and emits `EMITTED_DURING` evidence-link edges. Task/issue linkage reuses the
//! existing `REFERENCES_TASK` evidence-link surface (no new project-facing
//! label).
//!
//! # Correlation bases (closed set — [`CorrelationBasis`])
//!
//! Every `EMITTED_DURING` edge carries exactly one basis; **no edge is ever
//! emitted without one**:
//!
//! 1. **`content_hash_join`** — the `LogSource` a signature was `CAPTURED_FROM`
//!    carries a `source_artifact_hash` equal to a `CommandRun`'s captured
//!    stdout/stderr `OutputHandle.hash`. Exact BLAKE3 byte equality: the log
//!    artifact *is* that command's output. Deterministic and inherently
//!    within-repository. Confidence `1.0`.
//! 2. **`temporal_correlation`** — the signature's representative valid time
//!    (`last_seen`, falling back to `first_seen`) falls inside an
//!    `AgentRun` / `AgentTurn` execution window `[started_at, finished_at]`
//!    (extended by `--tolerance` seconds, default `0` = strict) for the **same
//!    repository**. A *correlation lead, never causation*. Confidence `0.5`.
//!    Overlapping runs each mint their own edge — no single winner is chosen.
//!
//! Both bases may apply to one signature (different targets); that is honest and
//! allowed. A signature is `uncorrelated` only when it received **zero** edges
//! of any basis.
//!
//! # Repository boundary
//!
//! `content_hash_join` is inherently within-repo (byte equality). For
//! `temporal_correlation`, the linker resolves a single **repository anchor**
//! (the sole `Repository` node's ID). Each signature's repository is *verified*
//! by recomputing its `LogSource` stable ID against that anchor
//! (`log_stable_id(["log_source", anchor, source_relative_path,
//! source_artifact_hash])`) — reading the identity the ID hash already encoded,
//! with no new stored field. A signature whose `LogSource` does not recompute to
//! the anchor is foreign: its in-window candidates are suppressed and counted in
//! `cross_repo_rejected`. With zero or multiple `Repository` anchors the linker
//! cannot attribute agent runs to a repository, so `temporal_correlation` is
//! disabled fail-closed (`content_hash_join` is unaffected).
//!
//! Output is deterministic and byte-identical across runs; raw log / transcript
//! / command text never enters the graph.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::ir::{
    CorrelationBasis, EdgeLabel, EvidenceLink, GraphRecord, LOG_SCHEMA_VERSION, LogPayload,
    NodeKind, OutputHandle, log_stable_id,
};
use crate::schema_version::domain_from_record_id;

/// Options for the log linker.
#[derive(Debug, Clone, Default)]
pub struct LinkLogsOptions {
    /// Symmetric window tolerance in seconds for `temporal_correlation`
    /// (`>= 0`; default `0` = strict). A run window `[start, end]` matches a
    /// signature time `t` when `start - tolerance <= t <= end + tolerance`.
    pub tolerance_seconds: i64,
    /// Resolved commit view (a full SHA) recorded on emitted evidence links,
    /// mirroring `resolve-frames`. `None` for the current-state view.
    pub at_commit: Option<String>,
}

/// Per-signature link tally (deterministic; used in the CLI envelope).
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct SignatureLinkTally {
    /// Stable `ErrorSignature` record ID.
    pub signature_id: String,
    /// `content_hash_join` edges minted for this signature.
    pub content_hash_join: u64,
    /// `temporal_correlation` edges minted for this signature.
    pub temporal_correlation: u64,
    /// `REFERENCES_TASK` edges minted for this signature.
    pub task_links: u64,
    /// `true` when the signature received zero `EMITTED_DURING` edges.
    pub uncorrelated: bool,
}

/// Aggregate totals across all signatures.
#[derive(Debug, Clone, Eq, PartialEq, Serialize)]
pub struct LinkTotals {
    /// Number of `ErrorSignature` records considered.
    pub signatures: u64,
    /// Total `content_hash_join` edges minted.
    pub content_hash_join_edges: u64,
    /// Total `temporal_correlation` edges minted.
    pub temporal_correlation_edges: u64,
    /// Total `REFERENCES_TASK` edges minted.
    pub task_link_edges: u64,
    /// Signatures that received zero `EMITTED_DURING` edges.
    pub uncorrelated: u64,
    /// Signatures whose in-window temporal candidates were suppressed by the
    /// repository guard (foreign log, or no single repository anchor).
    pub cross_repo_rejected: u64,
    /// The tolerance in effect for this pass.
    pub tolerance_seconds: i64,
    /// Whether `temporal_correlation` was enabled (exactly one repository
    /// anchor). `content_hash_join` runs regardless.
    pub temporal_correlation_enabled: bool,
}

/// Result of a link pass.
#[derive(Debug, Clone)]
pub struct LinkLogsResult {
    /// Enriched log-domain records (`ErrorSignature` nodes carrying their new
    /// evidence links, the other log-domain input records unchanged) followed by
    /// the new `EMITTED_DURING` / `REFERENCES_TASK` edges, in canonical order.
    /// Non-log records (agent / verification / project targets) are never
    /// re-emitted — they are referenced by stable ID.
    pub records: Vec<GraphRecord>,
    /// Per-signature tallies, ordered by signature ID.
    pub signatures: Vec<SignatureLinkTally>,
    /// Aggregate totals.
    pub totals: LinkTotals,
}

/// One pending `EMITTED_DURING` edge before canonical assembly.
struct PendingRun {
    signature_id: String,
    target_id: String,
    basis: CorrelationBasis,
}

/// A parsed run/turn execution window.
struct RunWindow {
    run_id: String,
    start: DateTime<Utc>,
    end: DateTime<Utc>,
}

/// Links `ErrorSignature` records to the agent runs / commands that produced
/// them over the union graph `records`.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn link_logs(records: &[GraphRecord], opts: &LinkLogsOptions) -> LinkLogsResult {
    let tolerance = Duration::seconds(opts.tolerance_seconds.max(0));

    // ── Repository anchor: exactly one Repository node enables temporal. ──────
    let repo_anchors: Vec<&str> = records
        .iter()
        .filter_map(|r| match r {
            GraphRecord::Node {
                id,
                kind: NodeKind::Repository,
                ..
            } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    let anchor: Option<&str> = if repo_anchors.len() == 1 {
        Some(repo_anchors[0])
    } else {
        None
    };
    let temporal_enabled = anchor.is_some();

    // ── CommandRun output hashes → set of CommandRun IDs. ─────────────────────
    let mut cmd_by_hash: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    // ── Run/turn execution windows. ───────────────────────────────────────────
    let mut windows: Vec<RunWindow> = Vec::new();
    // ── LogSource id → (source_relative_path, source_artifact_hash). ──────────
    let mut log_sources: BTreeMap<&str, (&str, &str)> = BTreeMap::new();
    // ── Existing task edges: run/command node id → set of task node ids. ──────
    let mut task_targets: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    // ── Node kinds (for task-target kind checks). ─────────────────────────────
    let mut node_kind: BTreeMap<&str, NodeKind> = BTreeMap::new();

    for r in records {
        match r {
            GraphRecord::Node {
                id,
                kind,
                started_at,
                finished_at,
                observed_at,
                stdout_handle,
                stderr_handle,
                log,
                ..
            } => {
                node_kind.insert(id.as_str(), *kind);
                match kind {
                    NodeKind::CommandRun => {
                        for handle in [stdout_handle, stderr_handle].into_iter().flatten() {
                            let OutputHandle { hash, .. } = handle.as_ref();
                            if !hash.is_empty() {
                                cmd_by_hash.entry(hash.as_str()).or_default().insert(id);
                            }
                        }
                    }
                    NodeKind::AgentRun | NodeKind::AgentTurn => {
                        // A window needs a start (started_at, else observed_at)
                        // and an explicit finished_at; incomplete runs provide
                        // no window and are skipped.
                        let start_raw = started_at.as_deref().or(observed_at.as_deref());
                        if let (Some(s), Some(e)) = (start_raw, finished_at.as_deref())
                            && let (Some(start), Some(end)) = (parse_ts(s), parse_ts(e))
                            && start <= end
                        {
                            windows.push(RunWindow {
                                run_id: id.clone(),
                                start,
                                end,
                            });
                        }
                    }
                    NodeKind::LogSource => {
                        if let Some(LogPayload::LogSource(p)) = log.as_deref() {
                            log_sources.insert(
                                id.as_str(),
                                (
                                    p.source_relative_path.as_str(),
                                    p.source_artifact_hash.as_str(),
                                ),
                            );
                        }
                    }
                    _ => {}
                }
            }
            GraphRecord::Edge {
                label: EdgeLabel::ReferencesTask | EdgeLabel::OwnedByTask,
                source,
                target,
                ..
            } => {
                task_targets
                    .entry(source.as_str())
                    .or_default()
                    .insert(target.as_str());
            }
            _ => {}
        }
    }
    // Deterministic window order (start, end, run_id).
    windows.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then(a.end.cmp(&b.end))
            .then(a.run_id.cmp(&b.run_id))
    });

    // ── signature id → set of LogSource ids (CAPTURED_FROM). ──────────────────
    let mut captured_from: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for r in records {
        if let GraphRecord::Edge {
            label: EdgeLabel::CapturedFrom,
            source,
            target,
            ..
        } = r
            && node_kind.get(source.as_str()) == Some(&NodeKind::ErrorSignature)
        {
            captured_from
                .entry(source.as_str())
                .or_default()
                .insert(target.as_str());
        }
    }

    // ── Signatures, sorted by ID. ─────────────────────────────────────────────
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

    let mut pending: Vec<PendingRun> = Vec::new();
    // (signature_id, task_id) -> strongest basis reaching the task.
    let mut task_edges: BTreeMap<(String, String), CorrelationBasis> = BTreeMap::new();
    let mut tallies: Vec<SignatureLinkTally> = Vec::new();
    let mut totals = LinkTotals {
        signatures: 0,
        content_hash_join_edges: 0,
        temporal_correlation_edges: 0,
        task_link_edges: 0,
        uncorrelated: 0,
        cross_repo_rejected: 0,
        tolerance_seconds: opts.tolerance_seconds.max(0),
        temporal_correlation_enabled: temporal_enabled,
    };

    for sig_id in &signature_ids {
        totals.signatures += 1;
        let sources = captured_from.get(sig_id);

        // Repository eligibility (temporal only): at least one CAPTURED_FROM
        // LogSource recomputes to the single anchor.
        let repo_eligible = anchor.is_some_and(|a| {
            sources.is_some_and(|srcs| {
                srcs.iter().any(|src| {
                    log_sources.get(src).is_some_and(|(path, hash)| {
                        &log_stable_id(&["log_source", a, path, hash]) == src
                    })
                })
            })
        });

        let mut this_targets: Vec<(String, CorrelationBasis)> = Vec::new();

        // ── content_hash_join (inherently within-repo). ───────────────────────
        if let Some(srcs) = sources {
            let mut hash_targets: BTreeSet<&str> = BTreeSet::new();
            for src in srcs {
                if let Some((_, hash)) = log_sources.get(src)
                    && let Some(cmds) = cmd_by_hash.get(hash)
                {
                    for cmd in cmds {
                        hash_targets.insert(cmd);
                    }
                }
            }
            for cmd in hash_targets {
                this_targets.push((cmd.to_owned(), CorrelationBasis::ContentHashJoin));
            }
        }

        // ── temporal_correlation (repository-guarded). ────────────────────────
        let rep_time = signature_valid_time(records, sig_id);
        let mut in_window: BTreeSet<&str> = BTreeSet::new();
        if let Some(t) = rep_time {
            for w in &windows {
                if (w.start - tolerance) <= t && t <= (w.end + tolerance) {
                    in_window.insert(w.run_id.as_str());
                }
            }
        }
        if !in_window.is_empty() {
            if temporal_enabled && repo_eligible {
                for run in &in_window {
                    this_targets.push(((*run).to_owned(), CorrelationBasis::TemporalCorrelation));
                }
            } else {
                // Suppressed by the repository guard: report absence, never hide.
                totals.cross_repo_rejected += 1;
            }
        }

        // Deterministic per-signature target order.
        this_targets.sort();
        this_targets.dedup();

        let mut tally = SignatureLinkTally {
            signature_id: (*sig_id).to_owned(),
            content_hash_join: 0,
            temporal_correlation: 0,
            task_links: 0,
            uncorrelated: false,
        };

        for (target_id, basis) in &this_targets {
            match basis {
                CorrelationBasis::ContentHashJoin => {
                    tally.content_hash_join += 1;
                    totals.content_hash_join_edges += 1;
                }
                CorrelationBasis::TemporalCorrelation => {
                    tally.temporal_correlation += 1;
                    totals.temporal_correlation_edges += 1;
                }
            }
            pending.push(PendingRun {
                signature_id: (*sig_id).to_owned(),
                target_id: target_id.clone(),
                basis: *basis,
            });

            // Task linkage: propagate the run's existing task edges to the
            // signature, keeping the strongest basis per (signature, task).
            if let Some(tasks) = task_targets.get(target_id.as_str()) {
                for task in tasks {
                    if !is_task_kind(node_kind.get(task)) {
                        continue;
                    }
                    let key = ((*sig_id).to_owned(), (*task).to_owned());
                    task_edges
                        .entry(key)
                        .and_modify(|b| *b = stronger(*b, *basis))
                        .or_insert(*basis);
                }
            }
        }

        if this_targets.is_empty() {
            tally.uncorrelated = true;
            totals.uncorrelated += 1;
        }
        tallies.push(tally);
    }

    // Per-signature task-link counts + totals.
    let mut task_link_count: BTreeMap<&str, u64> = BTreeMap::new();
    for (sig, _task) in task_edges.keys() {
        *task_link_count.entry(sig.as_str()).or_default() += 1;
        totals.task_link_edges += 1;
    }
    for tally in &mut tallies {
        tally.task_links = task_link_count
            .get(tally.signature_id.as_str())
            .copied()
            .unwrap_or(0);
    }

    // ── Assemble edges + per-signature evidence links (dual representation). ──
    let mut links_by_sig: BTreeMap<String, Vec<EvidenceLink>> = BTreeMap::new();
    let mut edges: Vec<GraphRecord> = Vec::new();
    let mut edge_ids: BTreeSet<String> = BTreeSet::new();

    for p in &pending {
        let confidence = p.basis.confidence();
        let edge_id = log_stable_id(&[
            "edge",
            EdgeLabel::EmittedDuring.as_str(),
            &p.signature_id,
            &p.target_id,
            p.basis.as_str(),
        ]);
        if edge_ids.insert(edge_id.clone()) {
            edges.push(GraphRecord::Edge {
                id: edge_id,
                schema_version: LOG_SCHEMA_VERSION,
                label: EdgeLabel::EmittedDuring,
                source: p.signature_id.clone(),
                target: p.target_id.clone(),
                confidence: Some(confidence.to_owned()),
                resolution: None,
                frame_resolution: None,
                frame_index: None,
                basis: Some(p.basis),
                is_exhaustive: None,
                temporal: None,
                summary: format!(
                    "error signature {} emitted during {} ({})",
                    p.signature_id,
                    p.target_id,
                    p.basis.as_str()
                ),
                producer: None,
            });
        }
        links_by_sig
            .entry(p.signature_id.clone())
            .or_default()
            .push(EvidenceLink {
                target_record_id: Some(p.target_id.clone()),
                target_domain: domain_of(&p.target_id),
                relation: EdgeLabel::EmittedDuring.as_str().to_owned(),
                confidence: confidence.to_owned(),
                as_of_commit: opts.at_commit.clone(),
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            });
    }

    for ((sig_id, task_id), basis) in &task_edges {
        let confidence = basis.confidence();
        let edge_id = log_stable_id(&["edge", EdgeLabel::ReferencesTask.as_str(), sig_id, task_id]);
        if edge_ids.insert(edge_id.clone()) {
            edges.push(GraphRecord::Edge {
                id: edge_id,
                schema_version: LOG_SCHEMA_VERSION,
                label: EdgeLabel::ReferencesTask,
                source: sig_id.clone(),
                target: task_id.clone(),
                confidence: Some(confidence.to_owned()),
                resolution: None,
                frame_resolution: None,
                frame_index: None,
                basis: None,
                is_exhaustive: None,
                temporal: None,
                summary: format!("error signature {sig_id} references task {task_id}"),
                producer: None,
            });
        }
        links_by_sig
            .entry(sig_id.clone())
            .or_default()
            .push(EvidenceLink {
                target_record_id: Some(task_id.clone()),
                target_domain: domain_of(task_id),
                relation: EdgeLabel::ReferencesTask.as_str().to_owned(),
                confidence: confidence.to_owned(),
                as_of_commit: opts.at_commit.clone(),
                target_repo_relative_path: None,
                target_span: None,
                target_git_commit: None,
            });
    }

    // ── Emit log-domain records, enriching ErrorSignature nodes. ──────────────
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
    nodes.sort_by(|a, b| a.id().cmp(b.id()));

    edges.sort_by_key(edge_sort_key);

    let mut out = nodes;
    out.extend(edges);

    LinkLogsResult {
        records: out,
        signatures: tallies,
        totals,
    }
}

/// Returns the representative valid time of an `ErrorSignature`: `last_seen`,
/// falling back to `first_seen`, parsed as UTC. `None` when neither parses.
fn signature_valid_time(records: &[GraphRecord], signature_id: &str) -> Option<DateTime<Utc>> {
    for r in records {
        if r.id() == signature_id
            && let Some(LogPayload::ErrorSignature(p)) = r.log_payload()
        {
            return parse_ts(&p.last_seen).or_else(|| parse_ts(&p.first_seen));
        }
    }
    None
}

/// Parses an RFC 3339 timestamp to UTC. Redaction-safe: time bounds only.
fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Returns the stronger of two bases (`content_hash_join` > `temporal_correlation`).
const fn stronger(a: CorrelationBasis, b: CorrelationBasis) -> CorrelationBasis {
    match (a, b) {
        (CorrelationBasis::ContentHashJoin, _) | (_, CorrelationBasis::ContentHashJoin) => {
            CorrelationBasis::ContentHashJoin
        }
        _ => CorrelationBasis::TemporalCorrelation,
    }
}

/// True when a node kind is a task-like project target.
const fn is_task_kind(kind: Option<&NodeKind>) -> bool {
    matches!(
        kind,
        Some(NodeKind::Task | NodeKind::GitHubIssue | NodeKind::LocalTask)
    )
}

fn domain_of(id: &str) -> String {
    domain_from_record_id(id).unwrap_or_else(|| "unknown".to_owned())
}

fn edge_sort_key(edge: &GraphRecord) -> (String, String, String, String) {
    match edge {
        GraphRecord::Edge {
            source,
            target,
            label,
            id,
            ..
        } => (
            source.clone(),
            label.as_str().to_owned(),
            target.clone(),
            id.clone(),
        ),
        _ => (
            String::new(),
            String::new(),
            String::new(),
            edge.id().to_owned(),
        ),
    }
}

/// Sorts evidence links deterministically (relation, target ID).
fn sort_evidence_links(links: &mut [EvidenceLink]) {
    links.sort_by(|a, b| {
        a.relation
            .cmp(&b.relation)
            .then_with(|| a.target_record_id.cmp(&b.target_record_id))
    });
}

#[cfg(test)]
mod tests;
