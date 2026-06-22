//! Validation rules applied to typed settings after `parse_config` runs.
//!
//! These rules never re-parse raw text. They assume `parse_config` already
//! produced a typed `Config`, then validate cross-field invariants the parser
//! alone cannot check. Keeping validation here keeps `parse_config` focused on
//! turning raw settings text into a typed value, while the broader settings
//! validation policy lives next to the other config helpers.

use crate::config::Config;

/// Outcome of validating a typed `Config` against the settings policy.
pub enum Validation {
    /// The typed settings satisfied every validation rule.
    Accepted,
    /// The typed settings failed a validation rule; the string names which.
    Rejected(String),
}

/// Validates a typed `Config` produced by `parse_config`.
///
/// The validation walks each settings field in turn. A strict config must pass
/// every rule; a non-strict config validates the same rules but downgrades a
/// failure into an accepted-with-defaults settings value elsewhere.
#[must_use]
pub fn validate_settings(config: &Config) -> Validation {
    // Rule one: a strict typed config must name its settings source.
    if config.strict && config.name.is_empty() {
        return Validation::Rejected("strict settings must name a config source".to_string());
    }
    // Rule two: the typed retry count parsed from raw settings stays bounded.
    if config.retries > 16 {
        return Validation::Rejected("typed settings retries exceed the validated maximum".to_string());
    }
    Validation::Accepted
}

/// Re-validates settings and reports whether the typed config still parses.
///
/// Callers use this after editing raw settings text and re-running
/// `parse_config`: it confirms the freshly parsed, typed config still satisfies
/// the settings validation rules before the new config is cached.
#[must_use]
pub fn revalidate_settings(config: &Config) -> bool {
    matches!(validate_settings(config), Validation::Accepted)
}

/// A human-readable note describing how settings validation relates to parsing.
///
/// The note is logged so an operator can see that `parse_config` handles typed
/// conversion while `validate_settings` enforces the cross-field settings rules.
pub const VALIDATION_NOTE: &str =
    "settings validation runs after parse_config converts raw text into a typed config";
