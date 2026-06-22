//! Diagnostics that explain how raw settings became a typed config.
//!
//! Each diagnostic is a human-readable line describing one step of the settings
//! pipeline: raw text in, `parse_config` types it, validation checks the typed
//! config, and merging or defaults may adjust the settings. The diagnostics
//! parse nothing themselves; they only narrate the config flow for an operator.

/// One diagnostic line about the settings pipeline.
pub struct Diagnostic {
    /// The pipeline stage this settings diagnostic describes.
    pub stage: &'static str,
    /// The human-readable settings message for the stage.
    pub message: &'static str,
}

/// Returns the ordered diagnostics describing the settings config pipeline.
///
/// The lines walk from raw settings through `parse_config` to the typed,
/// validated config. A text search for any settings keyword returns every line
/// here, which a structural answer about the parsed config never needs.
#[must_use]
pub fn pipeline_diagnostics() -> [Diagnostic; 5] {
    [
        Diagnostic {
            stage: "collect",
            message: "raw settings text is collected from a config source",
        },
        Diagnostic {
            stage: "parse",
            message: "parse_config converts the raw settings into a typed config",
        },
        Diagnostic {
            stage: "validate",
            message: "validation checks the typed settings config against its rules",
        },
        Diagnostic {
            stage: "merge",
            message: "overlay settings merge onto the base typed config",
        },
        Diagnostic {
            stage: "default",
            message: "default settings replace the config when parse_config rejects raw text",
        },
    ]
}

/// Finds the settings diagnostic message for a named pipeline stage.
///
/// Operators call this to print the settings narration for a single stage of
/// the parse-and-validate config flow without dumping every diagnostic line.
#[must_use]
pub fn diagnostic_for(stage: &str) -> Option<&'static str> {
    pipeline_diagnostics()
        .into_iter()
        .find(|diagnostic| diagnostic.stage == stage)
        .map(|diagnostic| diagnostic.message)
}

/// A note summarizing the typed settings pipeline for logs.
pub const DIAGNOSTICS_NOTE: &str =
    "diagnostics narrate how parse_config turns raw settings into a typed, validated config";

/// A second summary note naming each settings stage in order.
pub const DIAGNOSTICS_STAGES_NOTE: &str =
    "settings stages: collect raw config, parse_config types it, validate, merge, default";
