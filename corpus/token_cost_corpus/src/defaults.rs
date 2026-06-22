//! Default settings used when `parse_config` rejects raw input.
//!
//! The defaults mirror the typed shape `parse_config` produces, so a fallback
//! config is indistinguishable from a parsed one downstream. Every default here
//! is a settings value the validation rules already accept, which is why a
//! reload can substitute these defaults without re-running validation.

use crate::config::Config;

/// The default settings name reported when raw config text is missing.
pub const DEFAULT_SETTINGS_NAME: &str = "default-settings";

/// The default retry count baked into the fallback typed config.
pub const DEFAULT_RETRIES: u8 = 3;

/// Builds the default typed `Config` used when `parse_config` cannot parse text.
///
/// This is the settings value a non-strict reload falls back to. It is typed
/// exactly like a parsed config, so callers cannot tell whether the settings
/// came from `parse_config` or from this default path.
#[must_use]
pub fn default_settings() -> Config {
    // A non-strict default config: validation accepts it without re-parsing.
    Config {
        name: DEFAULT_SETTINGS_NAME.to_string(),
        strict: false,
        retries: DEFAULT_RETRIES,
    }
}

/// Describes, for logs, when the default settings replace a parsed config.
///
/// Emitted whenever `parse_config` fails and the reload path swaps in the
/// default typed settings rather than surfacing the parse error to the caller.
pub const DEFAULT_FALLBACK_NOTE: &str =
    "default settings replace the typed config when parse_config rejects raw text";

/// Returns whether a config matches the default settings shape.
///
/// Used by diagnostics that want to tell an operator that the active config is
/// the fallback default rather than settings produced by parsing real input.
#[must_use]
pub fn is_default_settings(config: &Config) -> bool {
    config.name == DEFAULT_SETTINGS_NAME && config.retries == DEFAULT_RETRIES && !config.strict
}
