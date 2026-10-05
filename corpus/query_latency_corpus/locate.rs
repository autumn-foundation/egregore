//! Positional entry into the evidence-backed context bundle (issue #212).
//!
//! `eg query locate <path>:<line>` resolves the innermost `Symbol` whose span
//! contains a `file:line` position (reusing the issue #151 span-containment
//! resolver, [`location_context`]) and then returns that symbol's
//! trust-separated cross-domain context — the *same* bundle as
//! `eg query context` — by anchoring [`record_context`] on the located record.
//!
//! `locate` is positional entry into the existing `context` contract, not a new
//! answer shape: an agent holding a stack-trace frame, blame line, or diff hunk
//! gets back source facts, agent observations, project state, artifacts, and
//! verification evidence without first guessing a symbol name.
//!
//! Absence is always a typed answer, never a nearest-neighbor guess: a line
//! outside every symbol span yields [`LocateOutcome::NoEnclosingSymbol`], a line
//! beyond the file's last recorded structural span yields
//! [`LocateOutcome::LineOutOfRange`], and an unknown path yields
//! [`LocateOutcome::NoMatch`]. The unscoped cross-repository collision fails
//! closed with [`LocateOutcome::AmbiguousRepository`] (issue #67).

use super::{RepositoryIndex, SymbolContext, location_context, record_context};
use crate::ir::GraphRecord;

/// The resolved outcome of an `eg query locate` position lookup.
///
/// Every non-success variant is a typed, deterministic answer — a positional
/// lookup never fabricates or approximates a symbol when the line falls outside
/// every recorded span.
#[derive(Debug)]
pub enum LocateOutcome<'a> {
    /// An innermost enclosing `Symbol` was found.
    Located {
        /// The innermost (smallest enclosing) `Symbol` node.
        primary: &'a GraphRecord,
        /// Containing `Module`/`Symbol` nodes, outermost → innermost; the last
        /// entry is always `primary`.
        chain: Vec<&'a GraphRecord>,
        /// The `File` node for the queried path in the selected view, if present.
        file_record: Option<&'a GraphRecord>,
        /// Owning repository ID of `primary`, when the store attributes it.
        repository_id: Option<&'a str>,
        /// The located symbol's trust-separated cross-domain context bundle,
        /// anchored on `primary` via [`record_context`]. This mirrors
        /// `eg query context` and is not temporally filtered even under a
        /// commit pin: the pin selects *which* symbol is located, the bundle is
        /// everything known about that symbol identity. Boxed to keep the
        /// success variant from dominating the enum's size.
        context: Box<SymbolContext<'a>>,
    },
    /// The path exists in the selected view but the line sits inside no symbol
    /// span (file-level `use`, comment, blank line, inter-item gap). Never a
    /// guessed neighbor.
    NoEnclosingSymbol {
        /// The resolved `File` node handle, when present.
        file_record: Option<&'a GraphRecord>,
    },
    /// The line is beyond the file's last recorded structural span — out of
    /// range of the graph's knowledge of the file.
    LineOutOfRange {
        /// The resolved `File` node handle, when present.
        file_record: Option<&'a GraphRecord>,
        /// The deepest recorded structural line for the path.
        max_known_line: usize,
    },
    /// No `File`/`Symbol`/`Module` record carries the path in the selected view.
    NoMatch,
    /// The unscoped query matched the path in more than one repository; a single
    /// positional answer never picks one implicitly (issue #67).
    AmbiguousRepository {
        /// The distinct owner groups that matched (`None` = unattributed rows).
        repositories: Vec<Option<&'a str>>,
    },
}

/// Resolves a `path:line` position to its innermost enclosing symbol and that
/// symbol's evidence-backed context bundle (issue #212).
///
/// `at_commit`, when supplied, must be a *fully resolved* commit SHA (the CLI
/// layer resolves `--at` prefixes and `--as-of` instants before calling this):
/// span containment is then evaluated against that commit's snapshot. Without
/// it, the current-state view applies (tombstone-excluded, HEAD snapshot for
/// history graphs), exactly as [`location_context`] documents.
///
/// This is a pure read over `records`; it never touches Git or the working tree.
#[must_use]
pub fn locate<'a>(
    records: &'a [GraphRecord],
    path: &str,
    line: usize,
    at_commit: Option<&str>,
    index: &'a RepositoryIndex,
    repo_scope: Option<&str>,
) -> LocateOutcome<'a> {
    let ctx = location_context(records, path, line, at_commit, index, repo_scope);

    // No record carries this path in the selected view: unknown handle.
    if ctx.repo_groups.is_empty() {
        return LocateOutcome::NoMatch;
    }

    // A single positional answer cannot represent two repositories: fail closed
    // on an unscoped collision instead of picking one implicitly (issue #67).
    if repo_scope.is_none() && ctx.repo_groups.len() > 1 {
        return LocateOutcome::AmbiguousRepository {
            repositories: ctx.repo_groups.into_iter().collect(),
        };
    }

    let Some(primary) = ctx.primary else {
        // The line sits outside every recorded symbol span. Distinguish a line
        // beyond the file's last recorded structural element (out of range)
        // from a gap between items — but never guess a neighbor either way.
        if let Some(max) = ctx.max_span_end_line
            && line > max
        {
            return LocateOutcome::LineOutOfRange {
                file_record: ctx.file_record,
                max_known_line: max,
            };
        }
        return LocateOutcome::NoEnclosingSymbol {
            file_record: ctx.file_record,
        };
    };

    // Positional entry into the existing context contract: anchor the full
    // trust-separated bundle on the located record ID so the exact located
    // symbol (not every same-named symbol) drives the answer.
    let context = record_context(records, primary.id());

    LocateOutcome::Located {
        primary,
        chain: ctx.chain,
        file_record: ctx.file_record,
        repository_id: index.owner_of(primary.id()),
        context: Box::new(context),
    }
}
