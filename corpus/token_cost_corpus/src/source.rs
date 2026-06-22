//! Settings sources that feed raw text into `parse_config`.
//!
//! A source yields the raw settings string; it never parses or validates that
//! string itself. Parsing into a typed config is `parse_config`'s job, and
//! validating the typed config is the validation module's job. Keeping the
//! source dumb means the same raw settings flow through `parse_config`
//! regardless of where the config text originated.

/// Where a block of raw settings text came from.
pub enum SettingsSource {
    /// Settings read from an inline string literal.
    Inline(String),
    /// Settings that an operator typed at a prompt.
    Operator(String),
    /// Settings loaded from an environment-provided config blob.
    Environment(String),
}

impl SettingsSource {
    /// Returns the raw settings text this source would hand to `parse_config`.
    ///
    /// The returned string is unparsed and unvalidated: it is exactly the raw
    /// config text that `parse_config` will later turn into a typed value.
    #[must_use]
    pub fn raw_settings(&self) -> &str {
        match self {
            Self::Inline(text) | Self::Operator(text) | Self::Environment(text) => text,
        }
    }

    /// Describes the source for a log line, naming the settings origin.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Inline(_) => "inline settings handed to parse_config",
            Self::Operator(_) => "operator settings handed to parse_config",
            Self::Environment(_) => "environment settings handed to parse_config",
        }
    }
}

/// Builds an inline settings source from a raw config string.
///
/// The raw settings are stored verbatim; no parsing or typed conversion happens
/// here, so a later `parse_config` sees exactly the text the caller supplied.
#[must_use]
pub fn inline_settings(raw: &str) -> SettingsSource {
    SettingsSource::Inline(raw.to_string())
}

/// A note explaining that sources never parse the settings they carry.
pub const SOURCE_NOTE: &str =
    "a settings source carries raw config text; parse_config performs the typed parsing";
