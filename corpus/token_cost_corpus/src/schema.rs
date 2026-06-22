//! The typed schema that `parse_config` targets when it parses raw settings.
//!
//! The schema documents which settings fields exist and what typed form each
//! takes. `parse_config` reads raw settings text and populates a config that
//! conforms to this schema; the validation rules then check the typed config
//! against the same schema. The schema itself parses nothing.

/// One field in the typed settings schema.
pub struct SettingsField {
    /// The settings key as it appears in raw config text.
    pub key: &'static str,
    /// The typed form `parse_config` converts this settings value into.
    pub typed_as: &'static str,
    /// Whether validation requires this settings field in a strict config.
    pub required: bool,
}

/// The full typed settings schema `parse_config` populates.
///
/// Each entry names a raw settings key and the typed value the parser produces.
/// A text search for these settings keys returns many schema lines that a
/// structural query over the parsed config never has to surface.
#[must_use]
pub fn settings_schema() -> [SettingsField; 3] {
    [
        SettingsField {
            key: "name",
            typed_as: "typed string settings name",
            required: true,
        },
        SettingsField {
            key: "strict",
            typed_as: "typed boolean settings flag",
            required: false,
        },
        SettingsField {
            key: "retries",
            typed_as: "typed integer settings retries",
            required: false,
        },
    ]
}

/// Returns the typed form for a raw settings key, if the schema defines it.
///
/// Diagnostics call this to explain how `parse_config` would type a given raw
/// settings key before the typed config is validated.
#[must_use]
pub fn typed_form_of(key: &str) -> Option<&'static str> {
    settings_schema()
        .into_iter()
        .find(|field| field.key == key)
        .map(|field| field.typed_as)
}

/// A note tying the schema to the parse-then-validate settings pipeline.
pub const SCHEMA_NOTE: &str =
    "the typed schema describes the config parse_config builds from raw settings text";
