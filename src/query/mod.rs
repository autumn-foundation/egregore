//! Agent-facing graph query helpers.
#![allow(
    clippy::uninlined_format_args,
    clippy::manual_let_else,
    clippy::collapsible_if,
    clippy::match_like_matches_macro,
    clippy::too_many_lines,
    clippy::doc_markdown,
    clippy::cast_precision_loss
)]

// Module list is alphabetized; each query lane lives in its own file.
mod as_of;
mod change_impact;
mod changes;
mod churn;
mod context;
mod coupling;
mod cycles;
mod debt_markers;
mod deltas;
mod deps;
mod drift;
mod error_context;
mod evidence_path;
mod failure_history;
mod file_at_point;
mod implementors;
mod lifeline;
pub(crate) mod liveness;
mod locate;
mod log_deltas;
mod memory_audit;
mod memory_decisions;
mod orientation;
mod ownership;
mod producer_drift;
mod public_api;
mod public_api_deltas;
mod recency;
mod repo;
mod semantic;
mod subsystem;
mod symbols;
mod task_evidence;
mod transaction_time;
mod transitive_callees;
mod transitive_callers;
mod undocumented;
mod unreferenced;
mod unsafe_sites;
mod unwrap_expect;
mod verification_coverage;
mod who;
// Appended (issue #225); kept at the end to minimize cross-lane merge conflicts.
mod path;
// Appended (issue #444); kept at the end to minimize cross-lane merge conflicts.
mod who_imports;
// Appended (issue #471); kept at the end to minimize cross-lane merge conflicts.
mod who_constructs;
// Appended (issue #112); kept at the end to minimize cross-lane merge conflicts.
mod sessions;
// Appended (issue #114); kept at the end to minimize cross-lane merge conflicts.
mod trust;

pub use as_of::*;
pub use change_impact::*;
pub use changes::*;
pub use churn::*;
pub use context::*;
pub use coupling::*;
pub use cycles::*;
pub use debt_markers::*;
pub use deltas::*;
pub use deps::*;
pub use drift::*;
pub use error_context::*;
pub use evidence_path::*;
pub use failure_history::*;
pub use file_at_point::*;
pub use implementors::*;
pub use lifeline::*;
pub use locate::*;
pub use log_deltas::*;
pub use memory_audit::*;
pub use memory_decisions::*;
pub use orientation::*;
pub use ownership::*;
pub use producer_drift::*;
pub use public_api::*;
pub use public_api_deltas::*;
pub use recency::*;
pub use repo::*;
pub use semantic::*;
pub use subsystem::*;
pub use symbols::*;
pub use task_evidence::*;
pub use transaction_time::*;
pub use transitive_callees::*;
pub use transitive_callers::*;
pub use undocumented::*;
pub use unreferenced::*;
pub use unsafe_sites::*;
pub use unwrap_expect::*;
pub use verification_coverage::*;
pub use who::*;
// Appended (issue #225); kept at the end to minimize cross-lane merge conflicts.
pub use path::*;
// Appended (issue #444); kept at the end to minimize cross-lane merge conflicts.
pub use who_imports::*;
// Appended (issue #471); kept at the end to minimize cross-lane merge conflicts.
pub use who_constructs::*;
// Appended (issue #112); kept at the end to minimize cross-lane merge conflicts.
pub use sessions::*;
// Appended (issue #114); kept at the end to minimize cross-lane merge conflicts.
pub use trust::{TrustClass, TrustIndex};
// The verification-outcome rule is shared with `crate::criteria_coverage` and
// the daemon write path, but it is not public API: re-exported crate-internally
// so the shared rule cannot fork, without widening `egregore::query`.
pub(crate) use trust::{
    VERIFICATION_DOMAIN_KINDS, VerificationOutcome, is_verification_domain_kind,
    is_verification_domain_record, verification_outcome,
};
