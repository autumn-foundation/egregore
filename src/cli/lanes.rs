//! Machine-readable query-lane capability manifest (issue #251).
//!
//! `eg query lanes` emits a deterministic JSON catalog of every public query
//! lane: stable lane id, one-line purpose, required inputs, required store
//! mode, answer trust class(es), whether answers carry citable record
//! handles, and freshness/staleness semantics.
//!
//! The catalog is derived from the actual clap command surface, not
//! hand-maintained prose:
//!
//! - lane ids are [`QuerySubcommand`]'s clap subcommand names
//!   (`get_name`, kebab-case);
//! - purposes are the first line of each variant's doc comment (clap
//!   `about`);
//! - required inputs are clap's required-argument metadata, plus the store
//!   selector every lane's loader enforces (`--graph | --data-dir`, or
//!   `--data-dir` for the embedding lanes, which accept no `--graph`).
//!
//! Only the per-lane *classification* (store mode, trust classes, citation
//! flag, freshness) is a static table ([`LANE_TABLE`]) — and the drift tests
//! (unit tests below plus `tests/integration/lane_manifest.rs`) fail CI when
//! a lane is added without a table entry (orphan) or a table entry names a
//! lane that no longer exists (phantom).
//!
//! The command is local and pure: it reads clap metadata only — no network,
//! no store — so it works on a fresh clone with an empty data dir.

use clap::Subcommand;
use std::fmt::Write as _;

use super::*;

/// Required store mode for a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum StoreMode {
    /// Structural store: a JSONL graph or an embedded store. No embedding
    /// model and no `--embed` ingest required.
    Structural,
    /// Semantic store: an embedded store ingested with `--embed`. The lane
    /// needs a vector index and refuses to answer when the query embedder's
    /// identity does not match the recorded one.
    Semantic,
}

/// Closed freshness vocabulary, in canonical order.
///
/// - `ingest_snapshot`: answers reflect the store as ingested/scanned;
///   staleness is bounded by the last ingest, never checked live.
/// - `valid_time`: answers are valid-time facts over recorded history
///   (commit/instant pins via `--at`/`--as-of`, or lanes that rank over
///   history); staleness is measured against the pinned commit/instant,
///   never wall-clock now.
/// - `working_tree`: the lane can check answers against the live working
///   tree and reports per-answer freshness codes (`--repo-path`).
/// - `store_live`: answers reflect the live agent-memory/project store;
///   staleness is a property of how recently records were written, not of a
///   scan.
#[cfg(test)]
const FRESHNESS_ORDER: &[&str] = &[
    "ingest_snapshot",
    "valid_time",
    "working_tree",
    "store_live",
];

/// Static per-lane classification: claims about the lane's answers, grounded
/// in the lane's docs and the closed [`crate::query::TrustClass`]
/// vocabulary (trust classes are listed in `TrustClass::ALL` doc order).
#[derive(Debug, Clone, Copy)]
struct LaneStatic {
    /// Required store mode.
    store_mode: StoreMode,
    /// Trust classes the lane's answers can carry.
    trust_classes: &'static [&'static str],
    /// Whether answers carry citable stable record handles.
    citable_handles: bool,
    /// Freshness/staleness semantics, in [`FRESHNESS_ORDER`].
    freshness: &'static [&'static str],
}

impl LaneStatic {
    /// Placeholder for a lane with no table entry. The drift tests reject
    /// every entry built from this, so an unlisted lane fails CI instead of
    /// shipping an empty contract.
    const fn unclassified() -> Self {
        Self {
            store_mode: StoreMode::Structural,
            trust_classes: &[],
            citable_handles: false,
            freshness: &[],
        }
    }
}

/// Per-lane classification table, keyed by stable clap lane id
/// (alphabetical). See the module docs for the anti-drift contract.
const LANE_TABLE: &[(&str, LaneStatic)] = &[
    (
        "at",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "audit",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "other",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "bench",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived", "verification_evidence"],
            citable_handles: true,
            freshness: &["valid_time"],
        },
    ),
    (
        "blind-spots",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "belief-timeline",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["agent_verified", "agent_unverified", "agent_contradicted"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "candidates",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["other"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "change-impact",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "changes",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "churn",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "clones",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "complexity",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "conflicts",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "verification_evidence",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "other",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "diagnostics",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "context",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "working_tree"],
        },
    ),
    (
        "coupling",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "cycles",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "dead-code",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "debt-markers",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "deltas",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "deprecated-symbols",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "deps",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "drift",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "error-context",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "verification_evidence",
                "runtime_observation",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "evidence-freshness",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "evidence-path",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
                "runtime_observation",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "failure-hotspots",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "failures",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "verification_evidence",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "file",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "working_tree"],
        },
    ),
    (
        "implementors",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "lifeline",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "locate",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "log-deltas",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived", "runtime_observation"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "manifest-deps",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "memory",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "verification_evidence",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "orient",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "origin",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived", "project_state"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "ownership",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "path",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "policy",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["other"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "producer-drift",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "public-api",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "public-api-deltas",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "recency",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "resolve",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "retirement",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["agent_verified", "agent_unverified", "agent_contradicted"],
            citable_handles: true,
            freshness: &["store_live", "valid_time"],
        },
    ),
    (
        "risk-markers",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "redaction-audit",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "verification_evidence",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "runtime_observation",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    #[cfg(feature = "embeddings")]
    (
        "semantic",
        LaneStatic {
            store_mode: StoreMode::Semantic,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    #[cfg(feature = "embeddings")]
    (
        "semantic-context",
        LaneStatic {
            store_mode: StoreMode::Semantic,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    #[cfg(feature = "embeddings")]
    (
        "semantic-memory",
        LaneStatic {
            store_mode: StoreMode::Semantic,
            trust_classes: &["agent_verified", "agent_unverified", "agent_contradicted"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "session",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "sessions",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["agent_unverified", "project_state"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "subsystem",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "symbol",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time", "working_tree"],
        },
    ),
    (
        "symbols",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "suppressions",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "task",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "verification_evidence",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "task-ready",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["project_state"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "track-record",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "verification_evidence",
                "other",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "trust-audit",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &[
                "source_derived",
                "verification_evidence",
                "agent_verified",
                "agent_unverified",
                "agent_contradicted",
                "project_state",
                "artifact",
                "runtime_observation",
            ],
            citable_handles: true,
            freshness: &["ingest_snapshot", "store_live"],
        },
    ),
    (
        "diagram",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "transitive-callers",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "transitive-callees",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "undocumented",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "unreferenced",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "unsafe-sites",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "unwrap-expect",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "uses",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time"],
        },
    ),
    (
        "verification-coverage",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived", "verification_evidence"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "verification-freshness",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived", "verification_evidence"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "working_tree"],
        },
    ),
    (
        "who",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot", "valid_time", "working_tree"],
        },
    ),
    (
        "who-constructs",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
    (
        "who-imports",
        LaneStatic {
            store_mode: StoreMode::Structural,
            trust_classes: &["source_derived"],
            citable_handles: true,
            freshness: &["ingest_snapshot"],
        },
    ),
];

/// One manifest entry: the machine-readable contract of a query lane.
#[derive(Debug, Clone, serde::Serialize)]
struct LaneEntry {
    /// Stable lane id: the `eg query <id>` subcommand name.
    id: String,
    /// One-line purpose: the first line of the variant's doc comment.
    purpose: String,
    /// Required inputs: clap-required arguments (`<positional>` /
    /// `--flag`), plus the store selector the loader enforces.
    required_inputs: Vec<String>,
    /// Required store mode.
    store_mode: StoreMode,
    /// Answer trust class(es), closed `TrustClass` vocabulary.
    trust_classes: Vec<String>,
    /// Whether answers carry citable stable record handles.
    citable_handles: bool,
    /// Freshness/staleness semantics, closed vocabulary.
    freshness: Vec<String>,
}

/// The manifest document.
#[derive(Debug, Clone, serde::Serialize)]
struct LaneManifest {
    /// Lanes in canonical (id-sorted) order.
    lanes: Vec<LaneEntry>,
}

/// First non-empty line of a subcommand's `about` text (the variant's doc
/// comment, first line). Falls back to the empty string, which the drift
/// tests reject.
fn first_doc_line(cmd: &clap::Command) -> String {
    cmd.get_about()
        .map(ToString::to_string)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// Required inputs for a lane, derived from clap metadata:
///
/// - every required argument: `<positional>` or `--flag`, sorted;
/// - then the store selector the query loader enforces: every lane reads
///   from exactly one store source, so `--graph | --data-dir` (or
///   `--data-dir` for the embedding lanes, which accept no `--graph`).
fn required_inputs(cmd: &clap::Command) -> Vec<String> {
    let mut inputs: Vec<String> = cmd
        .get_arguments()
        .filter(|arg| arg.is_required_set())
        .filter(|arg| arg.get_id() != "help" && arg.get_id() != "version")
        .map(|arg| {
            arg.get_long()
                .map_or_else(|| format!("<{}>", arg.get_id()), |long| format!("--{long}"))
        })
        .collect();
    inputs.sort();
    let has_graph = cmd.get_arguments().any(|arg| arg.get_id() == "graph");
    let has_data_dir = cmd.get_arguments().any(|arg| arg.get_id() == "data_dir");
    let store = match (has_graph, has_data_dir) {
        (true, true) => Some("--graph | --data-dir"),
        (true, false) => Some("--graph"),
        (false, true) => Some("--data-dir"),
        (false, false) => None,
    };
    if let Some(store) = store {
        inputs.push(store.to_owned());
    }
    inputs
}

/// Static classification for a lane id, or the unclassified placeholder the
/// drift tests reject.
fn classify(id: &str) -> LaneStatic {
    LANE_TABLE
        .iter()
        .find(|(key, _)| *key == id)
        .map_or_else(LaneStatic::unclassified, |(_, meta)| *meta)
}

/// The `query` command with all subcommands augmented, for introspection.
/// `QuerySubcommand` derives clap's `Subcommand` (not `CommandFactory`), so
/// the surface is built via `augment_subcommands`.
fn query_subcommand() -> clap::Command {
    <QuerySubcommand as Subcommand>::augment_subcommands(clap::Command::new("query"))
}

/// Build the manifest from the live clap command surface. Pure: reads
/// command metadata only.
fn build_manifest() -> LaneManifest {
    let mut lanes: Vec<LaneEntry> = query_subcommand()
        .get_subcommands()
        .filter(|sub| sub.get_name() != "lanes")
        .map(|sub| {
            let id = sub.get_name().to_owned();
            let meta = classify(&id);
            LaneEntry {
                purpose: first_doc_line(sub),
                required_inputs: required_inputs(sub),
                store_mode: meta.store_mode,
                trust_classes: meta.trust_classes.iter().map(ToString::to_string).collect(),
                citable_handles: meta.citable_handles,
                freshness: meta.freshness.iter().map(ToString::to_string).collect(),
                id,
            }
        })
        .collect();
    lanes.sort_by(|a, b| a.id.cmp(&b.id));
    LaneManifest { lanes }
}

/// Serialize the manifest deterministically (field order is declaration
/// order; lanes are id-sorted).
fn render_json() -> Result<String> {
    let manifest = build_manifest();
    serde_json::to_string_pretty(&manifest)
        .map_err(|err| anyhow::anyhow!("failed to serialize lane manifest: {err}"))
}

/// Human-skimmable table rendering of the manifest.
fn render_text() -> String {
    const PURPOSE_WIDTH: usize = 56;
    let manifest = build_manifest();
    let lane_w = manifest
        .lanes
        .iter()
        .map(|lane| lane.id.len())
        .max()
        .unwrap_or(4)
        .max("LANE".len());
    let mut out = String::new();
    // `write!` to a `String` is infallible; the result is ignored.
    let _ = writeln!(
        out,
        "{:<lane_w$}  {:<PURPOSE_WIDTH$}  {:<10}  {:<22}  {:<7}  FRESHNESS",
        "LANE",
        "PURPOSE",
        "STORE",
        "TRUST",
        "CITABLE",
        lane_w = lane_w
    );
    for lane in &manifest.lanes {
        let purpose = truncate(&lane.purpose, PURPOSE_WIDTH);
        let store = match lane.store_mode {
            StoreMode::Structural => "structural",
            StoreMode::Semantic => "semantic",
        };
        let trust = lane.trust_classes.join(",");
        let citable = if lane.citable_handles { "yes" } else { "no" };
        let freshness = lane.freshness.join(",");
        // `write!` to a `String` is infallible; the result is ignored.
        let _ = writeln!(
            out,
            "{:<lane_w$}  {:<PURPOSE_WIDTH$}  {:<10}  {:<22}  {:<7}  {freshness}",
            lane.id,
            purpose,
            store,
            trust,
            citable,
            lane_w = lane_w
        );
    }
    out
}

/// Truncate to `width` characters, marking truncation with `…`.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Run `eg query lanes`: emit the capability manifest.
pub(crate) fn query_lanes_cmd(format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => {
            println!("{}", render_json()?);
        }
        OutputFormat::Text => {
            print!("{}", render_text());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Run a closure on a thread with a 16 MiB stack. Debug builds of clap's
    /// `augment_subcommands` for the 47-variant `QuerySubcommand` use several
    /// MB of stack, overflowing the default 2 MiB test-thread stack.
    fn with_big_stack<T>(f: impl FnOnce() -> T + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        std::thread::Builder::new()
            .name("lanes-big-stack".to_owned())
            .stack_size(16 * 1024 * 1024)
            .spawn(f)
            .expect("failed to spawn big-stack thread")
            .join()
            .expect("big-stack thread panicked")
    }

    /// Clap-derived lane ids (the actual command surface, minus `lanes`).
    fn clap_lane_ids() -> BTreeSet<String> {
        with_big_stack(|| {
            query_subcommand()
                .get_subcommands()
                .filter(|sub| sub.get_name() != "lanes")
                .map(|sub| sub.get_name().to_owned())
                .collect()
        })
    }

    #[test]
    fn table_covers_clap_surface_exactly() {
        let table_ids: BTreeSet<&str> = LANE_TABLE.iter().map(|(key, _)| *key).collect();
        let clap_ids = clap_lane_ids();
        for id in &clap_ids {
            assert!(
                table_ids.contains(id.as_str()),
                "orphan lane {id}: implemented but unlisted — add a LANE_TABLE entry"
            );
        }
        for id in &table_ids {
            assert!(
                clap_ids.contains(*id),
                "phantom entry {id}: listed but unimplemented — remove it from LANE_TABLE"
            );
        }
    }

    #[test]
    fn manifest_is_byte_stable() {
        let json = || with_big_stack(|| render_json().expect("manifest should serialize"));
        assert_eq!(
            json(),
            json(),
            "manifest JSON must be byte-for-byte stable across runs"
        );
        assert_eq!(with_big_stack(render_text), with_big_stack(render_text));
    }

    #[test]
    fn every_entry_is_populated() {
        let trust_vocab: BTreeSet<&str> = [
            "source_derived",
            "verification_evidence",
            "agent_verified",
            "agent_unverified",
            "agent_contradicted",
            "project_state",
            "artifact",
            "runtime_observation",
            "other",
        ]
        .into_iter()
        .collect();
        let freshness_vocab: BTreeSet<&str> = FRESHNESS_ORDER.iter().copied().collect();
        for lane in &with_big_stack(build_manifest).lanes {
            assert!(!lane.purpose.is_empty(), "lane {} has no purpose", lane.id);
            assert!(
                !lane.purpose.contains('\n'),
                "lane {} purpose must be one line",
                lane.id
            );
            assert!(
                !lane.required_inputs.is_empty(),
                "lane {} has no required inputs",
                lane.id
            );
            assert!(
                !lane.trust_classes.is_empty(),
                "lane {} has no trust classes",
                lane.id
            );
            for class in &lane.trust_classes {
                assert!(
                    trust_vocab.contains(class.as_str()),
                    "lane {} has unknown trust class {class}",
                    lane.id
                );
            }
            assert!(
                lane.trust_classes.iter().collect::<BTreeSet<_>>().len()
                    == lane.trust_classes.len(),
                "lane {} has duplicate trust classes",
                lane.id
            );
            assert!(
                !lane.freshness.is_empty(),
                "lane {} has no freshness semantics",
                lane.id
            );
            for value in &lane.freshness {
                assert!(
                    freshness_vocab.contains(value.as_str()),
                    "lane {} has unknown freshness value {value}",
                    lane.id
                );
            }
        }
    }

    #[test]
    fn semantic_lanes_are_exactly_the_embedding_lanes() {
        let semantic: BTreeSet<String> = with_big_stack(build_manifest)
            .lanes
            .iter()
            .filter(|lane| lane.store_mode == StoreMode::Semantic)
            .map(|lane| lane.id.clone())
            .collect();
        let mut expected = BTreeSet::new();
        #[cfg(feature = "embeddings")]
        {
            expected.insert("semantic".to_owned());
            expected.insert("semantic-context".to_owned());
            expected.insert("semantic-memory".to_owned());
        }
        assert_eq!(
            semantic, expected,
            "semantic lanes are exactly the feature-gated embedding lanes"
        );
        for lane in &with_big_stack(build_manifest).lanes {
            if lane.store_mode == StoreMode::Semantic {
                assert!(
                    lane.required_inputs
                        .iter()
                        .any(|i| i.contains("--data-dir")),
                    "semantic lane {} must require --data-dir",
                    lane.id
                );
            }
        }
    }

    #[test]
    fn purposes_come_from_variant_doc_comments() {
        let manifest = with_big_stack(build_manifest);
        let by_id: std::collections::HashMap<&str, &LaneEntry> = manifest
            .lanes
            .iter()
            .map(|lane| (lane.id.as_str(), lane))
            .collect();
        assert_eq!(
            by_id["symbol"].purpose, "Find symbol nodes by name",
            "purpose must be the variant doc comment's first line (clap strips the trailing period)"
        );
        assert!(
            by_id["symbol"]
                .required_inputs
                .contains(&"<name>".to_owned()),
            "symbol must require its <name> positional"
        );
    }
}
