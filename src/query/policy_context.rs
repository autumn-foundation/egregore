//! Folds active approved user-context policy into cross-domain context answers.
//!
//! Issue #169. The durable policy records (`Preference`, `WorkflowRule`,
//! `NamingDecision`, `Constraint`) that [`active_policy`] surfaces are matched
//! against a scope auto-derived from the context anchor's own facts — the
//! owning repository, the repo-relative path, and the language — with no
//! caller-supplied policy filter. Records whose scope does not apply to the
//! anchor are excluded, as are pending/rejected/deferred candidates,
//! superseded or revoked records, and any record whose audit trail does not
//! validate (those are already excluded by [`active_policy`] itself).
//!
//! The policy rows folded here carry the approval-decision handle that
//! materialized them, so the authorization basis is always one hop away via
//! the decision record's prompt surface, chain, and supporting evidence.
//! Under the #114 closed trust vocabulary (`crate::query::TrustClass`) these
//! rows classify as [`TrustClass::Other`][crate::query::TrustClass]:
//! "other" is the vocabulary's home for records that are neither code facts
//! nor observations, and the authorization basis is the cited approval
//! decision (whose audit trail [`active_policy`] verified) rather than a
//! truth-bearing trust label.

use crate::ir::{GraphRecord, NodeKind, UserContextScope};

use super::RepositoryIndex;
use super::active_policy;

/// Matches a policy scope `path_glob` against a concrete repo-relative path.
///
/// Supported syntax:
/// - `*` matches any sequence of characters *within* one path segment
///   (it never crosses `/`);
/// - `**` matches any sequence of characters *across* segments, including `/`;
/// - `?` matches exactly one character that is not `/`;
/// - anything else matches literally.
///
/// The match is always segment-aware, so `src/policy/*` can never bleed into
/// a sibling prefix like `src/policy2/`.
#[must_use]
pub fn path_glob_matches(pattern: &str, path: &str) -> bool {
    fn go(pattern: &[u8], path: &[u8]) -> bool {
        if pattern.is_empty() {
            return path.is_empty();
        }
        // `**` first: it subsumes the single-`*` arm below.
        if pattern.starts_with(b"**") {
            // `**` with no following pattern consumes everything.
            if pattern.len() == 2 {
                return true;
            }
            // Collapse `**/` + explicit-segment edge cases into the general
            // recursion: try every split point, including the empty one.
            for i in 0..=path.len() {
                if go(&pattern[2..], &path[i..]) {
                    return true;
                }
            }
            return false;
        }
        if pattern[0] == b'*' {
            // Single `*`: consume up to (but not including) the next `/`.
            for i in 0..=path.len() {
                if i < path.len() && path[i] == b'/' {
                    break;
                }
                if go(&pattern[1..], &path[i..]) {
                    return true;
                }
            }
            return false;
        }
        if path.is_empty() {
            return false;
        }
        if pattern[0] == b'?' {
            return path[0] != b'/' && go(&pattern[1..], &path[1..]);
        }
        pattern[0] == path[0] && go(&pattern[1..], &path[1..])
    }
    go(pattern.as_bytes(), path.as_bytes())
}

/// Derives the policy-applicability scope for a context anchor record from the
/// anchor's own code facts — never from a caller-supplied filter.
///
/// The returned scope carries, when the anchor records them:
/// - `repo`: the owning repository record ID, resolved through the
///   containment topology (`RepositoryIndex::owner_of`). The scope matcher
///   compares this with exact string equality against the policy record's
///   `repo` dimension, so a policy scope must name the repository record ID
///   itself — display or selector aliases are not resolved;
/// - `path_glob`: the anchor's concrete repo-relative path. Module or Cargo
///   package containment is implicit in the path hierarchy itself, and the
///   record-scope matcher interprets the policy's glob against this path;
/// - `language`: the anchor's language, when recorded;
/// - `lifecycle_phase`: always `None` — code-graph facts carry no lifecycle
///   phase, so lifecycle-scoped records never match a symbol/file anchor.
///
/// Anchors that are not code-graph nodes (symbols, files, modules) yield an
/// all-`None` scope, which [`policy_scope_applies`] rejects for every record
/// scope that constrains anything.
#[must_use]
pub fn policy_scope_for_record(records: &[GraphRecord], anchor: &GraphRecord) -> UserContextScope {
    let index = RepositoryIndex::build(records);
    let GraphRecord::Node {
        id,
        repo_relative_path,
        language,
        ..
    } = anchor
    else {
        return UserContextScope {
            repo: None,
            path_glob: None,
            language: None,
            lifecycle_phase: None,
        };
    };
    UserContextScope {
        repo: index.owner_of(id).map(str::to_owned),
        path_glob: repo_relative_path.clone(),
        language: language.clone(),
        lifecycle_phase: None,
    }
}

/// Reports whether a policy record's scope applies to a derived target scope.
///
/// A record scope dimension applies when it *constrains and matches* the
/// target dimension; a dimension the record scope leaves unset applies to
/// every target. Concretely:
/// - `repo`: both record and target name a repository; the record must equal
///   the target exactly. A record scope with `repo` set never applies to a
///   target whose owning repository is unknown (`None`);
/// - `path_glob`: the pattern is interpreted against the target's concrete
///   path via [`path_glob_matches`];
/// - `language` / `lifecycle_phase`: exact equality when both are set.
///
/// An all-unset record scope applies to everything; an all-unset target scope
/// applies to nothing that constrains anything.
#[must_use]
pub fn policy_scope_applies(record_scope: &UserContextScope, target: &UserContextScope) -> bool {
    match (&record_scope.repo, &target.repo) {
        (Some(want), Some(have)) if want != have => return false,
        (Some(_), None) => return false,
        _ => {}
    }
    if let Some(pattern) = &record_scope.path_glob {
        let Some(path) = target.path_glob.as_deref() else {
            return false;
        };
        if !path_glob_matches(pattern, path) {
            return false;
        }
    }
    match (&record_scope.language, &target.language) {
        (Some(want), Some(have)) if want != have => return false,
        (Some(_), None) => return false,
        _ => {}
    }
    match (&record_scope.lifecycle_phase, &target.lifecycle_phase) {
        (Some(want), Some(have)) if want != have => return false,
        (Some(_), None) => return false,
        _ => {}
    }
    true
}

/// Supersession/revocation status of a policy row folded into context.
///
/// [`active_policy`] already excludes superseded and revoked records, so a
/// context fold only ever surfaces [`PolicyStatus::Active`]; the enum exists
/// so consumers can render the status column explicitly and so future
/// opt-in "include superseded" surfaces share the type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyStatus {
    /// The record is the current active policy (no `active_to`).
    Active,
}

impl PolicyStatus {
    /// Machine-stable status label for rendering.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
        }
    }
}

/// One active approved policy record folded into a context answer.
///
/// The record is borrowed from the store; the row additionally carries the
/// audit-chain-proven authorization metadata a caller needs without
/// re-running the audit trail: the approval-decision handle, the activation
/// timestamp, the record's own scope, and the human-readable rule body.
#[derive(Debug, Clone, Copy)]
pub struct PolicyEntry<'a> {
    /// The underlying durable policy record.
    pub record: &'a GraphRecord,
    /// Durable policy kind: `Preference`, `WorkflowRule`, `NamingDecision`,
    /// or `Constraint`.
    pub kind: NodeKind,
    /// Human-readable rule body: `rule_text` for preferences and workflow
    /// rules, `constraint_text` for constraints, `canonical_name` for naming
    /// decisions.
    pub body: &'a str,
    /// ID of the `PromotionDecision` that approved this record.
    pub approval_decision_id: &'a str,
    /// Activation timestamp: equals the approving decision's `decided_at`.
    pub active_from: &'a str,
    /// The scope this record was approved under.
    pub scope: &'a UserContextScope,
    /// Supersession/revocation status; always [`PolicyStatus::Active`] here.
    pub status: PolicyStatus,
}

impl<'a> PolicyEntry<'a> {
    /// The durable policy record's ID.
    #[must_use]
    pub fn record_id(&self) -> &'a str {
        self.record.id()
    }
}

/// Folds the active approved policy records whose scope applies to `anchor`
/// into deterministic rows, ready for a context answer's `policy` section.
///
/// Inclusion rules (mirroring the acceptance criteria):
/// - only records [`active_policy`] returns — durable kinds with a validated
///   audit chain, excluding anything with `active_to` (superseded/revoked),
///   pending/rejected/deferred candidates, and malformed chains;
/// - only records whose [`UserContextScope`] [`policy_scope_applies`] to the
///   scope [`policy_scope_for_record`] derives from `anchor`'s own facts —
///   no caller-supplied policy filter is consulted.
///
/// Rows are returned sorted by record ID for deterministic rendering.
#[must_use]
pub fn policy_for_anchor<'a>(
    records: &'a [GraphRecord],
    anchor: &'a GraphRecord,
) -> Vec<PolicyEntry<'a>> {
    let target = policy_scope_for_record(records, anchor);
    let mut out = Vec::new();
    for rec in active_policy(records, None) {
        let GraphRecord::Node {
            kind,
            user_context,
            superseded_by,
            ..
        } = rec
        else {
            continue;
        };
        // Belt-and-braces supersession defense: `active_policy` excludes
        // `active_to`, but a record superseded without an `active_to` stamp
        // must not surface either.
        if superseded_by.is_some() {
            continue;
        }
        let Some(scope) = &user_context.scope else {
            continue;
        };
        if !policy_scope_applies(scope, &target) {
            continue;
        }
        let body = match kind {
            NodeKind::Preference | NodeKind::WorkflowRule => user_context.rule_text.as_deref(),
            NodeKind::Constraint => user_context.constraint_text.as_deref(),
            NodeKind::NamingDecision => user_context.canonical_name.as_deref(),
            _ => None,
        };
        let (Some(body), Some(approval_decision_id), Some(active_from)) = (
            body,
            user_context.approval_decision_id.as_deref(),
            user_context.active_from.as_deref(),
        ) else {
            continue;
        };
        out.push(PolicyEntry {
            record: rec,
            kind: *kind,
            body,
            approval_decision_id,
            active_from,
            scope,
            status: PolicyStatus::Active,
        });
    }
    out.sort_by(|a: &PolicyEntry<'_>, b: &PolicyEntry<'_>| a.record_id().cmp(b.record_id()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matcher_unit_cases() {
        assert!(path_glob_matches("src/policy/*", "src/policy/mod.rs"));
        assert!(!path_glob_matches("src/policy/*", "src/policy/sub/x.rs"));
        assert!(!path_glob_matches("src/policy/*", "src/policy"));
        assert!(!path_glob_matches("src/policy/*", "src/policy2/mod.rs"));
        assert!(path_glob_matches("src/policy/**", "src/policy/sub/x.rs"));
        assert!(path_glob_matches("src/policy/**", "src/policy/mod.rs"));
        assert!(path_glob_matches("src/policy/mod.rs", "src/policy/mod.rs"));
        assert!(!path_glob_matches(
            "src/policy/mod.rs",
            "src/policy/other.rs"
        ));
        assert!(path_glob_matches("src/polic?/mod.rs", "src/policy/mod.rs"));
        assert!(!path_glob_matches(
            "src/polic?/mod.rs",
            "src/policyx/mod.rs"
        ));
        assert!(!path_glob_matches("src/polic?/mod.rs", "src/polic/mod.rs"));
    }

    #[test]
    fn all_unset_record_scope_applies_but_unset_target_constrains() {
        let everything = UserContextScope {
            repo: None,
            path_glob: None,
            language: None,
            lifecycle_phase: None,
        };
        let constrained = UserContextScope {
            repo: None,
            path_glob: Some("src/policy/*".to_owned()),
            language: None,
            lifecycle_phase: None,
        };
        let target = UserContextScope {
            repo: None,
            path_glob: Some("src/policy/mod.rs".to_owned()),
            language: None,
            lifecycle_phase: None,
        };
        assert!(policy_scope_applies(&everything, &target));
        assert!(policy_scope_applies(&constrained, &target));
        assert!(!policy_scope_applies(&constrained, &everything));
    }
}
