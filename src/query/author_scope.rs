//! Author scoping for memory recall (issue #195).
//!
//! In multi-agent / hive-mind deployments many agents write observations into
//! one shared store. The agent-memory schema mandates `agent_id` (and
//! `session_id`) on every agent-authored node (`docs/schema/agent-memory.md`
//! §3); this module turns that captured provenance into a queryable recall
//! dimension: an [`AuthorScope`] selects observations by their authoring agent
//! identity.
//!
//! # Contract
//!
//! - An **inactive** scope (no include, no exclude) matches every record: a
//!   recall without an author selector keeps its default unscoped behavior.
//! - An **active** scope matches only records carrying a resolvable `agent_id`.
//!   Deterministic code-graph facts carry no `agent_id`, so they are
//!   structurally excluded from author-scoped recall — never a filter-order
//!   accident.
//! - `include` requires exact equality with the record's `agent_id`; `exclude`
//!   vetoes exact equality. The two compose: a record must pass both, so when
//!   both name the same agent the veto wins and the record is dropped.
//! - Matching is exact, case-sensitive string equality on the opaque
//!   `agent_id` handle: no trimming, no case folding, no substring matching.
//!   `agent_id` is a stable identity, not a search term.
//! - When an author selector matches no observations, the recall lanes report
//!   an explicit empty result — not an error and not a silent fallback to
//!   unscoped recall. (The empty-answer shape lives in the CLI lanes; the
//!   predicate here only decides membership.)

use crate::ir::GraphRecord;

/// Selects recalled observations by authoring agent identity (issue #195).
///
/// Carries the CLI's `--agent` (include) and `--not-agent` (exclude) values.
/// Shared by the meaning-based recall lane (`eg query semantic-memory`,
/// issue #91) and the subsystem-scoped context lane (`eg query subsystem`,
/// issue #83) so the two recall paths cannot drift apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuthorScope {
    /// Keep only observations authored by this `agent_id` (`--agent`).
    pub include: Option<String>,
    /// Drop observations authored by this `agent_id` (`--not-agent`).
    pub exclude: Option<String>,
}

impl AuthorScope {
    /// `true` when at least one selector is set. An inactive scope is a
    /// no-op over the record stream.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.include.is_some() || self.exclude.is_some()
    }

    /// Whether a record carrying the given `agent_id` may be recalled under
    /// this scope. See the module-level contract for the membership rules.
    #[must_use]
    pub fn matches(&self, agent_id: Option<&str>) -> bool {
        // Inactive scope: recall stays unscoped.
        if !self.is_active() {
            return true;
        }
        // Active scope: a record without an `agent_id` never matches, so
        // deterministic code facts can never leak into author-scoped results.
        let Some(agent_id) = agent_id else {
            return false;
        };
        // `include` requires exact equality.
        if let Some(include) = self.include.as_deref() {
            if agent_id != include {
                return false;
            }
        }
        // `exclude` requires inequality; when both name the same agent the
        // exclusion wins.
        if let Some(exclude) = self.exclude.as_deref() {
            if agent_id == exclude {
                return false;
            }
        }
        true
    }
}

/// The authoring `agent_id` carried on a record, if it carries one.
///
/// Deterministic code-graph records (and edges / tombstones) have no
/// `agent_id`; they yield `None`.
#[must_use]
pub fn record_agent_id(record: &GraphRecord) -> Option<&str> {
    match record {
        GraphRecord::Node { agent_id, .. } => agent_id.as_deref(),
        GraphRecord::Edge { .. } | GraphRecord::Tombstone { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_scope_matches_everything() {
        let scope = AuthorScope::default();
        assert!(!scope.is_active());
        assert!(
            scope.matches(None),
            "inactive scope must not filter agent-less records"
        );
        assert!(scope.matches(Some("agent_1")));
    }

    #[test]
    fn include_selects_only_the_named_agent() {
        let scope = AuthorScope {
            include: Some("agent_1".to_owned()),
            exclude: None,
        };
        assert!(scope.is_active());
        assert!(scope.matches(Some("agent_1")));
        assert!(
            !scope.matches(Some("agent_2")),
            "include must drop other agents"
        );
    }

    #[test]
    fn exclude_drops_only_the_named_agent() {
        let scope = AuthorScope {
            include: None,
            exclude: Some("agent_1".to_owned()),
        };
        assert!(
            !scope.matches(Some("agent_1")),
            "exclude must drop the named agent"
        );
        assert!(scope.matches(Some("agent_2")));
    }

    #[test]
    fn include_and_exclude_compose_with_veto_winning() {
        let scope = AuthorScope {
            include: Some("agent_1".to_owned()),
            exclude: Some("agent_2".to_owned()),
        };
        assert!(scope.matches(Some("agent_1")));
        assert!(!scope.matches(Some("agent_2")));
        assert!(!scope.matches(Some("agent_3")));

        // Both selectors naming the same agent: the veto wins.
        let conflict = AuthorScope {
            include: Some("agent_1".to_owned()),
            exclude: Some("agent_1".to_owned()),
        };
        assert!(
            !conflict.matches(Some("agent_1")),
            "exclude must win when both selectors name the same agent"
        );
    }

    #[test]
    fn active_scope_rejects_records_without_agent_id() {
        // Deterministic code-graph facts carry no `agent_id`: they are
        // structurally excluded from author-scoped recall.
        for scope in [
            AuthorScope {
                include: Some("agent_1".to_owned()),
                exclude: None,
            },
            AuthorScope {
                include: None,
                exclude: Some("agent_1".to_owned()),
            },
            AuthorScope {
                include: Some("agent_1".to_owned()),
                exclude: Some("agent_2".to_owned()),
            },
        ] {
            assert!(
                !scope.matches(None),
                "active scope {scope:?} must reject agent-less records"
            );
        }
    }

    #[test]
    fn matching_is_exact_and_case_sensitive() {
        let scope = AuthorScope {
            include: Some("Agent_1".to_owned()),
            exclude: None,
        };
        assert!(scope.matches(Some("Agent_1")));
        assert!(
            !scope.matches(Some("agent_1")),
            "matching must be case-sensitive"
        );
        assert!(
            !scope.matches(Some("Agent_1x")),
            "matching must not be substring-based"
        );
        assert!(
            !scope.matches(Some(" Agent_1")),
            "matching must not trim whitespace"
        );
    }

    #[test]
    fn record_agent_id_reads_node_agent_id() {
        let mut node = GraphRecord::node(
            "agent_memory:v1:test".to_owned(),
            crate::ir::NodeKind::Observation,
            None,
            None,
            None,
            "test observation".to_owned(),
        );
        assert_eq!(record_agent_id(&node), None);
        if let GraphRecord::Node {
            agent_id: ref mut aid,
            ..
        } = node
        {
            *aid = Some("agent_7".to_owned());
        }
        assert_eq!(record_agent_id(&node), Some("agent_7"));
    }
}
