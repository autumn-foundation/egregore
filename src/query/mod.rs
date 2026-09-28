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
mod author_scope;
mod belief_timeline;
mod change_impact;
mod changes;
mod churn;
mod clones;
mod conflicts;
mod context;
mod coupling;
mod cycles;
mod dead_code;
mod debt_markers;
mod deltas;
mod dep_usage;
mod deprecated_symbols;
mod deps;
mod diagnostics;
mod drift;
mod error_context;
mod evidence_path;
mod failure_history;
mod failure_hotspots;
mod file_at_point;
mod implementors;
mod lifeline;
pub(crate) mod liveness;
mod locate;
mod log_deltas;
mod memory_audit;
mod memory_collapse;
mod memory_decisions;
mod orientation;
mod ownership;
mod producer_drift;
mod public_api;
mod public_api_deltas;
mod recency;
mod redaction_audit;
mod repo;
mod risk_markers;
mod semantic;
mod subsystem;
mod symbols;
mod task_evidence;
// Appended (issue #161); kept at the end to minimize cross-lane merge conflicts.
mod task_ready;
mod transaction_time;
mod transitive_callees;
mod transitive_callers;
mod tx_delta;
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
// Appended (issue #227); kept at the end to minimize cross-lane merge conflicts.
mod lint_suppressions;
// Appended (issue #214); kept at the end to minimize cross-lane merge conflicts.
mod brief;
mod who_constructs;
// Appended (issue #112); kept at the end to minimize cross-lane merge conflicts.
mod sessions;
// Appended (issue #114); kept at the end to minimize cross-lane merge conflicts.
mod trust;
// Appended (issue #236); kept at the end to minimize cross-lane merge conflicts.
mod trust_audit;
// Appended (issue #228); kept at the end to minimize cross-lane merge conflicts.
mod neighborhood;
// Appended (issue #265); kept at the end to minimize cross-lane merge conflicts.
mod blind_spots;
// Appended (issue #262); kept at the end to minimize cross-lane merge conflicts.
mod track_record;
// Appended (issue #259); kept at the end to minimize cross-lane merge conflicts.
mod session_retrospective;
// Appended (issue #169); kept at the end to minimize cross-lane merge conflicts.
mod policy_context;
// Appended (issue #162); kept at the end to minimize cross-lane merge conflicts.
mod complexity;

pub use as_of::*;
pub use author_scope::*;
pub use belief_timeline::*;
pub use change_impact::*;
pub use changes::*;
pub use churn::*;
pub use clones::*;
pub use conflicts::*;
pub use context::*;
pub use coupling::*;
pub use cycles::*;
pub use dead_code::*;
pub use debt_markers::*;
pub use deltas::*;
pub use dep_usage::*;
pub use deprecated_symbols::*;
pub use deps::*;
pub use diagnostics::*;
pub use drift::*;
pub use error_context::*;
pub use evidence_path::*;
pub use failure_history::*;
pub use failure_hotspots::*;
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
pub use redaction_audit::*;
pub use repo::*;
pub use risk_markers::*;
pub use semantic::*;
pub use subsystem::*;
pub use symbols::*;
pub use task_evidence::*;
pub use transaction_time::*;
pub use transitive_callees::*;
pub use transitive_callers::*;
pub use tx_delta::*;
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
// Appended (issue #227); kept at the end to minimize cross-lane merge conflicts.
pub use lint_suppressions::*;
// Appended (issue #114); kept at the end to minimize cross-lane merge conflicts.
pub use trust::{TrustClass, TrustIndex};
// Appended (issue #265); kept at the end to minimize cross-lane merge conflicts.
pub use blind_spots::*;
// Appended (issue #262); kept at the end to minimize cross-lane merge conflicts.
pub use track_record::*;
// Appended (issue #259); kept at the end to minimize cross-lane merge conflicts.
pub use policy_context::*;
pub use session_retrospective::*;
// Appended (issue #214); kept at the end to minimize cross-lane merge conflicts.
pub use brief::*;
// The verification-outcome rule is shared with `crate::criteria_coverage` and
// the daemon write path, but it is not public API: re-exported crate-internally
// so the shared rule cannot fork, without widening `egregore::query`.
pub(crate) use trust::{
    CLOSURE_TARGET_KINDS, VERIFICATION_DOMAIN_KINDS, VerificationOutcome, has_evidence_handle,
    is_closure_target_kind, is_verification_domain_kind, is_verification_domain_record,
    verification_outcome,
};
// Appended (issue #236); kept at the end to minimize cross-lane merge conflicts.
pub use trust_audit::*;
// Appended (issue #228); kept at the end to minimize cross-lane merge conflicts.
pub use neighborhood::*;
// Appended (issue #163); kept at the end to minimize cross-lane merge conflicts.
pub use memory_collapse::*;
// Appended (issue #162); kept at the end to minimize cross-lane merge conflicts.
pub use complexity::*;
// Appended (issue #161); kept at the end to minimize cross-lane merge conflicts.
pub use task_ready::*;
