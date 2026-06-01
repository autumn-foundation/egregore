//! Thin synchronous GitHub REST client (`docs/schema/import-github.md` §2–5).
//!
//! Built on `ureq` for full control over the request budget the conformance
//! tests assert: a two-step repo-probe auth state machine, `Link` pagination,
//! per-page `ETag` conditional fetch, rate-limit header handling with backoff, and
//! a request counter. The base URL is injectable so tests drive it against a
//! local mock server with no live network access.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::Value;

use crate::github::{
    error::{GithubError, GithubResult},
    scrubber,
};

/// Maximum time the unauthenticated repo probe will wait for an anonymous
/// rate-limit reset before exiting with `github_rate_limit_anon` (policy §2).
const ANON_RATE_LIMIT_WAIT_CAP: Duration = Duration::from_secs(5 * 60);

/// Default GitHub REST API base URL.
pub const DEFAULT_API_BASE: &str = "https://api.github.com";

/// Outcome of a single conditional list fetch.
pub enum FetchOutcome {
    /// Endpoint changed (or first fetch): JSON items plus the new per-page `ETags`.
    Modified {
        /// Flattened items across all pages.
        items: Vec<Value>,
        /// `"<path>?page=<n>" -> etag` for each page that returned one.
        etags: BTreeMap<String, String>,
    },
    /// Endpoint unchanged: every page returned `304 Not Modified`.
    NotModified,
}

/// Sleep hook so tests can run instantly without real backoff waits.
pub type SleepFn = Box<dyn Fn(Duration) + Send + Sync>;

/// A GitHub REST client bound to one API base URL and optional token.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<String>,
    /// Total HTTP requests issued (the per-run summary's `requests=` value).
    requests: AtomicU64,
    /// `X-RateLimit-Remaining` from the most recent response.
    last_remaining: AtomicU64,
    /// Max backoff retries for transient (5xx / rate-limit) failures.
    max_retries: u32,
    /// Injectable sleep (no-op in tests).
    sleep: SleepFn,
}

impl Client {
    /// Builds a client against `base` with an optional bearer `token`.
    #[must_use]
    pub fn new(base: impl Into<String>, token: Option<String>) -> Self {
        Self {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .timeout_read(Duration::from_secs(30))
                .build(),
            base: base.into().trim_end_matches('/').to_owned(),
            token,
            requests: AtomicU64::new(0),
            last_remaining: AtomicU64::new(u64::MAX),
            max_retries: 5,
            sleep: Box::new(std::thread::sleep),
        }
    }

    /// Replaces the sleep hook (tests pass a no-op to skip real waits).
    #[must_use]
    pub fn with_sleep(mut self, sleep: SleepFn) -> Self {
        self.sleep = sleep;
        self
    }

    /// Total number of HTTP requests issued so far.
    #[must_use]
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// `X-RateLimit-Remaining` from the last response, or `None` if never set.
    #[must_use]
    pub fn quota_remaining(&self) -> Option<u64> {
        let v = self.last_remaining.load(Ordering::Relaxed);
        (v != u64::MAX).then_some(v)
    }

    /// Whether this client carries a token (drives the probe state machine).
    #[must_use]
    pub const fn has_token(&self) -> bool {
        self.token.is_some()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Builds a request with the standard headers and optional auth.
    fn request(&self, url: &str, authed: bool) -> ureq::Request {
        let mut req = self
            .agent
            .get(url)
            .set("User-Agent", "egregore-github-import")
            .set("Accept", "application/vnd.github+json")
            .set("X-GitHub-Api-Version", "2022-11-28");
        if authed && let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        req
    }

    /// Records rate-limit headers from a response and returns
    /// `(remaining, reset_unix)` when present.
    fn record_rate_limit(&self, resp: &ureq::Response) -> (Option<u64>, Option<u64>) {
        let remaining = resp
            .header("X-RateLimit-Remaining")
            .and_then(|v| v.parse::<u64>().ok());
        if let Some(r) = remaining {
            self.last_remaining.store(r, Ordering::Relaxed);
        }
        let reset = resp
            .header("X-RateLimit-Reset")
            .and_then(|v| v.parse::<u64>().ok());
        (remaining, reset)
    }

    /// Two-step repository probe (`docs/schema/import-github.md` §2).
    ///
    /// Returns `Ok(())` when the repo is reachable; otherwise a typed
    /// [`GithubError`] naming the failing class.
    ///
    /// # Errors
    ///
    /// Returns `AuthMissing`, `AuthRejected`, `RepoNotFound`, or
    /// `RateLimitExhausted` per the documented decision table.
    pub fn probe_repo(&self, owner_repo: &str) -> GithubResult<()> {
        let path = format!("/repos/{owner_repo}");
        let url = self.url(&path);

        // Step 1 — unauthenticated probe. When the anonymous quota is exhausted
        // (403, remaining=0, no token) the policy says to wait until reset and
        // retry rather than fail immediately, exiting only if the wait would
        // exceed the cap.
        loop {
            self.requests.fetch_add(1, Ordering::Relaxed);
            match classify_probe(self.request(&url, false).call(), self) {
                ProbeStep::Ok => return Ok(()),
                ProbeStep::AuthRejected => return Err(GithubError::AuthRejected),
                ProbeStep::RateLimitNoToken { reset } => {
                    // Anonymous quota exhausted, no token fallback. Wait until
                    // reset and retry Step 1 unless that exceeds the cap.
                    match reset_wait(reset) {
                        Some(wait) if wait <= ANON_RATE_LIMIT_WAIT_CAP => {
                            (self.sleep)(wait);
                            // Retry Step 1.
                        }
                        _ => return Err(GithubError::RateLimitExhausted),
                    }
                }
                ProbeStep::NeedAuth => {
                    // Fall through to Step 2 only when a token is available.
                    if !self.has_token() {
                        return Err(GithubError::AuthMissing);
                    }
                    break;
                }
            }
        }

        // Step 2 — authenticated re-probe.
        self.requests.fetch_add(1, Ordering::Relaxed);
        match self.request(&url, true).call() {
            Ok(resp) => {
                self.record_rate_limit(&resp);
                Ok(())
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err(GithubError::AuthRejected),
            // 404 (repo missing / token lacks access) and any transport error
            // both surface as "repository not found" at this step.
            Err(_) => Err(GithubError::RepoNotFound),
        }
    }

    /// Conditionally fetches all pages of a list endpoint.
    ///
    /// `prior_etags` maps `"<path>?page=<n>"` to a stored `ETag`. Every stored
    /// page is probed conditionally — a 304 on page 1 does **not** short-circuit
    /// the endpoint, because a later page can change while page 1 stays
    /// unchanged. Page URLs are reconstructed deterministically (`&page=<n>`)
    /// rather than read from a `Link` header so that 304 pages (which carry no
    /// `Link`) are still followed; a changed page's `Link` header is honoured to
    /// discover pages added since the last run.
    ///
    /// Returns [`FetchOutcome::NotModified`] only when **every** probed page
    /// returned 304; otherwise [`FetchOutcome::Modified`] with the items from the
    /// changed pages and the union of refreshed and retained per-page `ETags`.
    ///
    /// # Errors
    ///
    /// Returns `FetchFailed` after the retry budget or `InvalidResponse` for a
    /// non-JSON body.
    pub fn fetch_paginated(
        &self,
        source_class: &str,
        first_path: &str,
        prior_etags: &BTreeMap<String, String>,
    ) -> GithubResult<FetchOutcome> {
        // Highest page number we have a stored ETag for (0 on a fresh import).
        let prefix = format!("{first_path}?page=");
        let max_known: u32 = prior_etags
            .keys()
            .filter_map(|k| k.strip_prefix(&prefix))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(0);

        let mut items = Vec::new();
        let mut etags = BTreeMap::new();
        let mut any_modified = false;
        let mut page = 1u32;

        loop {
            let etag_key = format!("{first_path}?page={page}");
            let prior = prior_etags.get(&etag_key);
            let url = self.url(&page_path(first_path, page));
            let mut has_link_next = false;
            match self.get_with_retry(source_class, &url, prior)? {
                ConditionalResponse::NotModified => {
                    // Unchanged page: retain its stored ETag, emit no items. No
                    // Link header is available from a 304, so continuation
                    // relies on `max_known`.
                    if let Some(p) = prior {
                        etags.insert(etag_key, p.clone());
                    }
                }
                ConditionalResponse::Modified { body, etag, link } => {
                    any_modified = true;
                    let parsed: Value =
                        serde_json::from_str(&body).map_err(|_| GithubError::InvalidResponse {
                            source_class: source_class.to_owned(),
                        })?;
                    match parsed {
                        Value::Array(arr) => items.extend(arr),
                        other => items.push(other),
                    }
                    if let Some(e) = etag {
                        etags.insert(etag_key, e);
                    }
                    has_link_next = link_next(link.as_deref()).is_some();
                }
            }

            // Continue while more known pages remain, or the last 200 advertised
            // a `next` page (the endpoint grew since the previous run).
            if page < max_known || has_link_next {
                page += 1;
            } else {
                break;
            }
        }

        if any_modified {
            Ok(FetchOutcome::Modified { items, etags })
        } else {
            Ok(FetchOutcome::NotModified)
        }
    }

    /// Single GET with conditional header and transient-failure retry/backoff.
    fn get_with_retry(
        &self,
        source_class: &str,
        url: &str,
        prior_etag: Option<&String>,
    ) -> GithubResult<ConditionalResponse> {
        let mut attempt = 0u32;
        let mut last_status = 0u16;
        loop {
            self.requests.fetch_add(1, Ordering::Relaxed);
            let mut req = self.request(url, true);
            if let Some(etag) = prior_etag {
                req = req.set("If-None-Match", etag);
            }
            match req.call() {
                // ureq returns 3xx (including 304 Not Modified) in the Ok arm,
                // so the conditional-fetch short-circuit must be checked here.
                Ok(resp) if resp.status() == 304 => {
                    self.record_rate_limit(&resp);
                    return Ok(ConditionalResponse::NotModified);
                }
                Ok(resp) => {
                    let (remaining, reset) = self.record_rate_limit(&resp);
                    self.maybe_throttle(remaining, reset);
                    let etag = resp.header("ETag").map(str::to_owned);
                    let link = resp.header("Link").map(str::to_owned);
                    let body = resp
                        .into_string()
                        .map_err(|_| GithubError::InvalidResponse {
                            source_class: source_class.to_owned(),
                        })?;
                    return Ok(ConditionalResponse::Modified { body, etag, link });
                }
                Err(ureq::Error::Status(304, _)) => {
                    return Ok(ConditionalResponse::NotModified);
                }
                // 429 is always a rate-limit signal (primary or secondary), even
                // when GitHub omits Retry-After / X-RateLimit headers; fall back
                // to exponential backoff in that case. A 403 is only a rate-limit
                // event when it carries a rate-limit header — otherwise it is an
                // auth rejection handled below.
                Err(ureq::Error::Status(code @ (429 | 403), resp))
                    if code == 429 || is_rate_limited(&resp) =>
                {
                    last_status = code;
                    let (_, reset) = self.record_rate_limit(&resp);
                    if attempt >= self.max_retries {
                        return Err(GithubError::FetchFailed {
                            source_class: source_class.to_owned(),
                            status: code,
                        });
                    }
                    let wait = retry_after(&resp)
                        .or_else(|| reset_wait(reset))
                        .unwrap_or_else(|| backoff(attempt, 60, 600));
                    (self.sleep)(wait);
                    attempt += 1;
                }
                Err(ureq::Error::Status(401 | 403, _)) => {
                    return Err(GithubError::AuthRejected);
                }
                Err(ureq::Error::Status(code @ 502..=504, _)) => {
                    last_status = code;
                    if attempt >= self.max_retries {
                        return Err(GithubError::FetchFailed {
                            source_class: source_class.to_owned(),
                            status: code,
                        });
                    }
                    (self.sleep)(backoff(attempt, 5, 60));
                    attempt += 1;
                }
                Err(ureq::Error::Status(code, _)) => {
                    return Err(GithubError::FetchFailed {
                        source_class: source_class.to_owned(),
                        status: code,
                    });
                }
                Err(_transport) => {
                    // Network/transport failure: treat as transient.
                    if attempt >= self.max_retries {
                        return Err(GithubError::FetchFailed {
                            source_class: source_class.to_owned(),
                            status: last_status,
                        });
                    }
                    (self.sleep)(backoff(attempt, 5, 60));
                    attempt += 1;
                }
            }
        }
    }

    /// Applies the §4 rate-limit threshold: when remaining is below the
    /// token-dependent floor, sleep until the reset instant.
    fn maybe_throttle(&self, remaining: Option<u64>, reset: Option<u64>) {
        let floor = if self.has_token() { 100 } else { 5 };
        if let Some(r) = remaining
            && r < floor
            && let Some(wait) = reset_wait(reset)
        {
            (self.sleep)(wait);
        }
    }
}

enum ConditionalResponse {
    NotModified,
    Modified {
        body: String,
        etag: Option<String>,
        link: Option<String>,
    },
}

enum ProbeStep {
    Ok,
    NeedAuth,
    AuthRejected,
    /// Anonymous quota exhausted with no token; `reset` is the
    /// `X-RateLimit-Reset` UNIX timestamp when known.
    RateLimitNoToken {
        reset: Option<u64>,
    },
}

fn classify_probe(result: Result<ureq::Response, ureq::Error>, client: &Client) -> ProbeStep {
    match result {
        Ok(resp) => {
            client.record_rate_limit(&resp);
            ProbeStep::Ok
        }
        Err(ureq::Error::Status(404, _)) => ProbeStep::NeedAuth,
        Err(ureq::Error::Status(403, ref resp)) if is_rate_limited(resp) => {
            let (_, reset) = client.record_rate_limit(resp);
            if client.has_token() {
                // Anonymous IP quota exhausted but auth quota is separate.
                ProbeStep::NeedAuth
            } else {
                ProbeStep::RateLimitNoToken { reset }
            }
        }
        Err(ureq::Error::Status(401 | 403, _)) => ProbeStep::AuthRejected,
        // Other transport/status: surface as auth-missing-style unreachability
        // only when no token; otherwise let Step 2 try.
        Err(_) => {
            if client.has_token() {
                ProbeStep::NeedAuth
            } else {
                ProbeStep::AuthRejected
            }
        }
    }
}

/// Returns `true` when a 403/429 response carries a zero remaining-quota header.
fn is_rate_limited(resp: &ureq::Response) -> bool {
    resp.header("X-RateLimit-Remaining")
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|r| r == 0)
        || resp.header("Retry-After").is_some()
}

/// Parses a `Retry-After` header (seconds) into a duration.
fn retry_after(resp: &ureq::Response) -> Option<Duration> {
    resp.header("Retry-After")
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Computes the wait until a `X-RateLimit-Reset` UNIX timestamp.
fn reset_wait(reset: Option<u64>) -> Option<Duration> {
    let reset = reset?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(Duration::from_secs(reset.saturating_sub(now)))
}

/// Exponential backoff in seconds: `start * 2^attempt`, capped at `cap`.
fn backoff(attempt: u32, start: u64, cap: u64) -> Duration {
    let secs = start.saturating_mul(2u64.saturating_pow(attempt)).min(cap);
    Duration::from_secs(secs)
}

/// Builds the request path for page `n`, appending an explicit `page=<n>` query
/// parameter. GitHub treats `page=1` as the default page, so this is safe for
/// the first page and lets every stored page be probed conditionally.
fn page_path(first_path: &str, n: u32) -> String {
    let sep = if first_path.contains('?') { '&' } else { '?' };
    format!("{first_path}{sep}page={n}")
}

/// Extracts the `rel="next"` URL from a `Link` header.
fn link_next(link: Option<&str>) -> Option<String> {
    let header = link?;
    for part in header.split(',') {
        let mut segs = part.split(';');
        let url_part = segs.next()?.trim();
        let is_next = segs.any(|s| s.trim() == r#"rel="next""#);
        if is_next {
            let url = url_part.trim_start_matches('<').trim_end_matches('>');
            return Some(url.to_owned());
        }
    }
    None
}

/// Scrubs any token-shaped substring from an operator-facing line.
///
/// Re-exported so the importer can route every diagnostic through one place.
#[must_use]
pub fn scrub_line(line: &str) -> String {
    scrubber::scrub(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_next_extracts_next_relation() {
        let header = r#"<https://api.github.com/x?page=2>; rel="next", <https://api.github.com/x?page=5>; rel="last""#;
        assert_eq!(
            link_next(Some(header)).as_deref(),
            Some("https://api.github.com/x?page=2")
        );
    }

    #[test]
    fn link_next_none_when_no_next() {
        let header = r#"<https://api.github.com/x?page=1>; rel="prev""#;
        assert_eq!(link_next(Some(header)), None);
        assert_eq!(link_next(None), None);
    }

    #[test]
    fn backoff_caps() {
        assert_eq!(backoff(0, 5, 60), Duration::from_secs(5));
        assert_eq!(backoff(1, 5, 60), Duration::from_secs(10));
        assert_eq!(backoff(10, 5, 60), Duration::from_secs(60));
        assert_eq!(backoff(0, 60, 600), Duration::from_secs(60));
    }

    #[test]
    fn scrub_line_redacts_token() {
        let tok = format!("ghp_{}", "A".repeat(40));
        let out = scrub_line(&format!("failed with {tok}"));
        assert!(!out.contains(&tok));
        assert!(out.contains("[REDACTED_GH_TOKEN]"));
    }

    #[test]
    fn page_path_appends_page_param() {
        assert_eq!(
            page_path("/repos/o/r/issues?state=all&per_page=100", 1),
            "/repos/o/r/issues?state=all&per_page=100&page=1"
        );
        assert_eq!(
            page_path("/repos/o/r/issues?state=all&per_page=100", 3),
            "/repos/o/r/issues?state=all&per_page=100&page=3"
        );
        // No existing query string → uses `?`.
        assert_eq!(
            page_path("/repos/o/r/labels", 2),
            "/repos/o/r/labels?page=2"
        );
    }
}
