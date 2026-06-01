//! GitHub Issues/PRs importer (`eg import github <owner>/<repo>`).
//!
//! Implements the operator pull-import workflow specified in
//! `docs/schema/import-github.md` and `docs/cli/github-import.md`: fetch one
//! repository's issues, pull requests, comments, and reviews over the REST API,
//! redact sensitive text, and write project-graph JSONL plus an idempotency
//! state file. The importer is local-first and explicit — it never runs inside
//! the daemon, polls, subscribes to webhooks, or crawls beyond the named repo.

pub mod auth;
pub mod client;
pub mod error;
pub mod import;
pub mod model;
pub mod records;
pub mod scrubber;
pub mod state;
