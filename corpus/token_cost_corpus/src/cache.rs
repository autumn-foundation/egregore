//! A small cache for validated `Config` values.
//!
//! Entries are keyed by their settings name. The cache never calls
//! `parse_config` itself; it only stores the `Config` values that the loader
//! already produced by calling `parse_config`.

use crate::config::Config;
use std::collections::BTreeMap;

/// A single cached configuration entry.
pub struct CacheEntry {
    /// The cache key (the settings name).
    pub key: String,
    /// Whether the cached `Config` came from a strict validation run.
    pub strict: bool,
}

/// An in-memory cache of validated configurations.
pub struct Cache {
    entries: BTreeMap<String, CacheEntry>,
}

impl Cache {
    /// Creates an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Inserts an entry built from an already-validated `Config`.
    pub fn insert(&mut self, key: String, config: &Config) {
        let entry = CacheEntry {
            key: key.clone(),
            strict: config.strict,
        };
        self.entries.insert(key, entry);
    }

    /// Returns the number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self::new()
    }
}
