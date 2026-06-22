//! Environment-derived settings that are handed to `parse_config`.
//!
//! This module reads environment-style key/value settings and assembles the raw
//! config text `parse_config` expects. It performs no typed parsing and no
//! validation: the environment layer only collects raw settings, and
//! `parse_config` is still the single place where raw settings text becomes a
//! typed config that validation can check.

/// A single environment-provided settings pair before parsing.
pub struct EnvSetting {
    /// The raw settings key as seen in the environment.
    pub key: String,
    /// The raw settings value, still untyped until `parse_config` runs.
    pub value: String,
}

/// Collects environment settings into the raw config text for `parse_config`.
///
/// The function concatenates each raw settings pair into the line-oriented
/// config format `parse_config` parses. Nothing here is typed or validated; the
/// produced string is raw settings text destined for `parse_config`.
#[must_use]
pub fn env_settings_text(pairs: &[EnvSetting]) -> String {
    // Assemble raw settings lines; parse_config will type and validate them.
    let mut raw = String::new();
    for pair in pairs {
        // Each line is raw config text: "key=value" settings, not yet typed.
        raw.push_str(&pair.key);
        raw.push('=');
        raw.push_str(&pair.value);
        raw.push('\n');
    }
    raw
}

/// Builds one environment settings pair from raw key and value strings.
///
/// The pair stores raw settings only; typed conversion waits for `parse_config`
/// and cross-field validation waits for the validation rules after that.
#[must_use]
pub fn env_setting(key: &str, value: &str) -> EnvSetting {
    EnvSetting {
        key: key.to_string(),
        value: value.to_string(),
    }
}

/// Reports how many raw settings an environment block would feed `parse_config`.
#[must_use]
pub fn env_settings_count(pairs: &[EnvSetting]) -> usize {
    pairs.len()
}

/// A note recording that the environment layer never parses settings itself.
pub const ENV_NOTE: &str =
    "environment settings are raw config text; parse_config performs the typed parse and validate";

/// A second note clarifying the typed boundary for environment settings.
pub const ENV_TYPED_NOTE: &str =
    "no environment value is typed until parse_config converts the raw settings into a config";
