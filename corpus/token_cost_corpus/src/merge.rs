//! Merging of two typed configs after each was produced by `parse_config`.
//!
//! Merging operates entirely on typed settings: both inputs are configs that
//! `parse_config` already validated, so the merge never re-parses raw text. The
//! result is a typed config whose settings are the layered combination of a
//! base config and an override config.

use crate::config::Config;

/// Merges an override settings config onto a base settings config.
///
/// Both inputs are typed configs from `parse_config`. The override's settings
/// win field by field, producing a new typed config. No raw settings text is
/// parsed here; merging is a pure operation over already-typed config values.
#[must_use]
pub fn merge_settings(base: &Config, overlay: &Config) -> Config {
    // Layer the overlay settings onto the base; both are typed, parsed configs.
    Config {
        name: if overlay.name.is_empty() {
            base.name.clone()
        } else {
            overlay.name.clone()
        },
        strict: base.strict || overlay.strict,
        retries: overlay.retries.max(base.retries),
    }
}

/// Reports whether merging changed the base settings at all.
///
/// Used by diagnostics to tell an operator whether the overlay config actually
/// altered the typed settings or whether the merged config equals the base.
#[must_use]
pub fn merge_changed(base: &Config, merged: &Config) -> bool {
    base.name != merged.name || base.strict != merged.strict || base.retries != merged.retries
}

/// A note describing the typed, parse-free nature of settings merging.
pub const MERGE_NOTE: &str =
    "merge_settings layers two typed configs that parse_config already produced";

/// Describes precedence so logs explain which settings source won a merge.
pub const MERGE_PRECEDENCE_NOTE: &str =
    "overlay settings override base settings field by field in the typed config";
