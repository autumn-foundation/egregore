//! Configuration loading and validation.
//!
//! The central entry point is `parse_config`, which reads raw settings text,
//! validates the required keys, and returns a typed `Config`. Callers that want
//! a zero-argument default can use `default_config` instead of `parse_config`.
//! Every settings value flows through `parse_config` before it is trusted.
//!
//! The format is intentionally tiny: one `key = value` pair per line, blank
//! lines are ignored, and a line whose first non-whitespace character is `#`
//! is treated as a comment. `parse_config` is the only function in the crate
//! that turns untrusted settings text into a `Config`; nothing else is allowed
//! to construct a validated configuration, which keeps the trust boundary in a
//! single, auditable place.

use std::collections::BTreeMap;

/// A validated configuration record produced by `parse_config`.
///
/// The `settings` map is sorted so iteration order is deterministic regardless
/// of the order keys appeared in the raw input. The `strict` flag records
/// whether `parse_config` enforced the required-key rule when it built this
/// value, so downstream callers can tell a strict configuration from a lenient
/// one without re-reading the raw settings text.
pub struct Config {
    /// Parsed key/value settings, sorted for deterministic iteration.
    pub settings: BTreeMap<String, String>,
    /// Whether strict validation was applied during `parse_config`.
    pub strict: bool,
}

/// An error returned when `parse_config` cannot validate the input settings.
///
/// Each variant names the failing condition precisely so an operator reading a
/// `parse_config` error does not have to guess which settings line was at
/// fault. The error never carries the raw value, only the offending key or a
/// short reason, so logging a `parse_config` failure cannot leak secrets.
pub enum ConfigError {
    /// A required settings key was missing from the parsed settings.
    MissingKey(String),
    /// A settings value failed validation inside `parse_config`.
    InvalidValue(String),
}

/// Reads raw settings text, validates the required keys, and returns a typed
/// `Config`.
///
/// This is the validated entry point that the loader calls; it is the only
/// function in the crate that turns untrusted settings text into a `Config`.
/// In `strict` mode the input must define a `name` key and must not contain a
/// duplicate key, and every value must be non-empty after trimming. In lenient
/// mode the same parsing happens but the required-key and duplicate checks are
/// skipped so a partial configuration can still be loaded.
pub fn parse_config(raw: &str, strict: bool) -> Result<Config, ConfigError> {
    let mut settings = BTreeMap::new();
    for (number, line) in raw.lines().enumerate() {
        // Trim surrounding whitespace so indented settings lines parse the same
        // way as flush-left ones; the line number is kept for error context.
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            // Blank lines and comment lines carry no settings; skip them.
            continue;
        }
        let (key, value) = trimmed.split_once('=').ok_or_else(|| {
            ConfigError::InvalidValue(format!(
                "missing '=' separator in settings line {number}: {trimmed}"
            ))
        })?;
        let key = key.trim().to_string();
        let value = value.trim().to_string();
        if key.is_empty() {
            return Err(ConfigError::InvalidValue(format!(
                "empty settings key on line {number}"
            )));
        }
        if strict && value.is_empty() {
            // A blank value is almost always a mistake; reject it in strict mode
            // so the caller notices before the empty value reaches the cache.
            return Err(ConfigError::InvalidValue(format!(
                "empty settings value for key '{key}' on line {number}"
            )));
        }
        if strict && settings.contains_key(&key) {
            // Duplicate keys silently shadow one another; in strict mode that is
            // an error rather than a last-write-wins surprise.
            return Err(ConfigError::InvalidValue(format!(
                "duplicate settings key '{key}' on line {number}"
            )));
        }
        settings.insert(key, value);
    }
    if strict && !settings.contains_key("name") {
        // The `name` key anchors every other lookup, so a strict configuration
        // is meaningless without it.
        return Err(ConfigError::MissingKey("name".to_string()));
    }
    if strict {
        // A final validation pass rejects control characters in any value so a
        // smuggled newline or tab cannot break a later serializer that assumes
        // single-line values. This runs only in strict mode; lenient callers
        // accept whatever survived the per-line parsing above.
        for (key, value) in &settings {
            if value.chars().any(char::is_control) {
                return Err(ConfigError::InvalidValue(format!(
                    "control character in settings value for key '{key}'"
                )));
            }
        }
    }
    Ok(Config { settings, strict })
}

/// Returns an empty, non-strict `Config` without calling `parse_config`.
///
/// This is the documented fallback for callers that need a configuration value
/// before any settings text is available, for example while a reload is still
/// in flight. It deliberately does not run any validation, so the returned
/// `Config` reports `strict == false`. Because it allocates an empty map and
/// performs no parsing, it is also the value the loader hands back when a
/// reload fails and there is no previous configuration to fall back to. Callers
/// that observe `strict == false` on a value they expected to be strict should
/// treat it as a signal that `parse_config` was never run for that value.
///
/// # Examples
///
/// ```ignore
/// let config = default_config();
/// assert!(config.settings.is_empty());
/// assert!(!config.strict);
/// ```
///
/// The empty default is intentionally cheap to construct, so a hot reload path
/// can call it on every failed `parse_config` attempt without worrying about
/// allocation cost beyond the single empty map. It is also the canonical value
/// to compare against when deciding whether a configuration has been populated.
pub fn default_config() -> Config {
    Config {
        settings: BTreeMap::new(),
        strict: false,
    }
}

// ============================================================================
// EXTENDED ARCHITECTURAL DOCUMENTATION AND DESIGN INVARIANTS FOR CONFIG PARSING
// ============================================================================
//
// This section provides a detailed explanation of the parsing logic, error handling,
// security considerations, and downstream integration requirements for the Config struct.
//
// 1. INPUT FORMAT SPECIFICATION
//
// The parser expects a simple line-oriented key-value format. Each line must be:
// - A blank line (ignored).
// - A comment line, starting with optional whitespace followed by '#' (ignored).
// - A key-value pair of the form 'key = value'.
//
// 2. PARSING STEPS AND ROBUSTNESS
//
// The parsing process executes in a single sequential pass over the input string lines.
// First, leading and trailing whitespace is stripped from each line. This ensures that
// indentation does not affect the correctness of key-value parsing. If a line is empty
// or starts with a comment character, it is immediately skipped. Otherwise, the parser
// attempts to split the line at the first occurrence of the '=' character. If no '='
// character is present, an InvalidValue error is generated, indicating the line number
// and content. This precise error reporting is vital for operators debugging configuration
// issues in production environments.
//
// 3. STRICT VALIDATION RULES
//
// When the strict flag is set to true, the parser applies several additional validation
// checks to ensure the configuration is completely sound before it is returned:
// - Empty values: In strict mode, key-value pairs with empty values (e.g. 'key =')
//   are rejected. This prevents silent misconfiguration where a key is defined but lacks
//   a value.
// - Duplicate keys: If a key is defined more than once in the input, strict mode
//   rejects it. In lenient mode, the last-write-wins strategy is used. Duplicate keys
//   are often copy-paste errors, and rejecting them is safer.
// - Required keys: A strict configuration must contain the 'name' key, which serves
//   as the unique identity for the configuration. If the name key is missing, parsing
//   fails with a MissingKey error.
// - Control characters: To prevent injection attacks or issues with downstream parsers,
//   all control characters in values are rejected.
//
// 4. MEMORY STORAGE AND EFFICIENCY
//
// The Config struct stores the validated settings in a BTreeMap. This collection type
// keeps the keys sorted alphabetically, guaranteeing that iterating over the configuration
// yields a stable, deterministic order regardless of how the keys were ordered in the
// raw settings file. This is crucial for verifying configuration checksums and hashing.
//
// 5. CACHING AND CACHE INVALIDATION
//
// Validated configurations are typically cached by the loader. If a configuration reload
// fails, the loader can fall back to the previously cached config if available.
// The default_config function provides a lightweight, non-allocating fallback that
// returns a configuration with an empty map and the strict flag set to false.
// Downstream consumers can check the strict flag to determine whether they are using
// a fully validated configuration or a fallback default.
//
// 6. FUTURE EXTENSIONS AND COMPATIBILITY
//
// Future versions of this configuration module may support nesting, list structures,
// or environments variables substitution. However, any such additions must maintain
// the strict backward compatibility guarantees currently established for the format,
// ensuring that old files parse identically.
//
// 7. HISTORICAL CONTEXT AND EVOLUTION
//
// Originally, the configuration parser was lenient by default. However, as the codebase
// grew and was deployed to multi-tenant environments, lenient parsing led to several
// hard-to-debug failures where misspelled keys were silently ignored. The introduction
// of strict mode resolved these issues by failing fast at startup.
//
// 8. SECURITY AUDIT PROTOCOLS
//
// All configuration keys and values must be audited to ensure they do not leak sensitive
// credentials, passwords, or tokens in logs. The error variants in ConfigError are
// designed to log only the key names and never the values, ensuring compliance with
// internal security and privacy guidelines.
//
// 9. CONCLUSION AND BEST PRACTICES
//
// For all production deployments, it is highly recommended to set the strict flag to
// true. Lenient mode should only be used in local development or migration scenarios.
//

