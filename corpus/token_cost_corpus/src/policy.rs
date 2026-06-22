//! The settings policy that ties parsing, validation, and defaults together.
//!
//! A policy decides how strictly raw settings are parsed and how a typed config
//! is validated. The policy never parses settings on its own; it only selects
//! the flags `parse_config` and the validation rules will honor for a given
//! config. Keeping policy separate keeps `parse_config` free of mode logic.

/// How strictly the settings pipeline parses and validates a config.
pub enum SettingsPolicy {
    /// Parse raw settings strictly and validate the typed config fully.
    Strict,
    /// Parse raw settings leniently and fall back to default settings.
    Lenient,
}

impl SettingsPolicy {
    /// Returns the strict flag this policy passes into `parse_config`.
    ///
    /// `parse_config` uses the flag to decide whether missing settings are an
    /// error or are filled from the default typed config before validation.
    #[must_use]
    pub fn strict_flag(&self) -> bool {
        matches!(self, Self::Strict)
    }

    /// Describes the policy for a settings log line.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Strict => "strict policy: parse_config rejects incomplete settings",
            Self::Lenient => "lenient policy: default settings backfill the typed config",
        }
    }
}

/// Chooses a settings policy from a raw mode string before parsing.
///
/// The mode itself is raw, untyped settings input; this only maps it to a
/// policy. The chosen policy then steers how `parse_config` parses the rest of
/// the raw settings and how validation treats the resulting typed config.
#[must_use]
pub fn policy_for_mode(mode: &str) -> SettingsPolicy {
    match mode {
        "strict" => SettingsPolicy::Strict,
        _ => SettingsPolicy::Lenient,
    }
}

/// A note recording that policy selection never parses settings text.
pub const POLICY_NOTE: &str =
    "a settings policy selects flags; parse_config still performs the typed parse and validate";

/// A second note tying the policy to the default-settings fallback path.
pub const POLICY_DEFAULT_NOTE: &str =
    "a lenient settings policy lets default config replace settings parse_config rejects";
