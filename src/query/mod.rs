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
mod failure_history;
mod file_at_point;
mod implementors;
mod lifeline;
mod locate;
mod log_deltas;
mod memory_audit;
mod memory_decisions;
mod orientation;
mod ownership;
mod producer_drift;
mod public_api;
mod public_api_deltas;
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
