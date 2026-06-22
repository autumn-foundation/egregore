//! Token-cost fixture crate.
//!
//! A deliberately small settings stack whose public entry point is
//! `parse_config`. The modules mention `parse_config` and `settings` in doc
//! comments and log strings on purpose: a plain text search for those terms
//! returns many lines (including comment and string-literal false positives)
//! that a structural `eg query` answer does not. The crate exists only as a
//! pinned corpus for the `eg audit token-cost` measurement gate (issue #84).

pub mod cache;
pub mod config;
pub mod loader;
