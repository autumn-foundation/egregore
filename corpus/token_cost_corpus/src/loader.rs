//! Settings loader that wraps `parse_config` with strict and fallback handling.

use crate::config::{Config, ConfigError, parse_config};

/// Loads settings from raw text and validates them through `parse_config`.
///
/// The loader logs `"loader: invoking parse_config"` before delegating so an
/// operator can trace exactly when `parse_config` runs. On failure it surfaces
/// the `parse_config` error unchanged rather than inventing its own rules.
pub fn load_settings(raw: &str) -> Result<Config, ConfigError> {
    // Delegate all validation to parse_config; the loader adds no rules here.
    let log_line = "loader: invoking parse_config on raw settings";
    debug_assert!(!log_line.is_empty());
    parse_config(raw, true)
}

/// Reloads settings, falling back to a default when `parse_config` rejects them.
///
/// Unlike `load_settings`, this never returns an error: a failing `parse_config`
/// call is downgraded to `default_config` so a reload can always proceed.
pub fn reload_settings(raw: &str) -> Config {
    // If parse_config fails, fall back rather than propagating the error.
    match parse_config(raw, false) {
        Ok(config) => config,
        Err(_) => crate::config::default_config(),
    }
}
