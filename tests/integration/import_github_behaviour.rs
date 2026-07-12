//! Behaviour conformance tests for `eg import github` (issue #46).
//!
//! These tests drive the real `egregore import github` CLI against a tiny
//! in-process mock GitHub server built on `std::net::TcpListener`. No live
//! network access is used: the CLI is pointed at the mock via `--api-base`.
//!
//! The mock gives byte-level control over status codes, `ETags`, `Link`
//! pagination, and `X-RateLimit-*` headers, which is what the conformance
//! budget assertions (exact request counts, 304 re-import) require.
#![allow(missing_docs)]
#![allow(clippy::too_many_lines)]
// Test-harness ergonomics: the mock server favours readability over the
// nursery/pedantic rewrites clippy suggests here.
#![allow(clippy::option_if_let_else)]
#![allow(clippy::format_push_string)]
#![allow(clippy::needless_lifetimes)]

use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};

use assert_cmd::prelude::*;
use std::process::Command as StdCommand;
use tempfile::TempDir;

// ── Mock GitHub server ──────────────────────────────────────────────────────────

/// A canned response for one path (matched by the request path prefix).
#[derive(Clone)]
struct Canned {
    status: u16,
    body: String,
    etag: Option<String>,
    /// When set and the request carries this `If-None-Match`, reply 304.
    not_modified_when: Option<String>,
    /// Optional next-page path (no host); emitted as a `Link: …; rel="next"`.
    link_next_path: Option<String>,
}

impl Canned {
    fn ok(body: &str, etag: &str) -> Self {
        Self {
            status: 200,
            body: body.to_owned(),
            etag: Some(etag.to_owned()),
            not_modified_when: Some(etag.to_owned()),
            link_next_path: None,
        }
    }

    /// Like [`Canned::ok`] but advertises `next_path` as the next page.
    fn ok_with_next(body: &str, etag: &str, next_path: &str) -> Self {
        let mut c = Self::ok(body, etag);
        c.link_next_path = Some(next_path.to_owned());
        c
    }

    fn status_only(status: u16) -> Self {
        Self {
            status,
            body: "[]".to_owned(),
            etag: None,
            not_modified_when: None,
            link_next_path: None,
        }
    }
}

/// Records the path of every request received, in order.
type RequestLog = Arc<Mutex<Vec<String>>>;
/// Shared, swappable route table so one server (one stable base URL) can serve
/// several sequential imports with different responses.
type SharedRoutes = Arc<Mutex<HashMap<String, Canned>>>;

struct MockServer {
    base_url: String,
    requests: RequestLog,
    routes: SharedRoutes,
    handle: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicU64>,
}

impl MockServer {
    /// Starts a mock server. `routes` maps an exact request path (including
    /// query string) to its canned response. Unmatched paths return 404.
    fn start(routes: HashMap<String, Canned>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("addr");
        let base_url = format!("http://{addr}");
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let routes: SharedRoutes = Arc::new(Mutex::new(routes));
        let stop = Arc::new(AtomicU64::new(0));

        let req_clone = Arc::clone(&requests);
        let routes_clone = Arc::clone(&routes);
        let stop_clone = Arc::clone(&stop);
        listener
            .set_nonblocking(true)
            .expect("set nonblocking listener");

        let handle = thread::spawn(move || {
            loop {
                if stop_clone.load(Ordering::Relaxed) == 1 {
                    break;
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        handle_conn(&mut stream, &routes_clone, &req_clone);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            base_url,
            requests,
            routes,
            handle: Some(handle),
            stop,
        }
    }

    fn request_paths(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Replaces the route table (for the next import against this same server).
    fn set_routes(&self, routes: HashMap<String, Canned>) {
        *self.routes.lock().unwrap() = routes;
    }

    /// Clears the recorded request log (to count one import's requests cleanly).
    fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(1, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn handle_conn(stream: &mut std::net::TcpStream, routes: &SharedRoutes, requests: &RequestLog) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    // "GET /path HTTP/1.1"
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();

    // Read headers; capture If-None-Match and Content-Length.
    let mut if_none_match: Option<String> = None;
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            break;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("If-None-Match:") {
            if_none_match = Some(v.trim().to_owned());
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        let _ = reader.read_exact(&mut body);
    }

    requests.lock().unwrap().push(path.clone());

    // GitHub treats `page=1` as the default page, so a route registered without
    // an explicit page param also answers `…&page=1` / `…?page=1`. Normalize the
    // first page away before lookup so single-page fixtures keep matching.
    let lookup = normalize_first_page(&path);
    let response = {
        let table = routes.lock().unwrap();
        match table.get(&lookup) {
            Some(c) => {
                let send_304 = matches!((&c.not_modified_when, &if_none_match),
                    (Some(stored), Some(sent)) if stored == sent);
                if send_304 {
                    build_response(304, "", None, None)
                } else {
                    // The importer only checks for the presence of a rel="next"
                    // link to decide whether to continue paging; the host in the
                    // Link URL is never dereferenced, so a relative path suffices.
                    let link = c
                        .link_next_path
                        .as_deref()
                        .map(|p| format!("<http://mock{p}>; rel=\"next\""));
                    build_response(c.status, &c.body, c.etag.as_deref(), link.as_deref())
                }
            }
            // Issue #336: every changed PR now triggers a per-PR timeline fetch.
            // A fixture that does not care about review-state transitions need not
            // register a timeline route: an unmatched `/timeline` path defaults to
            // an empty array (no transitions), keeping pre-#336 fixtures green.
            // Tests that DO exercise transitions register an explicit route.
            None if lookup.contains("/timeline") => build_response(200, "[]", None, None),
            None => build_response(404, r#"{"message":"Not Found"}"#, None, None),
        }
    };
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Strips a trailing `page=1` (the GitHub default page) from a request path so
/// routes registered without an explicit page param still match page-1 probes.
fn normalize_first_page(path: &str) -> String {
    path.strip_suffix("&page=1")
        .or_else(|| path.strip_suffix("?page=1"))
        .unwrap_or(path)
        .to_owned()
}

fn build_response(status: u16, body: &str, etag: Option<&str>, link: Option<&str>) -> String {
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        404 => "Not Found",
        _ => "Status",
    };
    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\
         X-RateLimit-Remaining: 4999\r\nX-RateLimit-Reset: 9999999999\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(e) = etag {
        headers.push_str(&format!("ETag: {e}\r\n"));
    }
    if let Some(l) = link {
        headers.push_str(&format!("Link: {l}\r\n"));
    }
    headers.push_str("\r\n");
    headers.push_str(body);
    headers
}

// ── Fixtures ────────────────────────────────────────────────────────────────────

fn two_issues_json() -> String {
    serde_json::json!([
        {
            "number": 1, "title": "First issue", "body": "Body one.",
            "state": "open", "labels": [{"name":"bug","color":"f00"}],
            "assignees": [{"login":"alice"}], "user": {"login":"reporter"},
            "milestone": {"title":"M1"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/1"
        },
        {
            "number": 2, "title": "Second issue", "body": "Body two.",
            "state":"closed","state_reason":"completed","labels":[],
            "assignees":[],"user":{"login":"reporter2"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-03T00:00:00Z",
            "closed_at":"2026-01-03T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/2"
        }
    ])
    .to_string()
}

fn one_pull_json() -> String {
    serde_json::json!([
        {
            "number": 7, "title":"A pull request","body":"PR body.",
            "state":"open","draft":false,"labels":[],"assignees":[{"login":"dev"}],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "head":{"ref":"feature","sha":"abc123"},
            "base":{"ref":"main","sha":"def456"},
            "html_url":"https://github.com/o/r/pull/7"
        }
    ])
    .to_string()
}

fn three_comments_json() -> String {
    serde_json::json!([
        {
            "id": 101, "body":"First comment.", "user":{"login":"alice"},
            "issue_url":"https://api.github.com/repos/o/r/issues/1",
            "created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/1#issuecomment-101"
        },
        {
            "id": 102, "body":"Second comment.", "user":{"login":"bob"},
            "issue_url":"https://api.github.com/repos/o/r/issues/1",
            "created_at":"2026-01-02T01:00:00Z","updated_at":"2026-01-02T01:00:00Z",
            "html_url":"https://github.com/o/r/issues/1#issuecomment-102"
        },
        {
            "id": 103, "body":"Third comment.", "user":{"login":"alice"},
            "issue_url":"https://api.github.com/repos/o/r/issues/2",
            "created_at":"2026-01-03T00:00:00Z","updated_at":"2026-01-03T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/2#issuecomment-103"
        }
    ])
    .to_string()
}

/// One threaded review discussion: A (root), B (reply to A), C (reply to B).
fn threaded_review_comments_json() -> String {
    serde_json::json!([
        {
            "id": 201, "body":"Root review comment.", "user":{"login":"reviewer"},
            "path":"src/lib.rs","line":10,"side":"RIGHT","diff_hunk":"@@ -1 +1 @@",
            "pull_request_url":"https://api.github.com/repos/o/r/pulls/7",
            "commit_id":"abc123",
            "created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/pull/7#discussion_r201"
        },
        {
            "id": 202, "body":"Reply B.", "user":{"login":"author"},
            "path":"src/lib.rs","line":10,"in_reply_to_id":201,
            "pull_request_url":"https://api.github.com/repos/o/r/pulls/7",
            "created_at":"2026-01-02T01:00:00Z","updated_at":"2026-01-02T01:00:00Z",
            "html_url":"https://github.com/o/r/pull/7#discussion_r202"
        },
        {
            "id": 203, "body":"Reply C.", "user":{"login":"reviewer"},
            "path":"src/lib.rs","line":10,"in_reply_to_id":202,
            "pull_request_url":"https://api.github.com/repos/o/r/pulls/7",
            "created_at":"2026-01-02T02:00:00Z","updated_at":"2026-01-02T02:00:00Z",
            "html_url":"https://github.com/o/r/pull/7#discussion_r203"
        }
    ])
    .to_string()
}

fn one_pr_review_json() -> String {
    serde_json::json!([
        {
            "id": 301, "body":"Looks good overall.", "state":"APPROVED",
            "user":{"login":"reviewer"},"submitted_at":"2026-01-02T03:00:00Z",
            "html_url":"https://github.com/o/r/pull/7#pullrequestreview-301"
        }
    ])
    .to_string()
}

/// Builds the full route table for the canonical fixture.
fn full_routes() -> HashMap<String, Canned> {
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok(&two_issues_json(), "\"issues-v1\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&one_pull_json(), "\"pulls-v1\""),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[{\"name\":\"bug\",\"color\":\"f00\"}]", "\"labels-v1\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok(&three_comments_json(), "\"ic-v1\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok(&threaded_review_comments_json(), "\"prc-v1\""),
    );
    routes.insert(
        "/repos/o/r/pulls/7/reviews?per_page=100".to_owned(),
        Canned::ok(&one_pr_review_json(), "\"prr-v1\""),
    );
    routes
}

// ── Test helpers ──────────────────────────────────────────────────────────────

fn egregore() -> StdCommand {
    StdCommand::cargo_bin("egregore").expect("egregore binary should build")
}

/// Runs `import github o/r` against `base_url`, returning (handoff JSONL, stderr).
fn run_import(
    base_url: &str,
    out: &std::path::Path,
    state: &std::path::Path,
    extra: &[&str],
) -> (String, String, bool) {
    let mut args: Vec<String> = vec![
        "import".into(),
        "github".into(),
        "o/r".into(),
        "--out".into(),
        out.to_str().unwrap().into(),
        "--state-file".into(),
        state.to_str().unwrap().into(),
        "--api-base".into(),
        base_url.into(),
        "--transaction-time".into(),
        "2026-02-01T00:00:00Z".into(),
        "--no-backoff".into(),
    ];
    for e in extra {
        args.push((*e).into());
    }
    // Ensure no ambient token leaks in; force anonymous unless a fixture sets it.
    let output = egregore()
        .args(&args)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("PATH")
        .env_remove("Path")
        .output()
        .expect("run import");
    let jsonl = std::fs::read_to_string(out).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    (jsonl, stderr, output.status.success())
}

fn nodes_of_kind(jsonl: &str, kind: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["record_type"] == "node" && v["kind"] == kind)
        .collect()
}

fn edges_of_label(jsonl: &str, label: &str) -> usize {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["record_type"] == "edge" && v["label"] == label)
        .count()
}

/// All edge records carrying `label`, as JSON values (issue #336 helpers).
fn edge_values(jsonl: &str, label: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["record_type"] == "edge" && v["label"] == label)
        .collect()
}

/// The single `pr_review`-kind Review node in a handoff JSONL (issue #336).
fn pr_review_node(jsonl: &str) -> serde_json::Value {
    let mut found: Vec<serde_json::Value> = nodes_of_kind(jsonl, "Review")
        .into_iter()
        .filter(|v| v["review_kind"] == "pr_review")
        .collect();
    assert_eq!(found.len(), 1, "expected exactly one pr_review Review node");
    found.pop().unwrap()
}

// ── Issue #336: review-state transition history fixtures ─────────────────────────

/// A PR whose `updated_at` advanced (so a re-import sees `pulls_changed`).
fn one_pull_updated_json() -> String {
    serde_json::json!([
        {
            "number": 7, "title":"A pull request","body":"PR body.",
            "state":"open","draft":false,"labels":[],"assignees":[{"login":"dev"}],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-05T00:00:00Z",
            "head":{"ref":"feature","sha":"abc123"},
            "base":{"ref":"main","sha":"def456"},
            "html_url":"https://github.com/o/r/pull/7"
        }
    ])
    .to_string()
}

/// Review 301 in its post-dismissal state (`DISMISSED`).
fn one_pr_review_dismissed_json() -> String {
    serde_json::json!([
        {
            "id": 301, "body":"Looks good overall.", "state":"DISMISSED",
            "user":{"login":"reviewer"},"submitted_at":"2026-01-02T03:00:00Z",
            "html_url":"https://github.com/o/r/pull/7#pullrequestreview-301"
        }
    ])
    .to_string()
}

/// The PR-7 timeline carrying the dismissal of review 301.
fn timeline_with_dismissal_json() -> String {
    serde_json::json!([
        { "event": "labeled", "id": 4000, "created_at": "2026-01-04T00:00:00Z" },
        {
            "event": "review_dismissed", "id": 5001,
            "created_at": "2026-01-04T12:00:00Z",
            "actor": {"login": "maintainer"},
            "dismissed_review": {
                "review_id": 301, "state": "dismissed",
                "dismissal_message": "stale after force-push"
            }
        }
    ])
    .to_string()
}

/// The canonical fixture plus an explicit PR-7 timeline route carrying the
/// dismissal, with the PR and its review already in the dismissed state.
fn routes_with_dismissal() -> HashMap<String, Canned> {
    let mut routes = full_routes();
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&one_pull_updated_json(), "\"pulls-v2\""),
    );
    routes.insert(
        "/repos/o/r/pulls/7/reviews?per_page=100".to_owned(),
        Canned::ok(&one_pr_review_dismissed_json(), "\"prr-v2\""),
    );
    routes.insert(
        "/repos/o/r/issues/7/timeline?per_page=100".to_owned(),
        Canned::ok(&timeline_with_dismissal_json(), "\"tl-v2\""),
    );
    routes
}

// ── AC1/AC2/AC8/AC9: two-snapshot dismissal preserves the approval ───────────────

#[test]
fn dismissal_snapshot_records_transition_without_erasing_the_approval() {
    // AC1: import the pre-dismissal snapshot, then the post-dismissal snapshot.
    // The Review keeps its stable record id (identity unchanged) with current
    // review_state "dismissed", PLUS one ReviewStateTransition for the dismissal
    // (valid_time = the timeline event's created_at, actor login, transition kind
    // review_dismissed) and a TRANSITIONS_REVIEW edge to that Review.
    let server = MockServer::start(full_routes());
    let dir = TempDir::new().expect("temp dir");
    let out = dir.path().join("graph.jsonl");
    let state = dir.path().join("state.json");

    // Pre-dismissal snapshot: review 301 is APPROVED, timeline empty.
    let (pre_jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok, "pre-dismissal import should succeed");
    let pre_review = pr_review_node(&pre_jsonl);
    let review_id = pre_review["id"].as_str().unwrap().to_owned();
    assert_eq!(
        pre_review["review_state"], "approved",
        "pre-state is approved"
    );
    assert_eq!(
        nodes_of_kind(&pre_jsonl, "ReviewStateTransition").len(),
        0,
        "no transition before the dismissal"
    );

    // Post-dismissal snapshot: review 301 DISMISSED + a review_dismissed timeline
    // event. The PR's updated_at advanced so the per-PR review + timeline fetches
    // fire (same `pulls_changed` gate).
    server.set_routes(routes_with_dismissal());
    let (post_jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok, "post-dismissal import should succeed");

    // The Review re-emits with current review_state "dismissed" and the SAME id —
    // the original approval's record id is unchanged in identity.
    let post_review = pr_review_node(&post_jsonl);
    assert_eq!(
        post_review["id"].as_str().unwrap(),
        review_id,
        "the Review record id is unchanged by the dismissal"
    );
    assert_eq!(
        post_review["review_state"], "dismissed",
        "review_state is now the last-write-wins dismissed summary"
    );

    // Exactly one ReviewStateTransition for the dismissal, fully attributed.
    let transitions = nodes_of_kind(&post_jsonl, "ReviewStateTransition");
    assert_eq!(transitions.len(), 1, "one dismissal transition");
    let t = &transitions[0];
    assert_eq!(t["transition_kind"], "review_dismissed");
    assert_eq!(t["author"], "maintainer", "actor login");
    assert_eq!(
        t["valid_time"], "2026-01-04T12:00:00Z",
        "valid_time is the timeline event's created_at"
    );
    // AC8: citable via system_native_id timeline:<id>.
    assert_eq!(t["system_native_id"], "timeline:5001");

    // A TRANSITIONS_REVIEW edge binds the transition to the dismissed Review.
    let edges = edge_values(&post_jsonl, "TRANSITIONS_REVIEW");
    assert_eq!(edges.len(), 1, "one TRANSITIONS_REVIEW edge");
    assert_eq!(
        edges[0]["source"].as_str().unwrap(),
        t["id"].as_str().unwrap()
    );
    assert_eq!(
        edges[0]["target"].as_str().unwrap(),
        review_id,
        "the edge targets the dismissed Review, its approval identity intact"
    );

    // AC9 (load-bearing): the transition's valid_time is strictly after the
    // review's, so a pre-dismissal valid-time window still finds the approval and
    // no dismissal; the dismissal never erases the earlier approval evidence.
    let review_vt = post_review["valid_time"].as_str().unwrap();
    let trans_vt = t["valid_time"].as_str().unwrap();
    assert!(
        review_vt < trans_vt,
        "approval valid_time {review_vt} precedes dismissal valid_time {trans_vt}"
    );
}

#[test]
fn dismissal_import_is_byte_identical_across_five_runs() {
    // AC2/determinism: a fresh import carrying the dismissal is byte-identical
    // across five runs (transition ids seed from the timeline event id).
    let server = MockServer::start(routes_with_dismissal());
    let mut prev: Option<String> = None;
    for _ in 0..5 {
        let dir = TempDir::new().expect("temp dir");
        let out = dir.path().join("graph.jsonl");
        let state = dir.path().join("state.json");
        let (jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
        assert!(ok, "import should succeed");
        // The dismissal transition is present on a fresh import too.
        assert_eq!(nodes_of_kind(&jsonl, "ReviewStateTransition").len(), 1);
        if let Some(p) = &prev {
            assert_eq!(
                *p, jsonl,
                "import output must be byte-identical across runs"
            );
        }
        prev = Some(jsonl);
    }
}

#[test]
fn unknown_timeline_event_kinds_are_filtered_without_diagnostic() {
    // AC3: an out-of-scope timeline kind (labeled) is skipped by the filter and
    // never becomes a transition or a diagnostic; only the 3 closed kinds count.
    let server = MockServer::start(routes_with_dismissal());
    let dir = TempDir::new().expect("temp dir");
    let out = dir.path().join("graph.jsonl");
    let state = dir.path().join("state.json");
    let (jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);
    // The fixture timeline also carries a `labeled` event; exactly one transition
    // (the dismissal) is recorded, and no timeline diagnostic is emitted.
    assert_eq!(nodes_of_kind(&jsonl, "ReviewStateTransition").len(), 1);
    assert!(
        !jsonl.contains("github_timeline_event_unparseable"),
        "a well-formed timeline emits no unparseable diagnostic"
    );
}

// ── AC1 / AC2 / Success metric: fresh import shapes + source handles ─────────────

#[test]
fn fresh_import_emits_tasks_links_reviews_with_source_handles() {
    let server = MockServer::start(full_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");

    let start = std::time::Instant::now();
    let (jsonl, stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok, "import should succeed; stderr={stderr}");
    // Success metric: under 5 seconds.
    assert!(start.elapsed().as_secs() < 5, "import should finish < 5s");

    // 2 issues + 1 PR → 3 Task, 3 ExternalLink.
    let tasks = nodes_of_kind(&jsonl, "Task");
    let links = nodes_of_kind(&jsonl, "ExternalLink");
    assert_eq!(tasks.len(), 3, "expected 3 Task nodes");
    assert_eq!(links.len(), 3, "expected 3 ExternalLink nodes");

    // 3 issue comments + 3 review comments + 1 PR review = 7 Review nodes.
    let reviews = nodes_of_kind(&jsonl, "Review");
    if reviews.len() != 7 {
        let kinds: Vec<String> = reviews
            .iter()
            .map(|r| r["review_kind"].as_str().unwrap_or("?").to_owned())
            .collect();
        std::fs::write(
            "/tmp/dbg_reviews.txt",
            format!("paths={:#?}\nkinds={kinds:?}\n", server.request_paths()),
        )
        .ok();
    }
    assert_eq!(reviews.len(), 7, "expected 7 Review nodes");

    // One EXTERNAL_HANDLE per Task; one REFERENCES_TASK per Review.
    assert_eq!(edges_of_label(&jsonl, "EXTERNAL_HANDLE"), 3);
    assert_eq!(edges_of_label(&jsonl, "REFERENCES_TASK"), 7);

    // AC2: source handles preserved on every work/review record.
    for task in &tasks {
        assert!(task["source_kind"].is_string(), "Task carries source_kind");
        assert!(task["entity_id"].is_string(), "Task carries entity_id");
        assert!(
            task["source_external_link_id"].is_string(),
            "Task links to ExternalLink"
        );
    }
    for link in &links {
        assert_eq!(link["system"], "github");
        assert!(link["system_native_id"].is_string());
        assert!(link["url"].is_string());
    }
    for review in &reviews {
        assert!(
            review["review_kind"].is_string(),
            "Review carries review_kind"
        );
        assert!(
            review["system_native_id"].is_string(),
            "Review carries source native id"
        );
    }

    // Labels/assignees present and a milestone round-tripped into body blob.
    let issue1 = tasks
        .iter()
        .find(|t| {
            t["summary"]
                .as_str()
                .unwrap_or("")
                .contains("github_issue #1")
        })
        .expect("issue 1 task");
    assert_eq!(issue1["labels"][0], "bug");
    assert_eq!(issue1["assignees"][0], "alice");
    assert_eq!(issue1["author"], "reporter");
    let body_inline = issue1["body_handle"]["inline"].as_str().unwrap_or("");
    assert!(
        body_inline.contains("M1"),
        "milestone title round-trips into body"
    );

    // Per-run stderr summary fields (AC of #46 / policy §4).
    assert!(stderr.contains("egregore-github-import:"));
    assert!(stderr.contains("requests="));
    assert!(stderr.contains("quota_remaining="));
    assert!(stderr.contains("elapsed="));
}

// ── AC3: 5-run unchanged re-import is byte-stable + zero duplicates ───────────────

#[test]
fn unchanged_reimport_is_byte_stable_and_creates_no_duplicates() {
    let server = MockServer::start(full_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");

    let (first, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);
    let first_work_records = nodes_of_kind(&first, "Task").len()
        + nodes_of_kind(&first, "ExternalLink").len()
        + nodes_of_kind(&first, "Review").len();
    assert!(first_work_records > 0);

    // Re-import 5 times; each must be byte-identical and contain zero work records.
    for run in 0..5 {
        let (again, _, ok) = run_import(&server.base_url, &out, &state, &[]);
        assert!(ok, "re-import run {run} should succeed");
        // Conditional 304s mean no work records re-emitted (only the handoff).
        assert_eq!(
            nodes_of_kind(&again, "Task").len(),
            0,
            "run {run}: zero duplicate Task records"
        );
        assert_eq!(nodes_of_kind(&again, "ExternalLink").len(), 0);
        assert_eq!(nodes_of_kind(&again, "Review").len(), 0);
        // Only the handoff Diagnostic record remains.
        let diags = nodes_of_kind(&again, "Diagnostic");
        assert_eq!(diags.len(), 1, "run {run}: only handoff record present");
    }

    // Two consecutive re-imports are byte-for-byte identical.
    let (run_a, _, _) = run_import(&server.base_url, &out, &state, &[]);
    let (run_b, _, _) = run_import(&server.base_url, &out, &state, &[]);
    assert_eq!(run_a, run_b, "unchanged re-imports are byte-stable");
}

// ── AC4: changed issue re-import emits only the changed record ────────────────────

#[test]
fn changed_issue_reimport_emits_only_changed_records() {
    // One server (stable base URL) serves both imports, so the persisted state
    // is reused across runs.
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let server = MockServer::start(full_routes());
    let (_, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // Second import: issue #2 changed (new ETag + new body + later updated_at);
    // everything else returns its prior ETag (→ 304).
    let mut routes = full_routes();
    let changed_issues = serde_json::json!([
        {
            "number": 1, "title": "First issue", "body": "Body one.",
            "state": "open", "labels": [{"name":"bug","color":"f00"}],
            "assignees": [{"login":"alice"}], "user": {"login":"reporter"},
            "milestone": {"title":"M1"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/1"
        },
        {
            "number": 2, "title": "Second issue EDITED", "body": "Body two changed.",
            "state":"closed","state_reason":"completed","labels":[],
            "assignees":[],"user":{"login":"reporter2"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-02-05T00:00:00Z",
            "closed_at":"2026-02-05T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/2"
        }
    ])
    .to_string();
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok(&changed_issues, "\"issues-v2\""), // new ETag → 200 not 304
    );
    server.set_routes(routes);

    let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // Only issue #2's Task + ExternalLink should be re-emitted; issue #1 unchanged.
    let tasks = nodes_of_kind(&jsonl, "Task");
    assert_eq!(tasks.len(), 1, "only the changed issue re-emits a Task");
    assert!(
        tasks[0]["summary"].as_str().unwrap().contains("#2"),
        "the re-emitted Task is issue #2"
    );
    assert_eq!(nodes_of_kind(&jsonl, "ExternalLink").len(), 1);
}

// ── AC5: token-shaped strings are redacted everywhere ────────────────────────────

#[test]
fn token_strings_are_redacted_in_output_and_readback() {
    let mut routes = full_routes();
    let raw_token_a = format!("ghp_{}", "A".repeat(40));
    let raw_token_b = format!("github_pat_{}", "B".repeat(40));
    let issues_with_tokens = serde_json::json!([
        {
            "number": 1, "title": format!("Leak {raw_token_a}"),
            "body": format!("Config has {raw_token_a} and {raw_token_b} embedded."),
            "state":"open","labels":[],"assignees":[],"user":{"login":"x"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/1"
        }
    ])
    .to_string();
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok(&issues_with_tokens, "\"issues-tok\""),
    );

    let server = MockServer::start(routes);
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // Zero raw token strings in generated output.
    assert!(
        !jsonl.contains(&raw_token_a),
        "raw ghp_ token must not persist"
    );
    assert!(
        !jsonl.contains(&raw_token_b),
        "raw github_pat_ token must not persist"
    );
    assert!(
        !stderr.contains(&raw_token_a),
        "raw token must not reach stderr"
    );
    // The schema redaction marker is present in the persisted record.
    assert!(
        jsonl.contains("<REDACTED:api_token:"),
        "schema marker present"
    );

    // Read-back through inspect must not surface raw tokens either.
    let inspect = egregore()
        .args(["inspect", out.to_str().unwrap()])
        .output()
        .expect("inspect");
    let inspect_out = String::from_utf8_lossy(&inspect.stdout);
    assert!(!inspect_out.contains(&raw_token_a));
    assert!(!inspect_out.contains(&raw_token_b));
}

// ── AC6: failure modes produce stable diagnostics, no token echo, no state ───────

#[test]
fn auth_missing_exits_with_stable_code_and_no_state_file() {
    // Repo probe returns 404 and no token is available → github_auth_missing.
    let mut routes = HashMap::new();
    routes.insert("/repos/o/r".to_owned(), Canned::status_only(404));

    let server = MockServer::start(routes);
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (_, stderr, ok) = run_import(&server.base_url, &out, &state, &[]);

    assert!(!ok, "auth-missing must exit non-zero");
    assert!(
        stderr.contains("github_auth_missing"),
        "stable code present: {stderr}"
    );
    assert!(
        !state.exists(),
        "no state file written on auth failure (policy §5)"
    );
    assert!(!out.exists(), "no partial handoff written on failure");
}

#[test]
fn invalid_repo_arg_exits_with_stable_code() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let output = egregore()
        .args([
            "import",
            "github",
            "not-a-valid-repo-slug",
            "--out",
            out.to_str().unwrap(),
            "--state-file",
            state.to_str().unwrap(),
            "--api-base",
            "http://127.0.0.1:1", // never contacted
        ])
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .output()
        .expect("run");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("github_invalid_repo_arg"),
        "stderr={stderr}"
    );
}

// ── AC7: file-anchored review comments link to seeded code-graph files ────────────

#[test]
fn review_comment_links_to_seeded_file_or_diagnoses_missing() {
    // Seed a code-graph JSONL with one File record for src/lib.rs.
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    let file_record = serde_json::json!({
        "record_type":"node","id":"codegraph:v4:file-librs","kind":"File",
        "schema_version":4,"repo_relative_path":"src/lib.rs","summary":"Rust source file src/lib.rs"
    });
    std::fs::write(&code_graph, format!("{file_record}\n")).unwrap();

    let server = MockServer::start(full_routes());
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok);

    // The three threaded review comments all anchor to src/lib.rs (which exists)
    // → at least one TOUCHES_FILE edge to the seeded File record.
    assert!(
        edges_of_label(&jsonl, "TOUCHES_FILE") >= 1,
        "unambiguous file link emits TOUCHES_FILE"
    );
    let touches_target_ok = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v["record_type"] == "edge"
                && v["label"] == "TOUCHES_FILE"
                && v["target"] == "codegraph:v4:file-librs"
        });
    assert!(touches_target_ok, "edge targets the seeded File record id");
}

#[test]
fn review_comment_missing_file_emits_diagnostic_not_guess() {
    // No code-graph seeded → src/lib.rs cannot resolve → diagnostics, no guess.
    let server = MockServer::start(full_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    assert_eq!(
        edges_of_label(&jsonl, "TOUCHES_FILE"),
        0,
        "no file links guessed without a seeded store"
    );
    let unresolved = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            v["record_type"] == "node"
                && v["kind"] == "Diagnostic"
                && v["summary"]
                    .as_str()
                    .unwrap_or("")
                    .contains("github_file_unresolved")
        })
        .count();
    assert!(
        unresolved >= 1,
        "missing file produces a diagnostic with handles"
    );
}

// ── PR review thread reconstructs via in_reply_to_id (issue #46 AC) ───────────────

#[test]
fn pr_review_thread_reconstructs_via_in_reply_to_id() {
    let server = MockServer::start(full_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    let comments: Vec<serde_json::Value> = nodes_of_kind(&jsonl, "Review")
        .into_iter()
        .filter(|v| v["review_kind"] == "pr_review_comment")
        .collect();
    assert_eq!(comments.len(), 3, "three threaded review comments");

    // Exactly one root (no in_reply_to_id); the other two carry one.
    let roots = comments
        .iter()
        .filter(|v| v.get("in_reply_to_id").is_none() || v["in_reply_to_id"].is_null())
        .count();
    assert_eq!(roots, 1, "exactly one thread root");
    let replies = comments
        .iter()
        .filter(|v| v["in_reply_to_id"].is_string())
        .count();
    assert_eq!(replies, 2, "two replies carry in_reply_to_id");
}

// ── AC8 (local-first): unchanged re-import issues only conditional probes ─────────

#[test]
fn unchanged_reimport_sends_only_conditional_probes() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");

    // One server so the second run reuses the persisted ETags.
    let server = MockServer::start(full_routes());
    let (_, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // Re-import: every endpoint returns 304. Count only this run's requests.
    server.clear_requests();
    let (_, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    let paths = server.request_paths();
    // 1 repo probe + one conditional probe per stored single-page endpoint.
    // Endpoints: issues, pulls, labels, issue comments, pr review comments.
    // (Per-PR reviews fire only when the pulls list changed; here pulls=304.)
    assert!(paths.iter().any(|p| p == "/repos/o/r"), "repo probe issued");
    assert!(
        paths
            .iter()
            .any(|p| p.starts_with("/repos/o/r/issues?state=all&per_page=100")),
        "issues probe issued"
    );
    // No per-PR review fetch on an unchanged re-import.
    assert!(
        !paths.iter().any(|p| p.contains("/pulls/7/reviews")),
        "no per-PR review fetch when pulls unchanged"
    );
    // Budget: exactly one request per endpoint (no duplicate/extra page probes).
    // 1 repo + 5 active endpoints (issues, pulls, labels, issue comments, pr
    // review comments) = 6 total on a single-page unchanged re-import.
    assert_eq!(paths.len(), 6, "unchanged re-import budget: got {paths:?}");
}

// ── Multi-page ETag probing: a 304 on page 1 must NOT skip a changed page 2 ───────

#[test]
fn paginated_reimport_probes_every_stored_page() {
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");

    // Build a two-page /issues fixture. Page 1 advertises a `next` link to page 2.
    let issue = |n: u64, updated: &str| {
        serde_json::json!({
            "number": n, "title": format!("Issue {n}"), "body": "b",
            "state":"open","labels":[],"assignees":[],"user":{"login":"u"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":updated,
            "html_url":format!("https://github.com/o/r/issues/{n}")
        })
    };
    let page1_v1 = serde_json::Value::Array(vec![issue(1, "2026-01-02T00:00:00Z")]).to_string();
    let page2_v1 = serde_json::Value::Array(vec![issue(2, "2026-01-02T00:00:00Z")]).to_string();

    let page1_key = "/repos/o/r/issues?state=all&per_page=100".to_owned();
    let page2_key = "/repos/o/r/issues?state=all&per_page=100&page=2".to_owned();
    let next_path = "/repos/o/r/issues?state=all&per_page=100&page=2";

    let mut routes = full_routes();
    // Replace the single-page issues route with two explicit pages; page 1
    // advertises a rel="next" link to page 2.
    routes.remove("/repos/o/r/issues?state=all&per_page=100");
    routes.insert(
        page1_key.clone(),
        Canned::ok_with_next(&page1_v1, "\"issues-p1-v1\"", next_path),
    );
    routes.insert(page2_key.clone(), Canned::ok(&page2_v1, "\"issues-p2-v1\""));

    let server = MockServer::start(routes);
    let (first, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);
    // Both pages' issues imported on the first run. full_routes() also has one
    // PR (#7), so count issue Tasks specifically: #1 (page 1) + #2 (page 2).
    let issue_tasks = |jsonl: &str| {
        nodes_of_kind(jsonl, "Task")
            .into_iter()
            .filter(|t| t["source_kind"] == "github_issue")
            .count()
    };
    assert_eq!(issue_tasks(&first), 2, "first import: both pages' issues");

    // Second import: page 1 unchanged (same ETag → 304), page 2 CHANGED (new
    // ETag + issue #2 updated). The importer must still probe page 2 and emit
    // only issue #2.
    let page2_v2 = serde_json::Value::Array(vec![issue(2, "2026-03-01T00:00:00Z")]).to_string();
    let mut routes2 = full_routes();
    routes2.remove("/repos/o/r/issues?state=all&per_page=100");
    routes2.insert(
        page1_key,
        // Same ETag → 304 (its rel="next" cannot be read from a 304, so
        // continuation relies on the stored page-2 ETag in state).
        Canned::ok_with_next(&page1_v1, "\"issues-p1-v1\"", next_path),
    );
    routes2.insert(page2_key, Canned::ok(&page2_v2, "\"issues-p2-v2\"")); // new ETag → 200
    server.set_routes(routes2);
    server.clear_requests();

    let (second, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // Page 2 MUST have been probed even though page 1 returned 304.
    let paths = server.request_paths();
    assert!(
        paths.iter().any(|p| p.contains("page=2")),
        "page 2 must be probed despite page-1 304: {paths:?}"
    );
    // Only the changed issue (#2) re-emits a Task; issue #1 (page 1) does not.
    // (All other endpoints return 304, so the only issue Task is #2.)
    let issue_task_summaries: Vec<String> = nodes_of_kind(&second, "Task")
        .into_iter()
        .filter(|t| t["source_kind"] == "github_issue")
        .map(|t| t["summary"].as_str().unwrap_or("").to_owned())
        .collect();
    assert_eq!(
        issue_task_summaries.len(),
        1,
        "only the changed page's issue re-emits: {issue_task_summaries:?}"
    );
    assert!(issue_task_summaries[0].contains("#2"));
}

// ── Issue #333: PR head/base/merge fields promoted to first-class Task fields ─────

/// A six-PR fixture: three merged with DISTINCT `merge_commit_sha`s, one
/// closed-unmerged, one draft, one open.
fn six_pulls_json() -> String {
    serde_json::json!([
        {
            "number": 10, "title":"Merged A","body":"PR body A.",
            "state":"closed","draft":false,"labels":[],"assignees":[{"login":"dev"}],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "merged_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-02T00:00:00Z",
            "head":{"ref":"feature-a","sha":"headsha000000000000000000000000000000a10"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b10"},
            "merge_commit_sha":"mergeaaa1111111111111111111111111111111a",
            "html_url":"https://github.com/o/r/pull/10"
        },
        {
            "number": 11, "title":"Merged B","body":"PR body B.",
            "state":"closed","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T01:00:00Z",
            "merged_at":"2026-01-02T01:00:00Z","closed_at":"2026-01-02T01:00:00Z",
            "head":{"ref":"feature-b","sha":"headsha000000000000000000000000000000b11"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b11"},
            "merge_commit_sha":"mergebbb2222222222222222222222222222222b",
            "html_url":"https://github.com/o/r/pull/11"
        },
        {
            "number": 12, "title":"Merged C","body":"PR body C.",
            "state":"closed","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T02:00:00Z",
            "merged_at":"2026-01-02T02:00:00Z","closed_at":"2026-01-02T02:00:00Z",
            "head":{"ref":"feature-c","sha":"headsha000000000000000000000000000000c12"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b12"},
            "merge_commit_sha":"mergeccc3333333333333333333333333333333c",
            "html_url":"https://github.com/o/r/pull/12"
        },
        {
            "number": 13, "title":"Closed unmerged","body":"PR body D.",
            "state":"closed","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T03:00:00Z",
            "closed_at":"2026-01-02T03:00:00Z",
            "head":{"ref":"feature-d","sha":"headsha000000000000000000000000000000d13"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b13"},
            "html_url":"https://github.com/o/r/pull/13"
        },
        {
            "number": 14, "title":"Draft PR","body":"PR body E.",
            "state":"open","draft":true,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T04:00:00Z",
            "head":{"ref":"feature-e","sha":"headsha000000000000000000000000000000e14"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b14"},
            "html_url":"https://github.com/o/r/pull/14"
        },
        {
            "number": 15, "title":"Open PR","body":"PR body F.",
            "state":"open","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T05:00:00Z",
            "head":{"ref":"feature-f","sha":"headsha000000000000000000000000000000f15"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b15"},
            "html_url":"https://github.com/o/r/pull/15"
        }
    ])
    .to_string()
}

/// Route table for the six-PR fixture: empty everything except pulls, with an
/// empty reviews endpoint for each PR (so the per-PR review fetch never 404s).
fn six_pr_routes() -> HashMap<String, Canned> {
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&six_pulls_json(), "\"pulls-333\""),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"ic-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"prc-empty\""),
    );
    for n in 10..=15 {
        routes.insert(
            format!("/repos/o/r/pulls/{n}/reviews?per_page=100"),
            Canned::ok("[]", &format!("\"prr-{n}\"")),
        );
    }
    routes
}

/// Finds the single `github_pr` Task whose summary is `github_pr #<number>`.
fn pr_task(jsonl: &str, number: u64) -> serde_json::Value {
    nodes_of_kind(jsonl, "Task")
        .into_iter()
        .find(|t| {
            t["source_kind"] == "github_pr"
                && t["summary"]
                    .as_str()
                    .unwrap_or("")
                    .contains(&format!("#{number}"))
        })
        .unwrap_or_else(|| panic!("PR Task #{number} should exist"))
}

/// A code-graph JSONL seed with one `Commit` node per given SHA.
fn commit_seed(shas: &[&str]) -> String {
    let mut out = String::new();
    for (i, sha) in shas.iter().enumerate() {
        let rec = serde_json::json!({
            "record_type":"node","id":format!("codegraph:v5:commit-{i}"),
            "kind":"Commit","schema_version":5,"name":sha,
            "summary":format!("Git commit {sha}")
        });
        out.push_str(&rec.to_string());
        out.push('\n');
    }
    out
}

#[test]
fn pr_tasks_promote_six_flat_fields_and_issues_omit_them() {
    let server = MockServer::start(six_pr_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // Merged PR #10: all six fields present with the recorded values.
    let pr10 = pr_task(&jsonl, 10);
    assert_eq!(pr10["head_sha"], "headsha000000000000000000000000000000a10");
    assert_eq!(pr10["head_ref"], "feature-a");
    assert_eq!(pr10["base_ref"], "main");
    assert_eq!(
        pr10["merge_commit_sha"],
        "mergeaaa1111111111111111111111111111111a"
    );
    assert_eq!(pr10["merged_at"], "2026-01-02T00:00:00Z");
    assert_eq!(pr10["draft"], false);

    // Draft PR #14: draft=true, no merge_commit_sha / merged_at (serde-skipped).
    let pr14 = pr_task(&jsonl, 14);
    assert_eq!(pr14["draft"], true);
    assert_eq!(pr14["head_ref"], "feature-e");
    assert!(pr14.get("merge_commit_sha").is_none() || pr14["merge_commit_sha"].is_null());
    assert!(pr14.get("merged_at").is_none() || pr14["merged_at"].is_null());

    // Open PR #15: no merge fields, draft=false, head/base refs present.
    let pr15 = pr_task(&jsonl, 15);
    assert_eq!(pr15["draft"], false);
    assert!(pr15.get("merge_commit_sha").is_none() || pr15["merge_commit_sha"].is_null());

    // Closed-unmerged PR #13: no merge_commit_sha / merged_at.
    let pr13 = pr_task(&jsonl, 13);
    assert!(pr13.get("merge_commit_sha").is_none() || pr13["merge_commit_sha"].is_null());
    assert!(pr13.get("merged_at").is_none() || pr13["merged_at"].is_null());

    // Issue Tasks (there are none in this fixture) never carry the fields; add
    // a mixed run to prove issue omission using the canonical fixture.
    let server2 = MockServer::start(full_routes());
    let tmp2 = TempDir::new().unwrap();
    let out2 = tmp2.path().join("g.jsonl");
    let state2 = tmp2.path().join("s.json");
    let (jsonl2, _, ok2) = run_import(&server2.base_url, &out2, &state2, &[]);
    assert!(ok2);
    for issue_task in nodes_of_kind(&jsonl2, "Task")
        .into_iter()
        .filter(|t| t["source_kind"] == "github_issue")
    {
        for field in [
            "head_sha",
            "head_ref",
            "base_ref",
            "merge_commit_sha",
            "merged_at",
            "draft",
        ] {
            assert!(
                issue_task.get(field).is_none() || issue_task[field].is_null(),
                "issue Task must omit PR field {field}: {issue_task}"
            );
        }
    }
}

#[test]
fn pr_promoted_fields_are_byte_identical_across_five_reimports() {
    let tmp = TempDir::new().unwrap();
    let mut outputs = Vec::new();
    for i in 0..5 {
        let server = MockServer::start(six_pr_routes());
        let out = tmp.path().join(format!("graph-{i}.jsonl"));
        let state = tmp.path().join(format!("state-{i}.json"));
        let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
        assert!(ok, "import {i} should succeed");
        outputs.push(jsonl);
    }
    for (i, jsonl) in outputs.iter().enumerate().skip(1) {
        assert_eq!(
            *jsonl, outputs[0],
            "re-import {i} must be byte-identical to the first"
        );
    }
    // The promoted fields are actually present in the stable output.
    assert!(outputs[0].contains("\"head_sha\":\"headsha000000000000000000000000000000a10\""));
    assert!(
        outputs[0].contains("\"merge_commit_sha\":\"mergeaaa1111111111111111111111111111111a\"")
    );
}

#[test]
fn pr_promotion_is_additive_schema_v1_with_stable_ids() {
    // First import establishes the Task IDs.
    let server = MockServer::start(six_pr_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    // PROJECT_SCHEMA_VERSION stays 1 on every PR Task.
    for n in 10..=15 {
        let t = pr_task(&jsonl, n);
        assert_eq!(t["schema_version"], 1, "project schema version stays 1");
    }
    let ids_first: Vec<String> = (10..=15)
        .map(|n| pr_task(&jsonl, n)["id"].as_str().unwrap().to_owned())
        .collect();

    // A legacy Task record (pre-#333: no new fields) still parses and inspects
    // without warnings.
    let legacy = serde_json::json!({
        "record_type":"node","id":"project:v1:legacy-task","kind":"Task",
        "schema_version":1,"domain":"project","source_kind":"github_pr",
        "summary":"github_pr #999","title":"Legacy"
    });
    let legacy_path = tmp.path().join("legacy.jsonl");
    std::fs::write(&legacy_path, format!("{legacy}\n")).unwrap();
    let inspect = egregore()
        .args(["inspect", legacy_path.to_str().unwrap()])
        .output()
        .expect("inspect legacy");
    assert!(inspect.status.success(), "legacy Task inspects cleanly");
    let inspect_err = String::from_utf8_lossy(&inspect.stderr);
    assert!(
        !inspect_err.to_lowercase().contains("warn"),
        "no warnings for a legacy Task: {inspect_err}"
    );

    // Re-import into the same state: identical output → stable IDs, no churn.
    let server2 = MockServer::start(six_pr_routes());
    let out2 = tmp.path().join("graph2.jsonl");
    let state2 = tmp.path().join("state2.json");
    let (jsonl2, _, ok2) = run_import(&server2.base_url, &out2, &state2, &[]);
    assert!(ok2);
    let ids_second: Vec<String> = (10..=15)
        .map(|n| pr_task(&jsonl2, n)["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids_first, ids_second, "stable IDs must not change");
}

#[test]
fn merged_pr_links_to_seeded_commit_or_diagnoses_unresolved() {
    // Seed commits for PR #10 (AAA) and #11 (BBB); #12 (CCC) is unseeded.
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(
        &code_graph,
        commit_seed(&[
            "mergeaaa1111111111111111111111111111111a",
            "mergebbb2222222222222222222222222222222b",
        ]),
    )
    .unwrap();

    let server = MockServer::start(six_pr_routes());
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok);

    // Exactly two MERGED_AS edges (#10 → commit-0, #11 → commit-1).
    assert_eq!(
        edges_of_label(&jsonl, "MERGED_AS"),
        2,
        "one MERGED_AS edge per resolved merge commit"
    );
    let pr10_id = pr_task(&jsonl, 10)["id"].as_str().unwrap().to_owned();
    let has_edge = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v["record_type"] == "edge"
                && v["label"] == "MERGED_AS"
                && v["source"] == pr10_id.as_str()
                && v["target"] == "codegraph:v5:commit-0"
        });
    assert!(has_edge, "PR #10 MERGED_AS edge targets the seeded Commit");

    // PR #12's SHA (CCC) has no matching Commit → a github_commit_unresolved
    // Diagnostic carrying the SHA and the Task record ID.
    let pr12_id = pr_task(&jsonl, 12)["id"].as_str().unwrap().to_owned();
    let unresolved = nodes_of_kind(&jsonl, "Diagnostic")
        .into_iter()
        .find(|d| {
            d["summary"]
                .as_str()
                .unwrap_or("")
                .contains("github_commit_unresolved")
                && d["summary"]
                    .as_str()
                    .unwrap_or("")
                    .contains("mergeccc3333333333333333333333333333333c")
        })
        .expect("PR #12 emits a github_commit_unresolved Diagnostic");
    assert!(
        unresolved["summary"].as_str().unwrap().contains(&pr12_id),
        "diagnostic carries the Task record ID: {unresolved}"
    );

    // Open / draft / closed-unmerged PRs (no merge_commit_sha) → no edge, no
    // diagnostic keyed to them.
    assert!(
        !jsonl.contains("headsha000000000000000000000000000000e14")
            || edges_of_label(&jsonl, "MERGED_AS") == 2,
        "unmerged PRs produce no MERGED_AS edge"
    );
}

#[test]
fn merged_as_multiple_commit_matches_emits_diagnostic_not_guess() {
    // Two Commit records claim the SAME sha as PR #10's merge_commit_sha.
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(
        &code_graph,
        commit_seed(&[
            "mergeaaa1111111111111111111111111111111a",
            "mergeaaa1111111111111111111111111111111a",
        ]),
    )
    .unwrap();

    let server = MockServer::start(six_pr_routes());
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok);

    // Ambiguous SHA → no MERGED_AS edge for #10, a diagnostic instead.
    let pr10_id = pr_task(&jsonl, 10)["id"].as_str().unwrap().to_owned();
    let edge_for_10 = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| v["label"] == "MERGED_AS" && v["source"] == pr10_id.as_str());
    assert!(!edge_for_10, "ambiguous merge commit must not be guessed");
    let ambiguous = nodes_of_kind(&jsonl, "Diagnostic").into_iter().any(|d| {
        d["summary"]
            .as_str()
            .unwrap_or("")
            .contains("github_commit_unresolved")
            && d["summary"].as_str().unwrap_or("").contains(&pr10_id)
    });
    assert!(
        ambiguous,
        "multiple matches emit a github_commit_unresolved Diagnostic"
    );
}

#[test]
fn merged_as_edge_is_a_project_domain_edge() {
    // Issue #333 / Codex P2: the MERGED_AS Task→Commit edge must be a
    // project-domain edge (`project:v1:` ID + PROJECT_SCHEMA_VERSION), not a
    // `codegraph:v5:` edge. A codegraph-stamped edge serializes under the
    // codegraph domain, bypasses the daemon project-edge validator, and makes
    // `project:v1:` consumers miss the PR→Commit merge link.
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(
        &code_graph,
        commit_seed(&["mergeaaa1111111111111111111111111111111a"]),
    )
    .unwrap();

    let server = MockServer::start(six_pr_routes());
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok);

    let pr10_id = pr_task(&jsonl, 10)["id"].as_str().unwrap().to_owned();
    let edge = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["label"] == "MERGED_AS" && v["source"] == pr10_id.as_str())
        .expect("PR #10 MERGED_AS edge present");

    let id = edge["id"].as_str().unwrap_or("");
    assert!(
        id.starts_with("project:v1:"),
        "MERGED_AS must be a project-domain edge, got id '{id}'"
    );
    assert_eq!(
        edge["schema_version"], 1,
        "MERGED_AS edge must carry PROJECT_SCHEMA_VERSION (1), got {edge}"
    );
    // The target stays the codegraph Commit node — only the edge's own identity
    // moves into the project domain.
    assert_eq!(edge["target"], "codegraph:v5:commit-0");
}

#[test]
fn upgrading_state_format_forces_one_pulls_refresh_then_idempotent() {
    // Issue #333 / Codex P2: a pre-#333 state file carries an older state-format
    // version. When the upgraded binary runs against it, a cached `/pulls` ETag
    // would otherwise 304 and skip the pulls branch, so unchanged PRs never get
    // the newly-promoted flat Task fields. The state-format bump must discard the
    // stale state so the pulls branch re-fetches and re-emits the new fields.
    // After that ONE forced refresh, AC8 idempotency must still hold.
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let server = MockServer::start(six_pr_routes());

    // 1. Fresh import writes a current-version state with cached pulls ETag.
    let (_j1, _e1, ok1) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok1);

    // 2. Simulate a pre-#333 state file: identical cached ETags/hashes but the
    //    OLDER state-format version (1). Without the format bump this file is
    //    reused as-is and the pulls endpoint 304s, suppressing the new fields.
    let mut sj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    sj["schema_version"] = serde_json::json!(1);
    std::fs::write(&state, serde_json::to_string_pretty(&sj).unwrap()).unwrap();

    // 3. Re-import with the upgraded binary. The older-version state is discarded
    //    → pulls re-fetched (200) → PR #10 re-emitted WITH the promoted head_sha.
    let out2 = tmp.path().join("graph2.jsonl");
    let (j2, _e2, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);
    let pr10 = pr_task(&j2, 10);
    assert_eq!(
        pr10["head_sha"], "headsha000000000000000000000000000000a10",
        "forced refresh must re-emit the promoted head_sha field: {pr10}"
    );

    // 4. A second unchanged re-import on the now-current-version state must be
    //    idempotent — zero per-resource records re-emitted (issue #333 AC8).
    let out3 = tmp.path().join("graph3.jsonl");
    let (j3, _e3, ok3) = run_import(&server.base_url, &out3, &state, &[]);
    assert!(ok3);
    assert_eq!(
        nodes_of_kind(&j3, "Task").len(),
        0,
        "AC8: no Task re-emitted on an unchanged re-import: {j3}"
    );
    assert_eq!(
        nodes_of_kind(&j3, "ExternalLink").len(),
        0,
        "AC8: no ExternalLink re-emitted on an unchanged re-import"
    );
    assert_eq!(
        nodes_of_kind(&j3, "Review").len(),
        0,
        "AC8: no Review re-emitted on an unchanged re-import"
    );
}

/// Route table for one MERGED PR (#30) carrying a real `merge_commit_sha`, with
/// an overridable pulls `ETag` so a re-import can force the pulls list to
/// re-fetch (200) while the PR payload stays byte-identical. Used to exercise the
/// PR-resource `is_unchanged` change-detection gate independently of the pulls
/// `ETag`/304 gate (issue #333, Codex round-4).
fn one_merged_pr_routes(pulls_etag: &str) -> HashMap<String, Canned> {
    let pulls = serde_json::json!([
        {
            "number": 30, "title":"Merged PR","body":"PR body.",
            "state":"closed","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "merged_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-02T00:00:00Z",
            "head":{"ref":"feature-x","sha":"headsha000000000000000000000000000000x30"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b30"},
            "merge_commit_sha":"merge30000000000000000000000000000000030",
            "html_url":"https://github.com/o/r/pull/30"
        }
    ])
    .to_string();
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, pulls_etag),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"ic-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"prc-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/30/reviews?per_page=100".to_owned(),
        Canned::ok("[]", "\"prr-30\""),
    );
    routes
}

#[test]
fn seeded_code_graph_change_re_emits_merge_link_then_stays_idempotent() {
    // Issue #333, Codex round-4: when a repo already has current importer state
    // and the PR payload is unchanged, the PR-resource `is_unchanged` gate must
    // still re-run merge-link resolution when the seed graph changes. Reported
    // broken flow: import GitHub first WITHOUT `--code-graph` (or before the
    // merge commit is in the code graph), then re-import WITH a seeded code graph
    // that now contains the merge commit — the MERGED_AS edge never appeared
    // because `pull_hash` did not depend on `ctx.commit_index`. Folding the
    // resolution outcome into the PR change-detection hash re-emits the edge on a
    // changed seed graph; an unchanged seed keeps re-imports idempotent (AC8).
    //
    // The pulls `ETag` is bumped between runs so the pulls list re-fetches (200)
    // while PR #30's payload stays byte-identical — this isolates the
    // change-detection hash gate from the pulls `ETag`/304 gate.
    let sha = "merge30000000000000000000000000000000030";
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[sha])).unwrap();

    let server = MockServer::start(one_merged_pr_routes("\"pulls-v1\""));

    // 1. First import WITHOUT `--code-graph`: the merge SHA cannot resolve, so no
    //    MERGED_AS edge. State persists the pr:30 change hash (marker "none").
    let out1 = tmp.path().join("graph1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    assert_eq!(
        edges_of_label(&j1, "MERGED_AS"),
        0,
        "no MERGED_AS without a seeded code graph"
    );

    // 2. Re-import the SAME unchanged PR payload but now WITH a seeded code graph
    //    containing a Commit whose SHA == merge_commit_sha. The pulls list
    //    re-fetches (bumped ETag → 200) but PR #30's payload is byte-identical.
    //    Before the fix the unchanged-hash gate suppresses the new resolution and
    //    NO MERGED_AS edge is emitted; this assertion FAILS against pre-fix code.
    server.set_routes(one_merged_pr_routes("\"pulls-v2\""));
    let out2 = tmp.path().join("graph2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);
    assert_eq!(
        edges_of_label(&j2, "MERGED_AS"),
        1,
        "a seed graph that newly resolves the merge SHA must re-emit the MERGED_AS edge"
    );
    let pr30_id = pr_task(&j2, 30)["id"].as_str().unwrap().to_owned();
    let linked = j2
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v["label"] == "MERGED_AS"
                && v["source"] == pr30_id.as_str()
                && v["target"] == "codegraph:v5:commit-0"
        });
    assert!(linked, "MERGED_AS links PR #30 to the seeded Commit");

    // 3. Re-import a THIRD time with the SAME seed graph and unchanged PR. The
    //    resolution outcome is identical, so the hash is unchanged and ZERO
    //    per-resource records re-emit — no duplicate MERGED_AS (AC8 preserved).
    server.set_routes(one_merged_pr_routes("\"pulls-v3\""));
    let out3 = tmp.path().join("graph3.jsonl");
    let (j3, _, ok3) = run_import(
        &server.base_url,
        &out3,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok3);
    assert_eq!(
        nodes_of_kind(&j3, "Task").len(),
        0,
        "AC8: unchanged PR + unchanged seed re-emits no Task: {j3}"
    );
    assert_eq!(
        edges_of_label(&j3, "MERGED_AS"),
        0,
        "AC8: no duplicate MERGED_AS on an unchanged re-import"
    );
}

#[test]
fn seed_graph_change_reprocesses_prs_across_304() {
    // Issue #333, Codex round-5: the round-4 `pull_hash` merge-link marker only
    // runs INSIDE the `FetchOutcome::Modified` branch. When the cached `/pulls`
    // ETag matches, GitHub returns 304 and PR processing short-circuits BEFORE the
    // marker is ever computed, so a changed seed graph never re-emits the
    // `MERGED_AS` edge. The `/pulls` conditional request must therefore be gated on
    // a seed-graph fingerprint: a changed seed graph suppresses the
    // `If-None-Match` so `/pulls` returns a full 200 and merge links recompute,
    // while an unchanged seed keeps the 304 fast path.
    //
    // Unlike the round-4 test, the pulls ETag is held CONSTANT across every run so
    // the mock genuinely returns 304 whenever the importer sends `If-None-Match`.
    let sha = "merge30000000000000000000000000000000030";
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[sha])).unwrap();

    // One server, one stable pulls ETag for every run.
    let server = MockServer::start(one_merged_pr_routes("\"pulls-const\""));

    // 1. First import WITHOUT `--code-graph`: 200 (first fetch, no prior ETag). The
    //    merge SHA cannot resolve → no MERGED_AS. State caches the pulls ETag and
    //    the fingerprint for the empty seed ("none").
    let out1 = tmp.path().join("graph1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    assert_eq!(
        edges_of_label(&j1, "MERGED_AS"),
        0,
        "no MERGED_AS without a seeded code graph"
    );

    // 2. Re-import the SAME unchanged PR (same pulls ETag → the mock is prepared to
    //    return 304) but now WITH a seed code graph containing a Commit whose SHA
    //    == merge_commit_sha. Against pre-fix code the importer sends the cached
    //    ETag → mock 304 → PRs skipped → NO MERGED_AS (RED). With the fix the
    //    changed fingerprint suppresses the `/pulls` ETag → mock 200 → MERGED_AS
    //    emitted.
    server.clear_requests();
    let out2 = tmp.path().join("graph2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);
    assert_eq!(
        edges_of_label(&j2, "MERGED_AS"),
        1,
        "a changed seed graph must suppress the /pulls ETag so the merge link \
         recomputes across a would-be 304"
    );
    let pr30_id = pr_task(&j2, 30)["id"].as_str().unwrap().to_owned();
    let linked = j2
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| {
            v["label"] == "MERGED_AS"
                && v["source"] == pr30_id.as_str()
                && v["target"] == "codegraph:v5:commit-0"
        });
    assert!(linked, "MERGED_AS links PR #30 to the seeded Commit");
    // A full 200 payload means the per-PR reviews endpoint was visited (per-PR
    // reviews fire only when the pulls list changed).
    assert!(
        server
            .request_paths()
            .iter()
            .any(|p| p.contains("/pulls/30/reviews")),
        "a changed seed graph forces a full /pulls 200 (per-PR reviews fetched)"
    );

    // 3. Re-import a THIRD time with the SAME seed graph and unchanged PR. The
    //    fingerprint now matches, so the importer sends `If-None-Match` and the
    //    mock returns a genuine 304 fast path: zero per-resource records, no
    //    duplicate MERGED_AS, and the per-PR reviews endpoint is never visited.
    server.clear_requests();
    let out3 = tmp.path().join("graph3.jsonl");
    let (j3, _, ok3) = run_import(
        &server.base_url,
        &out3,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok3);
    assert_eq!(
        nodes_of_kind(&j3, "Task").len(),
        0,
        "AC8: unchanged PR + unchanged seed re-emits no Task: {j3}"
    );
    assert_eq!(
        edges_of_label(&j3, "MERGED_AS"),
        0,
        "AC8: no duplicate MERGED_AS on an unchanged re-import"
    );
    // A genuine 304 on `/pulls` short-circuits PR processing: the per-PR reviews
    // endpoint must NOT be visited on the unchanged-seed fast path. This proves the
    // mock honoured the conditional request (returned 304) in step 3.
    assert!(
        !server
            .request_paths()
            .iter()
            .any(|p| p.contains("/pulls/30/reviews")),
        "an unchanged seed graph keeps the /pulls 304 fast path (no per-PR review fetch)"
    );
}

/// Route table for one OPEN PR (#20, `merged_at: null`) whose REST payload still
/// carries a `merge_commit_sha` — GitHub's temporary TEST-MERGE commit for a
/// mergeable-but-unmerged PR. The importer must treat this SHA as *not* merge
/// evidence: no flat `merge_commit_sha` field, no `MERGED_AS` edge, and no
/// `github_commit_unresolved` diagnostic, even when the seeded code graph
/// contains a Commit with that exact SHA.
fn open_pr_with_test_merge_routes() -> HashMap<String, Canned> {
    let pulls = serde_json::json!([
        {
            "number": 20, "title":"Open, mergeable","body":"PR body T.",
            "state":"open","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T06:00:00Z",
            "head":{"ref":"feature-t","sha":"headsha000000000000000000000000000000t20"},
            "base":{"ref":"main","sha":"basesha000000000000000000000000000000b20"},
            "merge_commit_sha":"testmerge99999999999999999999999999999999",
            "html_url":"https://github.com/o/r/pull/20"
        }
    ])
    .to_string();
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, "\"pulls-testmerge\""),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"ic-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"prc-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/20/reviews?per_page=100".to_owned(),
        Canned::ok("[]", "\"prr-20\""),
    );
    routes
}

#[test]
fn open_pr_test_merge_sha_is_not_merge_evidence() {
    // Regression (Codex P2, #333): an OPEN PR whose payload carries a temporary
    // test-merge `merge_commit_sha` must never be treated as merge evidence, even
    // when the seeded code graph contains a Commit with that exact SHA.
    let sha = "testmerge99999999999999999999999999999999";
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[sha])).unwrap();

    let server = MockServer::start(open_pr_with_test_merge_routes());
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok);

    // 1. No first-class flat merge_commit_sha on the open PR's Task.
    let pr20 = pr_task(&jsonl, 20);
    assert!(
        pr20.get("merge_commit_sha").is_none() || pr20["merge_commit_sha"].is_null(),
        "unmerged PR must not carry a merge_commit_sha field: {pr20}"
    );
    assert!(
        pr20.get("merged_at").is_none() || pr20["merged_at"].is_null(),
        "unmerged PR has no merged_at"
    );

    // 2. No MERGED_AS edge from this PR Task (none at all in this fixture).
    let pr20_id = pr20["id"].as_str().unwrap().to_owned();
    let has_edge = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| v["label"] == "MERGED_AS" && v["source"] == pr20_id.as_str());
    assert!(!has_edge, "unmerged PR must not emit a MERGED_AS edge");
    assert_eq!(
        edges_of_label(&jsonl, "MERGED_AS"),
        0,
        "no MERGED_AS edge for a test-merge SHA on an unmerged PR"
    );

    // 3. No github_commit_unresolved diagnostic keyed to this PR/SHA.
    let diagnosed = nodes_of_kind(&jsonl, "Diagnostic").into_iter().any(|d| {
        let s = d["summary"].as_str().unwrap_or("");
        s.contains("github_commit_unresolved") && (s.contains(sha) || s.contains(&pr20_id))
    });
    assert!(
        !diagnosed,
        "unmerged PR must not emit a github_commit_unresolved diagnostic"
    );
}

#[test]
fn pr_promoted_fields_survive_redaction_on_export() {
    // Redaction is always on in the importer; the six PR fields are plaintext
    // substrate and must survive verbatim (never routed through redaction).
    let server = MockServer::start(six_pr_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    let pr10 = pr_task(&jsonl, 10);
    assert_eq!(
        pr10["head_sha"], "headsha000000000000000000000000000000a10",
        "head_sha survives redaction-on export as plaintext"
    );
    assert_eq!(
        pr10["merge_commit_sha"], "mergeaaa1111111111111111111111111111111a",
        "merge_commit_sha survives redaction-on export as plaintext"
    );
    assert!(
        !jsonl.contains("<REDACTED:")
            || (pr10["head_sha"] == "headsha000000000000000000000000000000a10"),
        "PR SHA fields are never redaction markers"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn pr_promoted_fields_survive_embedded_inspect_roundtrip() {
    let server = MockServer::start(six_pr_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (_jsonl, _, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok);

    let data_dir = tmp.path().join("store");
    let ingest = egregore()
        .args([
            "ingest",
            out.to_str().unwrap(),
            "--adapter",
            "embedded",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("ingest embedded");
    assert!(
        ingest.status.success(),
        "embedded ingest of PR tasks should succeed: {}",
        String::from_utf8_lossy(&ingest.stderr)
    );

    let inspect = egregore()
        .args(["inspect", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .expect("inspect data-dir");
    assert!(inspect.status.success());
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    let report: serde_json::Value =
        serde_json::from_str(stdout.lines().next().unwrap_or("{}")).expect("inspect JSON");
    // Zero unknown (domain, kind, schema_version) tuples.
    assert_eq!(
        report["unknown_schema_versions"]
            .as_object()
            .map_or(0, serde_json::Map::len),
        0,
        "no unknown schema versions: {report}"
    );
    // The six PR Tasks are counted under the (project, Task, 1) tuple.
    assert_eq!(
        report["schema_versions"]["project:Task:1"], 6,
        "six PR Tasks under project:Task:1: {report}"
    );
}

// ── Issue #333 (Codex round-6): a changed merge-resolution outcome on re-import
//    retracts the superseded prior artifact via a Tombstone(deleted_id) ─────────

/// Counts `record_type: "tombstone"` records in a handoff JSONL.
fn tombstones(jsonl: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["record_type"] == "tombstone")
        .collect()
}

#[test]
fn merge_resolution_unresolved_to_resolved_retracts_prior_diagnostic() {
    // The importer is otherwise purely additive: when PR #30's merge SHA goes from
    // UNRESOLVED (a github_commit_unresolved Diagnostic D) to RESOLVED (a MERGED_AS
    // edge E), the new edge carries a NEW id and — without retraction — D lingers
    // live in a persistent store, so stale and fresh merge evidence coexist for one
    // PR. The changed outcome must emit a Tombstone(deleted_id == D).
    let merge_sha = "merge30000000000000000000000000000000030";
    let decoy_sha = "decoy000000000000000000000000000000000000";
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    let server = MockServer::start(one_merged_pr_routes("\"pulls-r6a\""));

    // 1. A NON-EMPTY seed lacking the merge SHA → PR #30 emits diagnostic D.
    std::fs::write(&code_graph, commit_seed(&[decoy_sha])).unwrap();
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(
        &server.base_url,
        &out1,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok1);
    assert_eq!(
        edges_of_label(&j1, "MERGED_AS"),
        0,
        "an unseeded merge SHA emits no MERGED_AS edge"
    );
    let d_id = nodes_of_kind(&j1, "Diagnostic")
        .into_iter()
        .find(|d| {
            let s = d["summary"].as_str().unwrap_or("");
            s.contains("github_commit_unresolved") && s.contains(merge_sha)
        })
        .expect("run 1 emits a github_commit_unresolved Diagnostic")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 2. Re-seed so the merge SHA now RESOLVES → edge E emitted, D retracted.
    std::fs::write(&code_graph, commit_seed(&[merge_sha])).unwrap();
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);
    // (a) the new MERGED_AS edge E is emitted.
    assert_eq!(
        edges_of_label(&j2, "MERGED_AS"),
        1,
        "a resolving seed emits the MERGED_AS edge"
    );
    // (b) a Tombstone whose deleted_id == D is emitted (RED against pre-fix code:
    //     the additive importer never emitted a tombstone, so D stayed live).
    let retracted_d = tombstones(&j2)
        .into_iter()
        .any(|t| t["deleted_id"] == d_id.as_str());
    assert!(
        retracted_d,
        "the superseded diagnostic D must be retracted via a Tombstone(deleted_id): {j2}"
    );
    // The stale diagnostic node itself is not re-emitted on the resolving run.
    assert!(
        !nodes_of_kind(&j2, "Diagnostic")
            .into_iter()
            .any(|d| d["id"] == d_id.as_str()),
        "the stale diagnostic node is retracted, not re-emitted"
    );

    // 3. Idempotency (AC8): a third re-import with the SAME resolving seed changes
    //    nothing — no new tombstone, zero per-resource records.
    let out3 = tmp.path().join("g3.jsonl");
    let (j3, _, ok3) = run_import(
        &server.base_url,
        &out3,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok3);
    assert_eq!(
        nodes_of_kind(&j3, "Task").len(),
        0,
        "AC8: no Task re-emitted on an unchanged re-import: {j3}"
    );
    assert_eq!(
        edges_of_label(&j3, "MERGED_AS"),
        0,
        "AC8: no duplicate MERGED_AS on an unchanged re-import"
    );
    assert_eq!(
        tombstones(&j3).len(),
        0,
        "AC8: no tombstone emitted on an unchanged re-import: {j3}"
    );
}

#[test]
fn upgraded_v2_store_still_tombstones_changed_merge_resolution() {
    // Codex #352 P2 end-to-end regression: bumping STATE_SCHEMA_VERSION 2→3 must
    // MIGRATE the on-disk state, not discard it. A blunt discard drops #333's
    // `pr_merge_artifacts` tracking, so on the FIRST v3 run against an upgraded
    // store a PR whose merge now resolves differently emits the fresh artifact but
    // cannot tombstone the stale one → stale + fresh merge evidence coexist. The
    // v2→v3 migration preserves `pr_merge_artifacts`, so the retraction still fires.
    let merge_sha = "merge30000000000000000000000000000000030";
    let decoy_sha = "decoy000000000000000000000000000000000000";
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    let server = MockServer::start(one_merged_pr_routes("\"pulls-v2mig\""));

    // 1. Unseeded (decoy-only) merge SHA → PR #30 emits diagnostic D; the state
    //    records D's id in pr_merge_artifacts. State is written at the current
    //    (v3) version.
    std::fs::write(&code_graph, commit_seed(&[decoy_sha])).unwrap();
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(
        &server.base_url,
        &out1,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok1);
    let d_id = nodes_of_kind(&j1, "Diagnostic")
        .into_iter()
        .find(|d| {
            let s = d["summary"].as_str().unwrap_or("");
            s.contains("github_commit_unresolved") && s.contains(merge_sha)
        })
        .expect("run 1 emits a github_commit_unresolved Diagnostic")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 2. Simulate a pre-#334 (v2) store that already tracked pr_merge_artifacts:
    //    downgrade ONLY the on-disk schema_version to 2, leaving pr_merge_artifacts
    //    (and every other field) intact — exactly what an upgraded store looks like.
    let mut sj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    sj["schema_version"] = serde_json::json!(2);
    assert!(
        sj["pr_merge_artifacts"]
            .as_object()
            .is_some_and(|m| !m.is_empty()),
        "precondition: the v2 state must carry a tracked merge artifact"
    );
    std::fs::write(&state, serde_json::to_string_pretty(&sj).unwrap()).unwrap();

    // 3. First v3 run against the migrated store, now with a RESOLVING seed: the
    //    outcome changes D→E. Because the migration preserved pr_merge_artifacts,
    //    the superseded diagnostic D is tombstoned. (RED against the blunt-discard
    //    code: prior_merge_artifact would be empty and no tombstone would fire.)
    std::fs::write(&code_graph, commit_seed(&[merge_sha])).unwrap();
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);
    assert_eq!(
        edges_of_label(&j2, "MERGED_AS"),
        1,
        "the resolving seed emits the fresh MERGED_AS edge"
    );
    let retracted_d = tombstones(&j2)
        .into_iter()
        .any(|t| t["deleted_id"] == d_id.as_str());
    assert!(
        retracted_d,
        "v2→v3 migration must preserve pr_merge_artifacts so the stale diagnostic D is tombstoned: {j2}"
    );
    assert!(
        !nodes_of_kind(&j2, "Diagnostic")
            .into_iter()
            .any(|d| d["id"] == d_id.as_str()),
        "the stale diagnostic node is retracted, not re-emitted"
    );
}

#[test]
fn merge_resolution_resolved_to_unresolved_retracts_prior_edge() {
    // The reverse transition: PR #30 goes from RESOLVED (MERGED_AS edge E) back to
    // UNRESOLVED (a diagnostic). The superseded edge E must be retracted via a
    // Tombstone(deleted_id == E) so it does not linger live beside the diagnostic.
    let merge_sha = "merge30000000000000000000000000000000030";
    let decoy_sha = "decoy000000000000000000000000000000000000";
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    let server = MockServer::start(one_merged_pr_routes("\"pulls-r6b\""));

    // 1. A resolving seed → MERGED_AS edge E.
    std::fs::write(&code_graph, commit_seed(&[merge_sha])).unwrap();
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(
        &server.base_url,
        &out1,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok1);
    let pr30_id = pr_task(&j1, 30)["id"].as_str().unwrap().to_owned();
    let e_id = j1
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["label"] == "MERGED_AS" && v["source"] == pr30_id.as_str())
        .expect("run 1 emits a MERGED_AS edge")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 2. Re-seed to a non-empty graph WITHOUT the merge SHA → outcome regresses to
    //    unresolved: a diagnostic is emitted and E is retracted.
    std::fs::write(&code_graph, commit_seed(&[decoy_sha])).unwrap();
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);
    assert_eq!(
        edges_of_label(&j2, "MERGED_AS"),
        0,
        "no MERGED_AS edge once the merge SHA no longer resolves"
    );
    let retracted_e = tombstones(&j2)
        .into_iter()
        .any(|t| t["deleted_id"] == e_id.as_str());
    assert!(
        retracted_e,
        "the superseded MERGED_AS edge E must be retracted via a Tombstone(deleted_id): {j2}"
    );
    assert!(
        nodes_of_kind(&j2, "Diagnostic").into_iter().any(|d| {
            let s = d["summary"].as_str().unwrap_or("");
            s.contains("github_commit_unresolved") && s.contains(merge_sha)
        }),
        "the new unresolved outcome emits a github_commit_unresolved Diagnostic"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn merge_resolution_change_suppresses_stale_artifact_in_embedded_current_view() {
    // End-to-end proof through an embedded store's CURRENT read view: an
    // unresolved→resolved re-import ingested into one AletheiaDB store must leave
    // the fresh MERGED_AS edge E visible and the superseded diagnostic D suppressed
    // (retracted by the Tombstone(deleted_id) the importer now emits). Records are
    // written in-process through a single sink instance — mirroring two sequential
    // imports into one persistent store while staying deterministic under load.
    use aletheia_egregore::adapters::{EmbeddedAletheiaSink, ingest_records, records_from_jsonl};
    use aletheia_egregore::ir::GraphRecord;

    let merge_sha = "merge30000000000000000000000000000000030";
    let decoy_sha = "decoy000000000000000000000000000000000000";
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let data_dir = tmp.path().join("store");
    // The import's `--code-graph` files are read as resolution indexes only, never
    // ingested — run 1 (decoy: merge SHA unresolved → diagnostic D) and run 2
    // (merge: resolved → edge E) use SEPARATE index files.
    let run1_cg = tmp.path().join("run1_cg.jsonl");
    let run2_cg = tmp.path().join("run2_cg.jsonl");
    std::fs::write(&run1_cg, commit_seed(&[decoy_sha])).unwrap();
    std::fs::write(&run2_cg, commit_seed(&[merge_sha])).unwrap();
    let server = MockServer::start(one_merged_pr_routes("\"pulls-r6emb\""));

    // Run 1: the decoy index leaves the merge SHA unresolved → diagnostic D.
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(
        &server.base_url,
        &out1,
        &state,
        &["--code-graph", run1_cg.to_str().unwrap()],
    );
    assert!(ok1);
    let d_id = nodes_of_kind(&j1, "Diagnostic")
        .into_iter()
        .find(|d| {
            let s = d["summary"].as_str().unwrap_or("");
            s.contains("github_commit_unresolved") && s.contains(merge_sha)
        })
        .expect("run 1 emits a github_commit_unresolved Diagnostic")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Run 2: the resolving index links the merge SHA → edge E + Tombstone(D).
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", run2_cg.to_str().unwrap()],
    );
    assert!(ok2);
    let e_id = j2
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["label"] == "MERGED_AS")
        .expect("run 2 emits a MERGED_AS edge")["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Ingest through ONE sink instance: batch 1 = run 1's handoff (writes D);
    // batch 2 = run 2's handoff prepended with the seed Commit so edge E's target
    // resolves in-batch (writes E and the Tombstone(D), which supersedes D by a
    // higher write sequence).
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("open embedded store");
    let batch1 = records_from_jsonl(&j1).expect("parse run 1 handoff");
    let report1 = ingest_records(&batch1, &mut sink);
    assert_eq!(
        report1.failed, 0,
        "run 1 ingest failed: {:?}",
        report1.failures
    );
    let mut batch2 = records_from_jsonl(&commit_seed(&[merge_sha])).expect("parse seed commit");
    batch2.extend(records_from_jsonl(&j2).expect("parse run 2 handoff"));
    let report2 = ingest_records(&batch2, &mut sink);
    assert_eq!(
        report2.failed, 0,
        "run 2 ingest failed: {:?}",
        report2.failures
    );

    // Current read view of the store: D suppressed, E present.
    let records = sink.read_all_records().expect("read current view");
    let d_present = records
        .iter()
        .any(|r| matches!(r, GraphRecord::Node { id, .. } if *id == d_id));
    let e_present = records
        .iter()
        .any(|r| matches!(r, GraphRecord::Edge { id, .. } if *id == e_id));
    assert!(
        !d_present,
        "the superseded diagnostic D must NOT appear in the embedded current read view"
    );
    assert!(
        e_present,
        "the fresh MERGED_AS edge E must appear in the embedded current read view"
    );
}

// ── Issue #334: anchor every review to the commit it reviewed ────────────────────

// Distinct review-commit SHAs. `_a`/`_b` are seeded (resolvable); `_x` is never
// seeded (unresolved). None equal any PR head SHA — reviews anchor to the exact
// commit reviewed, which after a force-push differs from the PR head.
const RC_SHA_A: &str = "revcommitaaa0000000000000000000000000000a";
const RC_SHA_B: &str = "revcommitbbb0000000000000000000000000000b";
const RC_SHA_X: &str = "revcommitxxx0000000000000000000000000000x";

/// Four PRs (10–13), each open with a distinct head SHA that differs from every
/// review commit SHA (the force-push case).
fn four_pulls_json() -> String {
    let mut prs = Vec::new();
    for n in 10..=13 {
        prs.push(serde_json::json!({
            "number": n, "title": format!("PR {n}"), "body": format!("Body {n}."),
            "state":"open","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "head":{"ref":format!("feature-{n}"),"sha":format!("prheadsha{n}00000000000000000000000000000000")},
            "base":{"ref":"main","sha":format!("prbasesha{n}00000000000000000000000000000000")},
            "html_url":format!("https://github.com/o/r/pull/{n}")
        }));
    }
    serde_json::Value::Array(prs).to_string()
}

/// Eight PR review summaries across the four PRs. Mix of resolvable, unresolved,
/// and genuinely-absent (`commit_id` omitted → unanchored) anchors.
fn eight_pr_reviews_for(number: u64) -> String {
    let rows: Vec<serde_json::Value> = match number {
        10 => vec![
            review_row(401, Some(RC_SHA_A)),
            review_row(402, Some(RC_SHA_X)),
        ],
        11 => vec![review_row(403, Some(RC_SHA_B)), review_row(404, None)],
        12 => vec![
            review_row(405, Some(RC_SHA_A)),
            review_row(406, Some(RC_SHA_B)),
        ],
        13 => vec![review_row(407, Some(RC_SHA_X)), review_row(408, None)],
        _ => vec![],
    };
    serde_json::Value::Array(rows).to_string()
}

fn review_row(id: u64, commit_id: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": id, "body": format!("Review {id}."), "state":"APPROVED",
        "user":{"login":"reviewer"},"submitted_at":"2026-01-02T03:00:00Z",
        "html_url": format!("https://github.com/o/r/pull/x#pullrequestreview-{id}")
    });
    if let Some(sha) = commit_id {
        v["commit_id"] = serde_json::json!(sha);
    }
    v
}

/// Six PR review comments across the four PRs: resolvable, unresolved, and one
/// with no `commit_id` (unanchored).
fn six_review_comments_json() -> String {
    let rows = vec![
        review_comment_row(501, 10, Some(RC_SHA_A)),
        review_comment_row(502, 10, Some(RC_SHA_X)),
        review_comment_row(503, 11, Some(RC_SHA_B)),
        review_comment_row(504, 11, None),
        review_comment_row(505, 12, Some(RC_SHA_A)),
        review_comment_row(506, 13, Some(RC_SHA_X)),
    ];
    serde_json::Value::Array(rows).to_string()
}

fn review_comment_row(id: u64, pr: u64, commit_id: Option<&str>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": id, "body": format!("Comment {id}."), "user":{"login":"reviewer"},
        "path":"src/lib.rs","line":10,"side":"RIGHT",
        "pull_request_url": format!("https://api.github.com/repos/o/r/pulls/{pr}"),
        "created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
        "html_url": format!("https://github.com/o/r/pull/{pr}#discussion_r{id}")
    });
    if let Some(sha) = commit_id {
        v["commit_id"] = serde_json::json!(sha);
    }
    v
}

/// One issue comment on a PR conversation (proves the `issue_comment` exemption).
fn one_issue_comment_json() -> String {
    serde_json::json!([
        {
            "id": 601, "body":"General PR chatter.", "user":{"login":"pm"},
            "issue_url":"https://api.github.com/repos/o/r/issues/10",
            "created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/pull/10#issuecomment-601"
        }
    ])
    .to_string()
}

fn review_anchor_routes(pulls_etag: &str) -> HashMap<String, Canned> {
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&four_pulls_json(), pulls_etag),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok(&one_issue_comment_json(), "\"ic-334\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok(&six_review_comments_json(), "\"prc-334\""),
    );
    for n in 10..=13 {
        routes.insert(
            format!("/repos/o/r/pulls/{n}/reviews?per_page=100"),
            Canned::ok(&eight_pr_reviews_for(n), &format!("\"prr-334-{n}\"")),
        );
    }
    routes
}

/// The Review node whose summary names `#<pr>` and carries review kind `kind`.
fn review_node_by_summary(jsonl: &str, kind: &str, needle: &str) -> Option<serde_json::Value> {
    nodes_of_kind(jsonl, "Review")
        .into_iter()
        .find(|r| r["review_kind"] == kind && r["summary"].as_str().unwrap_or("").contains(needle))
}

/// The Review node whose `system_native_id` ends with `:<id>`.
fn review_by_native(jsonl: &str, id: u64) -> serde_json::Value {
    let suffix = format!(":{id}");
    nodes_of_kind(jsonl, "Review")
        .into_iter()
        .find(|r| {
            r["system_native_id"]
                .as_str()
                .unwrap_or("")
                .ends_with(&suffix)
        })
        .unwrap_or_else(|| panic!("Review with native id ending {suffix} should exist"))
}

fn diagnostics_with(jsonl: &str, code: &str) -> usize {
    nodes_of_kind(jsonl, "Diagnostic")
        .into_iter()
        .filter(|d| d["summary"].as_str().unwrap_or("").contains(code))
        .count()
}

#[test]
fn fresh_review_import_anchors_resolved_diagnoses_unresolved_and_unanchored() {
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    // Seed Commits for RC_SHA_A (commit-0) and RC_SHA_B (commit-1); RC_SHA_X is
    // never seeded.
    std::fs::write(&code_graph, commit_seed(&[RC_SHA_A, RC_SHA_B])).unwrap();

    let server = MockServer::start(review_anchor_routes("\"pulls-334-v1\""));
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, stderr, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok, "import should succeed; stderr={stderr}");

    // review_commit_sha field populated on a resolvable pr_review.
    let approved_review = review_by_native(&jsonl, 401);
    assert_eq!(approved_review["review_kind"], "pr_review");
    assert_eq!(approved_review["review_commit_sha"], RC_SHA_A);

    // review_commit_sha on a pr_review_comment.
    let inline_comment = review_by_native(&jsonl, 501);
    assert_eq!(inline_comment["review_kind"], "pr_review_comment");
    assert_eq!(inline_comment["review_commit_sha"], RC_SHA_A);

    // issue_comment review is exempt: no review_commit_sha at all.
    let ic = review_node_by_summary(&jsonl, "issue_comment", "#10")
        .expect("issue_comment review present");
    assert!(
        ic.get("review_commit_sha").is_none() || ic["review_commit_sha"].is_null(),
        "issue_comment reviews carry no review_commit_sha: {ic}"
    );

    // REVIEWS_COMMIT edges: 4 resolvable reviews (401,403,405,406) + 3 resolvable
    // comments (501,503,505) = 7.
    assert_eq!(
        edges_of_label(&jsonl, "REVIEWS_COMMIT"),
        7,
        "one REVIEWS_COMMIT edge per resolved review commit"
    );
    // Every REVIEWS_COMMIT edge is a project-domain edge targeting a Commit.
    for edge in jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["label"] == "REVIEWS_COMMIT")
    {
        assert!(
            edge["id"].as_str().unwrap_or("").starts_with("project:v1:"),
            "REVIEWS_COMMIT must be a project-domain edge: {edge}"
        );
        assert_eq!(edge["schema_version"], 1);
        let target = edge["target"].as_str().unwrap_or("");
        assert!(
            target == "codegraph:v5:commit-0" || target == "codegraph:v5:commit-1",
            "edge targets a seeded Commit: {edge}"
        );
    }

    // Unresolved (RC_SHA_X): reviews 402,407 + comments 502,506 = 4 diagnostics.
    assert_eq!(
        diagnostics_with(&jsonl, "github_commit_unresolved"),
        4,
        "unresolved review commits diagnose, never guess"
    );
    // Unanchored (absent commit_id): reviews 404,408 + comment 504 = 3.
    assert_eq!(
        diagnostics_with(&jsonl, "github_review_unanchored"),
        3,
        "absent commit_id diagnoses the gap, never fabricates a SHA"
    );

    // The unresolved review still carries the raw SHA on its field.
    let rv402 = review_by_native(&jsonl, 402);
    assert_eq!(rv402["review_commit_sha"], RC_SHA_X);
}

#[test]
fn review_anchor_import_is_byte_identical_across_five_reimports() {
    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[RC_SHA_A, RC_SHA_B])).unwrap();
    let mut outputs = Vec::new();
    for i in 0..5 {
        let server = MockServer::start(review_anchor_routes("\"pulls-334-v1\""));
        let out = tmp.path().join(format!("graph-{i}.jsonl"));
        let state = tmp.path().join(format!("state-{i}.json"));
        let (jsonl, _, ok) = run_import(
            &server.base_url,
            &out,
            &state,
            &["--code-graph", code_graph.to_str().unwrap()],
        );
        assert!(ok, "import {i} should succeed");
        outputs.push(jsonl);
    }
    for (i, jsonl) in outputs.iter().enumerate().skip(1) {
        assert_eq!(
            *jsonl, outputs[0],
            "re-import {i} must be byte-identical to the first"
        );
    }
    assert!(outputs[0].contains("REVIEWS_COMMIT"));
    assert!(outputs[0].contains(&format!("\"review_commit_sha\":\"{RC_SHA_A}\"")));
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn review_anchor_survives_embedded_roundtrip_and_inspect() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;
    use aletheia_egregore::ir::{EdgeLabel, GraphRecord};

    let tmp = TempDir::new().unwrap();
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[RC_SHA_A, RC_SHA_B])).unwrap();
    let server = MockServer::start(review_anchor_routes("\"pulls-334-v1\""));
    let out = tmp.path().join("graph.jsonl");
    let state = tmp.path().join("state.json");
    let (_jsonl, _, ok) = run_import(
        &server.base_url,
        &out,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok);

    let data_dir = tmp.path().join("store");
    // The REVIEWS_COMMIT edge targets a seeded Commit node, so the seed graph
    // must be ingested into the same store first (it lives only in the seed file,
    // never re-emitted by the importer).
    let seed_ingest = egregore()
        .args([
            "ingest",
            code_graph.to_str().unwrap(),
            "--adapter",
            "embedded",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("ingest seed commits");
    assert!(
        seed_ingest.status.success(),
        "seed commit ingest should succeed: {}",
        String::from_utf8_lossy(&seed_ingest.stderr)
    );
    let ingest = egregore()
        .args([
            "ingest",
            out.to_str().unwrap(),
            "--adapter",
            "embedded",
            "--data-dir",
            data_dir.to_str().unwrap(),
        ])
        .output()
        .expect("ingest embedded");
    assert!(
        ingest.status.success(),
        "embedded ingest should succeed (REVIEWS_COMMIT edge label must round-trip): {}",
        String::from_utf8_lossy(&ingest.stderr)
    );

    // Read back through the embedded sink: the field + edge survive.
    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("open embedded store");
    let records = sink.read_all_records().expect("read current view");
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Edge {
                label: EdgeLabel::ReviewsCommit,
                ..
            }
        )),
        "a REVIEWS_COMMIT edge must round-trip through the embedded store"
    );
    assert!(
        records.iter().any(|r| matches!(
            r,
            GraphRecord::Node { review_commit_sha: Some(sha), .. } if sha == RC_SHA_A
        )),
        "review_commit_sha must round-trip through the embedded store"
    );

    let inspect = egregore()
        .args(["inspect", "--data-dir", data_dir.to_str().unwrap()])
        .output()
        .expect("inspect data-dir");
    assert!(inspect.status.success());
    let stdout = String::from_utf8_lossy(&inspect.stdout);
    let report: serde_json::Value =
        serde_json::from_str(stdout.lines().next().unwrap_or("{}")).expect("inspect JSON");
    assert_eq!(
        report["unknown_schema_versions"]
            .as_object()
            .map_or(0, serde_json::Map::len),
        0,
        "no unknown schema versions after review-anchor ingest: {report}"
    );
}

/// One open PR (#20) with one `pr_review` (id 700) carrying a resolvable
/// `commit_id`, with an overridable pulls `ETag` so a re-import can force the
/// reviews to re-resolve while the review payload stays byte-identical (#334).
fn one_review_routes(pulls_etag: &str, review_commit: &str) -> HashMap<String, Canned> {
    let pulls = serde_json::json!([
        {
            "number": 20, "title":"Open PR","body":"Body.",
            "state":"open","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "head":{"ref":"feature-x","sha":"prheadsha200000000000000000000000000000000"},
            "base":{"ref":"main","sha":"prbasesha200000000000000000000000000000000"},
            "html_url":"https://github.com/o/r/pull/20"
        }
    ])
    .to_string();
    let reviews = serde_json::json!([
        {
            "id": 700, "body":"Anchored review.", "state":"APPROVED",
            "user":{"login":"reviewer"},"submitted_at":"2026-01-02T03:00:00Z",
            "commit_id": review_commit,
            "html_url":"https://github.com/o/r/pull/20#pullrequestreview-700"
        }
    ])
    .to_string();
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, pulls_etag),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"ic-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"prc-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/20/reviews?per_page=100".to_owned(),
        Canned::ok(&reviews, "\"prr-700\""),
    );
    routes
}

fn tombstones334(jsonl: &str) -> Vec<serde_json::Value> {
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["record_type"] == "tombstone")
        .collect()
}

#[test]
fn review_seed_graph_change_re_resolves_tombstones_and_revives() {
    // Issue #334 (contracts #4/#5/#6): a review whose commit_id resolves only
    // after a seed-graph change must (1) re-emit the REVIEWS_COMMIT edge across a
    // would-be 304, (2) tombstone the superseded diagnostic when the outcome
    // changes, and (3) revive the edge after a resolved→unresolved→resolved cycle.
    // The pulls ETag is held CONSTANT so the mock returns 304 unless the seed
    // fingerprint suppresses it.
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[RC_SHA_A])).unwrap();
    // A DIFFERENT non-empty seed that does NOT contain the review's commit, so
    // the outcome flips edge→unresolved-diagnostic (not edge→nothing) while the
    // seed fingerprint still changes to force reprocessing across a would-be 304.
    let code_graph_other = tmp.path().join("code_other.jsonl");
    std::fs::write(&code_graph_other, commit_seed(&[RC_SHA_B])).unwrap();

    let server = MockServer::start(one_review_routes("\"pulls-const-334\"", RC_SHA_A));

    // 1. First import WITHOUT --code-graph: commit_id cannot resolve → no edge,
    //    no diagnostic (mirrors MERGED_AS empty-seed None).
    let out1 = tmp.path().join("graph1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    assert_eq!(
        edges_of_label(&j1, "REVIEWS_COMMIT"),
        0,
        "no REVIEWS_COMMIT without a seeded code graph"
    );

    // 2. Re-import WITH the seed graph. The constant pulls ETag would 304, but the
    //    changed seed fingerprint must suppress it so reviews re-resolve and the
    //    edge appears.
    server.clear_requests();
    let out2 = tmp.path().join("graph2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);
    assert_eq!(
        edges_of_label(&j2, "REVIEWS_COMMIT"),
        1,
        "a changed seed graph must re-emit the REVIEWS_COMMIT edge across a would-be 304"
    );
    assert!(
        server
            .request_paths()
            .iter()
            .any(|p| p.contains("/pulls/20/reviews")),
        "a changed seed graph forces the per-PR reviews fetch"
    );

    // 3. Re-import with a DIFFERENT seed graph that lacks the review's commit: the
    //    outcome changes edge→unresolved-diagnostic and the superseded edge must be
    //    retracted via a Tombstone(deleted_id == edge id).
    let edge_id = j2
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| v["label"] == "REVIEWS_COMMIT")
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .expect("edge id from run 2");
    server.clear_requests();
    let out3 = tmp.path().join("graph3.jsonl");
    let (j3, _, ok3) = run_import(
        &server.base_url,
        &out3,
        &state,
        &["--code-graph", code_graph_other.to_str().unwrap()],
    );
    assert!(ok3);
    assert_eq!(
        edges_of_label(&j3, "REVIEWS_COMMIT"),
        0,
        "removing the seed commit drops the edge"
    );
    assert!(
        tombstones334(&j3)
            .iter()
            .any(|t| t["deleted_id"] == edge_id.as_str()),
        "the superseded REVIEWS_COMMIT edge must be retracted via a Tombstone: {j3}"
    );
    assert_eq!(
        diagnostics_with(&j3, "github_commit_unresolved"),
        1,
        "the now-unresolved review emits a diagnostic"
    );

    // 4. Re-import WITH the seed graph once more: the outcome flips back to the
    //    ORIGINAL edge id. The prior diagnostic is tombstoned and the byte-
    //    identical edge is revived (contract #6).
    server.clear_requests();
    let out4 = tmp.path().join("graph4.jsonl");
    let (j4, _, ok4) = run_import(
        &server.base_url,
        &out4,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok4);
    let revived = j4
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .any(|v| v["label"] == "REVIEWS_COMMIT" && v["id"] == edge_id.as_str());
    assert!(
        revived,
        "the byte-identical REVIEWS_COMMIT edge must be re-emitted (revived): {j4}"
    );
    assert!(
        !tombstones334(&j4).is_empty(),
        "flipping back tombstones the superseded diagnostic: {j4}"
    );
}

/// Merge SHA for the v2→v3 upgrade P1 regression fixture (resolves against seed).
const UPGRADE_MERGE_SHA: &str = "upgmerge00000000000000000000000000000010";

/// Route table for the v2→v3 upgrade P1 regression: one closed+merged PR (#10)
/// whose `merge_commit_sha` resolves against the seed, one issue (#1) with a
/// label, one inline PR review comment (id 501) and one PR review summary (id
/// 401), each anchored to a resolvable review commit. The pulls `ETag` is held
/// constant so the mock 304s the `/pulls` list unless the migration clears it.
fn upgrade_regression_routes(pulls_etag: &str) -> HashMap<String, Canned> {
    let pulls = serde_json::json!([
        {
            "number": 10, "title":"Merged PR","body":"Body.",
            "state":"closed","draft":false,"labels":[],"assignees":[],
            "user":{"login":"dev"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "merged_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-02T00:00:00Z",
            "head":{"ref":"feature-a","sha":"upgheadsha00000000000000000000000000000010"},
            "base":{"ref":"main","sha":"upgbasesha00000000000000000000000000000010"},
            "merge_commit_sha": UPGRADE_MERGE_SHA,
            "html_url":"https://github.com/o/r/pull/10"
        }
    ])
    .to_string();
    let issues = serde_json::json!([
        {
            "number": 1, "title":"An issue","body":"Body.",
            "state":"open","labels":[{"name":"bug","color":"f00"}],"assignees":[],
            "user":{"login":"reporter"},
            "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/issues/1"
        }
    ])
    .to_string();
    let review_comments = serde_json::json!([
        {
            "id": 501, "body":"Inline.", "user":{"login":"reviewer"},
            "path":"src/lib.rs","line":10,"side":"RIGHT",
            "pull_request_url":"https://api.github.com/repos/o/r/pulls/10",
            "commit_id": RC_SHA_A,
            "created_at":"2026-01-02T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
            "html_url":"https://github.com/o/r/pull/10#discussion_r501"
        }
    ])
    .to_string();
    let reviews = serde_json::json!([
        {
            "id": 401, "body":"Anchored review.", "state":"APPROVED",
            "user":{"login":"reviewer"},"submitted_at":"2026-01-02T03:00:00Z",
            "commit_id": RC_SHA_A,
            "html_url":"https://github.com/o/r/pull/10#pullrequestreview-401"
        }
    ])
    .to_string();
    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok(&issues, "\"issues-upg\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, pulls_etag),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[{\"name\":\"bug\",\"color\":\"f00\"}]", "\"labels-upg\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"ic-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok(&review_comments, "\"prc-upg\""),
    );
    routes.insert(
        "/repos/o/r/pulls/10/reviews?per_page=100".to_owned(),
        Canned::ok(&reviews, "\"prr-401\""),
    );
    routes
}

#[test]
fn upgraded_v2_store_reemits_review_anchors_when_pull_list_unchanged() {
    // Codex #352 P1: the per-PR `/pulls/{n}/reviews` fetch is gated behind
    // `if pulls_changed` in import.rs, which is only true when the `/pulls` LIST
    // returns 200. Round 1's v2→v3 migration PRESERVED the `/pulls` list ETag, so
    // an upgraded store with an UNCHANGED PR list and seed graph took the 304 fast
    // path → pulls_changed == false → the per-PR reviews were never fetched → the
    // pre-existing `pr_review` records never re-emitted with `review_commit_sha` /
    // `REVIEWS_COMMIT`, silently dropping the #334 contract for existing reviews
    // (the common upgrade case). The migration must force a FULL review refresh
    // even when nothing changed. The pulls ETag and the seed are held CONSTANT so
    // the mock WOULD 304 the /pulls list if its ETag survived the migration.
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");
    let code_graph = tmp.path().join("code.jsonl");
    std::fs::write(&code_graph, commit_seed(&[UPGRADE_MERGE_SHA, RC_SHA_A])).unwrap();

    // 1. Fresh v3 import: emits the PR/issue Tasks, the MERGED_AS edge, and both
    //    review anchors (review 401 + comment 501). State is written at v3.
    let server = MockServer::start(upgrade_regression_routes("\"pulls-upg-const\""));
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(
        &server.base_url,
        &out1,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok1);
    assert_eq!(edges_of_label(&j1, "MERGED_AS"), 1, "fresh: MERGED_AS edge");
    assert_eq!(
        edges_of_label(&j1, "REVIEWS_COMMIT"),
        2,
        "fresh: two review anchors (summary + comment)"
    );
    assert_eq!(review_by_native(&j1, 401)["review_commit_sha"], RC_SHA_A);

    // 2. Simulate a pre-#334 (v2) store: downgrade the on-disk schema_version to 2
    //    and drop the review-family resource hashes. A genuine v2 store hashed
    //    reviews as blake3(payload) (the v2 formula), which the v3 `review_hash`
    //    (payload+marker) can never equal — so on the forced 200 refetch every
    //    review re-emits exactly once. Deleting the review hashes models that
    //    guaranteed formula mismatch. Every OTHER field (ETags incl. the /pulls
    //    LIST ETag, the pr:10 / issue:1 resource hashes, watermarks, the
    //    fingerprint) is left intact — exactly what an upgraded store looks like.
    let mut sj: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    sj["schema_version"] = serde_json::json!(2);
    if let Some(hashes) = sj["resource_hashes"].as_object_mut() {
        hashes.retain(|k, _| !k.starts_with("pr_review"));
    }
    assert!(
        sj["etags"]
            .as_object()
            .is_some_and(|m| m.keys().any(|k| k.contains("/pulls?state=all"))),
        "precondition: the v2 state carries a /pulls LIST ETag that would 304"
    );
    std::fs::write(&state, serde_json::to_string_pretty(&sj).unwrap()).unwrap();

    // 3. First v3 run against the migrated store, PR list + seed UNCHANGED. The
    //    migration must clear the /pulls list ETag so the mock returns 200,
    //    pulls_changed becomes true, and the per-PR reviews re-fetch. RED against
    //    round-1 code: the preserved /pulls ETag 304s → review 401 is never fetched.
    server.clear_requests();
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(
        &server.base_url,
        &out2,
        &state,
        &["--code-graph", code_graph.to_str().unwrap()],
    );
    assert!(ok2);

    // The per-PR reviews endpoint WAS fetched (the whole point of the P1 fix).
    assert!(
        server
            .request_paths()
            .iter()
            .any(|p| p.contains("/pulls/10/reviews")),
        "the migration must force the per-PR reviews fetch even on an unchanged PR list"
    );
    // Both existing reviews re-emit with the #334 anchor contract.
    let summary_review = review_by_native(&j2, 401);
    assert_eq!(summary_review["review_kind"], "pr_review");
    assert_eq!(
        summary_review["review_commit_sha"], RC_SHA_A,
        "the pre-existing pr_review must re-emit with review_commit_sha on upgrade"
    );
    let inline_comment = review_by_native(&j2, 501);
    assert_eq!(inline_comment["review_kind"], "pr_review_comment");
    assert_eq!(inline_comment["review_commit_sha"], RC_SHA_A);
    assert_eq!(
        edges_of_label(&j2, "REVIEWS_COMMIT"),
        2,
        "both review anchors (summary + comment) re-emit their REVIEWS_COMMIT edge"
    );

    // Issue #335 changed the migration to a FULL refresh: v2/v3 → v4 clears every
    // per-resource hash (not just the review-family ones), because the #335
    // reviewer-identity facts re-use the unchanged #334 review-hash formula, so a
    // preserved hash would MATCH and suppress the new REVIEWED_BY /
    // REQUESTED_REVIEW_FROM edges. So on the migrated run EVERY issue, PR, and
    // review re-emits exactly once. The seed graph is unchanged, so no artifact is
    // superseded and none is tombstoned (prior == current merge artifact).
    assert!(
        tombstones334(&j2).is_empty(),
        "no artifact is tombstoned when the seed graph is unchanged: {j2}"
    );
    // The PR Task re-emits on the full refresh (its cleared hash forces it), so its
    // MERGED_AS edge re-emits too — byte-identical, deduped by the store.
    assert_eq!(
        edges_of_label(&j2, "MERGED_AS"),
        1,
        "the full v4 refresh re-emits the PR and its MERGED_AS edge"
    );
    assert!(
        !nodes_of_kind(&j2, "Task").is_empty(),
        "the v4 migration is a full refresh: every Task re-emits once"
    );
}

// ── Issue #335: reviewer identity ────────────────────────────────────────────

/// Extracts the `author` login from every `ExternalIdentity` node in the JSONL.
fn identity_logins(jsonl: &str) -> Vec<String> {
    nodes_of_kind(jsonl, "ExternalIdentity")
        .into_iter()
        .filter_map(|v| v["author"].as_str().map(str::to_owned))
        .collect()
}

/// The reviewer-identity fixture (issue #335, AC8): two PRs exercising the
/// segregation-of-duties cases with >=5 distinct logins.
///
/// - PR #1 author `carol`; requests review from `dave` + `erin` and team
///   `backend`; approved by NON-author `dave`. `carol` leaves NO review and NO
///   comment — her `ExternalIdentity` is minted purely from `pr.user` (issue
///   #335, AC1; Codex P2), which is exactly what the segregation-of-duties join
///   needs (a citable author identity to subtract from the approver set).
/// - PR #2 author `frank`; requests review from `grace`; approved by author
///   `frank` (self-approval).
///
/// Distinct logins: carol, dave, erin, frank, grace (5). `backend` is a team,
/// recorded as a diagnostic, never a login. `carol` and `frank` appear as
/// identities solely by authoring their PRs.
fn reviewer_identity_routes() -> HashMap<String, Canned> {
    let pulls = serde_json::json!([
        {
            "number": 1, "title": "PR one", "body": null, "state": "closed",
            "merged_at": "2026-01-05T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "carol"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-05T00:00:00Z",
            "requested_reviewers": [{"login": "dave"}, {"login": "erin"}],
            "requested_teams": [{"slug": "backend"}],
            "html_url": "https://github.com/o/r/pull/1"
        },
        {
            "number": 2, "title": "PR two", "body": null, "state": "closed",
            "merged_at": "2026-01-06T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "frank"},
            "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-06T00:00:00Z",
            "requested_reviewers": [{"login": "grace"}],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/2"
        }
    ])
    .to_string();
    let reviews_1 = serde_json::json!([
        {
            "id": 901, "body": "LGTM", "state": "APPROVED", "user": {"login": "dave"},
            "submitted_at": "2026-01-04T00:00:00Z",
            "html_url": "https://github.com/o/r/pull/1#pullrequestreview-901"
        }
    ])
    .to_string();
    let reviews_2 = serde_json::json!([
        {
            "id": 902, "body": "self approve", "state": "APPROVED", "user": {"login": "frank"},
            "submitted_at": "2026-01-05T00:00:00Z",
            "html_url": "https://github.com/o/r/pull/2#pullrequestreview-902"
        }
    ])
    .to_string();
    // The PR author (carol) leaves no comment: her identity must come from
    // `pr.user`, not a conversation comment (Codex P2).
    let issue_comments = "[]".to_owned();

    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, "\"pulls-335\""),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok(&issue_comments, "\"ic-335\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"prc-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/1/reviews?per_page=100".to_owned(),
        Canned::ok(&reviews_1, "\"prr-1\""),
    );
    routes.insert(
        "/repos/o/r/pulls/2/reviews?per_page=100".to_owned(),
        Canned::ok(&reviews_2, "\"prr-2\""),
    );
    routes
}

#[test]
fn fresh_import_emits_reviewer_identities_edges_and_team_diagnostic() {
    let server = MockServer::start(reviewer_identity_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("g.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok, "import should succeed");

    // Exactly one identity node per distinct login (carol, dave, erin, frank, grace).
    let mut logins = identity_logins(&jsonl);
    logins.sort();
    logins.dedup();
    let mut all_logins = identity_logins(&jsonl);
    all_logins.sort();
    assert_eq!(
        all_logins, logins,
        "each (system, login) is minted exactly once: {all_logins:?}"
    );
    assert_eq!(
        logins,
        vec!["carol", "dave", "erin", "frank", "grace"],
        "5 distinct participant identities, backend team excluded"
    );

    // REVIEWED_BY: one per emitted review (2 pr_review approvals; the authors
    // carol/frank leave no review or comment).
    assert_eq!(
        edges_of_label(&jsonl, "REVIEWED_BY"),
        2,
        "one REVIEWED_BY per emitted review (2 approvals, no comments)"
    );
    // REQUESTED_REVIEW_FROM: dave, erin (PR#1) + grace (PR#2) = 3.
    assert_eq!(
        edges_of_label(&jsonl, "REQUESTED_REVIEW_FROM"),
        3,
        "one REQUESTED_REVIEW_FROM per requested reviewer login"
    );

    // The requested TEAM is a diagnostic, never a login, never expanded.
    let team_diag = nodes_of_kind(&jsonl, "Diagnostic")
        .into_iter()
        .filter(|v| {
            v["summary"]
                .as_str()
                .is_some_and(|s| s.contains("github_team_review_request_unexpanded"))
        })
        .count();
    assert_eq!(
        team_diag, 1,
        "the requested team is recorded as one diagnostic"
    );
    assert!(
        !logins.contains(&"backend".to_owned()),
        "a team is never expanded into a member identity"
    );

    // Segregation of duties (AC8): compute {approving identity} minus {author}.
    // PR #1: author carol, approved by dave → self_approval == false.
    // PR #2: author frank, approved by frank → self_approval == true.
    let carol_id = identity_id_of(&jsonl, "carol");
    let frank_id = identity_id_of(&jsonl, "frank");
    // Both author identities are citable and byte-stable (Codex P2): carol's
    // node exists SOLELY because she authored PR #1 (she left no review/comment
    // and is not a requested reviewer), so the id equals the one minted from any
    // other source for the same login.
    assert_eq!(
        carol_id,
        aletheia_egregore::github::records::external_identity_id("github", "carol"),
        "carol's author identity id is the stable (system, login) id"
    );
    assert!(
        !jsonl
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| v["record_type"] == "edge"
                && (v["label"] == "REVIEWED_BY" || v["label"] == "REQUESTED_REVIEW_FROM")
                && v["target"].as_str() == Some(&carol_id)),
        "carol is neither a reviewer nor a requested reviewer: her identity comes only from pr.user"
    );
    let pr1_approvers = reviewed_by_identity_ids_for_pr(&jsonl, 1);
    let pr2_approvers = reviewed_by_identity_ids_for_pr(&jsonl, 2);
    // {approving identities} minus {author identity} is computable in both cases.
    assert!(
        !pr1_approvers.contains(&carol_id),
        "PR#1 (non-author approval) must NOT be flagged as self-approval"
    );
    assert_eq!(
        pr1_approvers
            .difference(&std::collections::BTreeSet::from([carol_id]))
            .count(),
        pr1_approvers.len(),
        "author id subtracts cleanly from PR#1's approver set (no false self-approval)"
    );
    assert!(
        pr2_approvers.contains(&frank_id),
        "PR#2 (author-approved-own-PR) must be flagged as self-approval"
    );

    // Byte-stable across 5 consecutive runs (a fresh store + state each time).
    for i in 0..5 {
        let out_n = tmp.path().join(format!("g_{i}.jsonl"));
        let state_n = tmp.path().join(format!("state_{i}.json"));
        let (jn, _, okn) = run_import(&server.base_url, &out_n, &state_n, &[]);
        assert!(okn);
        assert_eq!(
            jn, jsonl,
            "import output must be byte-identical across runs"
        );
    }

    // `eg inspect` counts the identities under (project, ExternalIdentity, 1).
    let inspect = egregore()
        .args(["inspect", out.to_str().unwrap()])
        .env_remove("PATH")
        .env_remove("Path")
        .output()
        .expect("inspect runs");
    let inspect_out = String::from_utf8_lossy(&inspect.stdout);
    assert!(
        inspect_out.contains("ExternalIdentity"),
        "inspect must count ExternalIdentity nodes: {inspect_out}"
    );
}

#[test]
fn changed_requested_reviewers_reemit_but_unchanged_pr_stays_suppressed() {
    // AC5/idempotency: a PR whose requested-reviewer set changes re-emits its
    // REQUESTED_REVIEW_FROM edges; an unchanged PR stays suppressed on re-import.
    let server = MockServer::start(reviewer_identity_routes());
    let tmp = TempDir::new().unwrap();
    let out1 = tmp.path().join("g1.jsonl");
    let state = tmp.path().join("state.json");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    assert_eq!(edges_of_label(&j1, "REQUESTED_REVIEW_FROM"), 3);

    // Re-import unchanged → nothing re-emits (no new request edges).
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);
    assert_eq!(
        edges_of_label(&j2, "REQUESTED_REVIEW_FROM"),
        0,
        "an unchanged PR must not re-emit its request edges"
    );

    // Change PR #1's requested reviewers (drop erin, add heidi) + bump updated_at.
    let mut routes = reviewer_identity_routes();
    let pulls = serde_json::json!([
        {
            "number": 1, "title": "PR one", "body": null, "state": "closed",
            "merged_at": "2026-01-05T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "carol"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-07T00:00:00Z",
            "requested_reviewers": [{"login": "dave"}, {"login": "heidi"}],
            "requested_teams": [{"slug": "backend"}],
            "html_url": "https://github.com/o/r/pull/1"
        },
        {
            "number": 2, "title": "PR two", "body": null, "state": "closed",
            "merged_at": "2026-01-06T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "frank"},
            "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-06T00:00:00Z",
            "requested_reviewers": [{"login": "grace"}],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/2"
        }
    ])
    .to_string();
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, "\"pulls-335-v2\""),
    );
    server.set_routes(routes);

    let out3 = tmp.path().join("g3.jsonl");
    let (j3, _, ok3) = run_import(&server.base_url, &out3, &state, &[]);
    assert!(ok3);
    // PR #1 re-emits its 2 request edges (dave, heidi); PR #2 unchanged → 0.
    assert_eq!(
        edges_of_label(&j3, "REQUESTED_REVIEW_FROM"),
        2,
        "the changed PR re-emits its request edges; the unchanged PR does not"
    );
    assert!(
        identity_logins(&j3).contains(&"heidi".to_owned()),
        "the newly-requested reviewer identity is minted"
    );
}

/// The stable `ExternalIdentity` record id for a `github` login, read from the
/// emitted JSONL (matches `external_identity_id`).
fn identity_id_of(jsonl: &str, login: &str) -> String {
    nodes_of_kind(jsonl, "ExternalIdentity")
        .into_iter()
        .find(|v| v["author"].as_str() == Some(login))
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("identity for {login} must exist"))
}

/// The set of identity record ids that APPROVED PR `n` (reached by `REVIEWED_BY`
/// from `pr_review` `Review` nodes whose `review_state` is `approved`).
///
/// This deliberately excludes non-approving reviews such as the PR author's
/// conversation comment (an `issue_comment` Review), so the segregation-of-duties
/// computation `{approving identities} minus {author identity}` is exact.
fn reviewed_by_identity_ids_for_pr(jsonl: &str, n: u64) -> std::collections::BTreeSet<String> {
    let pr_task = pr_task(jsonl, n);
    let task_id = pr_task["id"].as_str().unwrap().to_owned();
    // Approving review record ids: reviews with review_state == "approved".
    let approving_review_ids: std::collections::BTreeSet<String> = nodes_of_kind(jsonl, "Review")
        .into_iter()
        .filter(|v| v["review_state"].as_str() == Some("approved"))
        .filter_map(|v| v["id"].as_str().map(str::to_owned))
        .collect();
    // Of those, the ones referencing this PR's Task.
    let review_ids: std::collections::BTreeSet<String> = jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            v["record_type"] == "edge"
                && v["label"] == "REFERENCES_TASK"
                && v["target"].as_str() == Some(&task_id)
                && v["source"]
                    .as_str()
                    .is_some_and(|s| approving_review_ids.contains(s))
        })
        .filter_map(|v| v["source"].as_str().map(str::to_owned))
        .collect();
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| {
            v["record_type"] == "edge"
                && v["label"] == "REVIEWED_BY"
                && v["source"].as_str().is_some_and(|s| review_ids.contains(s))
        })
        .filter_map(|v| v["target"].as_str().map(str::to_owned))
        .collect()
}

// ── Issue #335 (Codex P1): removed requested reviewers are tombstoned ─────────
//
// The importer emits `REQUESTED_REVIEW_FROM` edges for the reviewers CURRENTLY
// in a PR's `requested_reviewers`. When that set shrinks (a reviewer approves,
// the PR merges/closes, or a reviewer is manually removed) the importer is
// otherwise purely additive, so the previously-emitted edge for the removed
// reviewer would linger LIVE in a persistent store and downstream queries would
// still report the removed reviewer as "requested". The changed set must retract
// each dropped reviewer's edge via a `Tombstone(deleted_id == edge_id)`,
// mirroring the #333/#334 supersession discipline — and only the edge, never the
// global `ExternalIdentity` node and never any immutable `REVIEWED_BY` edge.

/// The `REQUESTED_REVIEW_FROM` edge record id linking PR `n`'s `Task` to
/// `login`'s `ExternalIdentity`, read from the emitted JSONL.
fn request_edge_id_for(jsonl: &str, n: u64, login: &str) -> String {
    let task_id = pr_task(jsonl, n)["id"].as_str().unwrap().to_owned();
    let identity_id = identity_id_of(jsonl, login);
    jsonl
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find(|v| {
            v["record_type"] == "edge"
                && v["label"] == "REQUESTED_REVIEW_FROM"
                && v["source"].as_str() == Some(&task_id)
                && v["target"].as_str() == Some(&identity_id)
        })
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("REQUESTED_REVIEW_FROM edge for {login} on PR#{n} must exist"))
}

/// `reviewer_identity_routes` with PR #1's requested reviewers, `updated_at`, and
/// the `/pulls` `ETag` overridden — so a re-import can shrink or grow the request
/// set while PR #2 stays byte-identical.
fn reviewer_routes_pr1(
    reviewers: &[&str],
    updated_at: &str,
    pulls_etag: &str,
) -> HashMap<String, Canned> {
    let mut routes = reviewer_identity_routes();
    let reviewer_json: Vec<serde_json::Value> = reviewers
        .iter()
        .map(|l| serde_json::json!({ "login": l }))
        .collect();
    let pulls = serde_json::json!([
        {
            "number": 1, "title": "PR one", "body": null, "state": "closed",
            "merged_at": "2026-01-05T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "carol"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": updated_at,
            "requested_reviewers": reviewer_json,
            "requested_teams": [{"slug": "backend"}],
            "html_url": "https://github.com/o/r/pull/1"
        },
        {
            "number": 2, "title": "PR two", "body": null, "state": "closed",
            "merged_at": "2026-01-06T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "frank"},
            "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-06T00:00:00Z",
            "requested_reviewers": [{"login": "grace"}],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/2"
        }
    ])
    .to_string();
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, pulls_etag),
    );
    routes
}

#[test]
fn removed_requested_reviewer_edge_is_tombstoned_survivor_and_identity_untouched() {
    let server = MockServer::start(reviewer_identity_routes());
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");

    // 1. Fresh import: PR #1 requests [dave, erin], PR #2 requests [grace] → 3
    //    REQUESTED_REVIEW_FROM edges. Capture erin's edge id and identity id.
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    assert_eq!(edges_of_label(&j1, "REQUESTED_REVIEW_FROM"), 3);
    let erin_edge_id = request_edge_id_for(&j1, 1, "erin");
    let erin_identity_id = identity_id_of(&j1, "erin");

    // 2. Re-import with erin dropped from PR #1 (now [dave] only). The removed
    //    reviewer's edge must be retracted via exactly one Tombstone; dave's edge
    //    stays live; PR #2 is unchanged.
    server.set_routes(reviewer_routes_pr1(
        &["dave"],
        "2026-01-07T00:00:00Z",
        "\"pulls-335-drop-erin\"",
    ));
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);

    let ts = tombstones(&j2);
    assert_eq!(
        ts.len(),
        1,
        "exactly one Tombstone (erin's removed request edge) must be emitted: {j2}"
    );
    assert_eq!(
        ts[0]["deleted_id"].as_str(),
        Some(erin_edge_id.as_str()),
        "the Tombstone must retract erin's REQUESTED_REVIEW_FROM edge id"
    );
    // The surviving reviewer's edge is re-emitted live; PR #2 stays suppressed.
    assert_eq!(
        edges_of_label(&j2, "REQUESTED_REVIEW_FROM"),
        1,
        "only the surviving reviewer (dave) re-emits a live request edge"
    );
    let dave_id = identity_id_of(&j1, "dave");
    assert!(
        j2.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| v["record_type"] == "edge"
                && v["label"] == "REQUESTED_REVIEW_FROM"
                && v["target"].as_str() == Some(&dave_id)),
        "dave's request edge stays live: {j2}"
    );
    // The global ExternalIdentity node is NEVER tombstoned (a login persists
    // across PRs), and no immutable REVIEWED_BY edge is tombstoned.
    assert!(
        !ts.iter()
            .any(|t| t["deleted_id"].as_str() == Some(erin_identity_id.as_str())),
        "erin's ExternalIdentity node must NOT be tombstoned"
    );
    let reviewed_by_ids: std::collections::BTreeSet<String> = j1
        .lines()
        .chain(j2.lines())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["record_type"] == "edge" && v["label"] == "REVIEWED_BY")
        .filter_map(|v| v["id"].as_str().map(str::to_owned))
        .collect();
    assert!(
        !ts.iter().any(|t| t["deleted_id"]
            .as_str()
            .is_some_and(|d| reviewed_by_ids.contains(d))),
        "no REVIEWED_BY edge may be tombstoned"
    );
}

#[test]
fn re_requested_reviewer_edge_is_revived_and_unchanged_import_emits_no_tombstones() {
    let server = MockServer::start(reviewer_identity_routes());
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");

    // 1. Fresh import: PR #1 requests [dave, erin].
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    let erin_edge_id = request_edge_id_for(&j1, 1, "erin");

    // 2. Drop erin → erin's edge is tombstoned.
    server.set_routes(reviewer_routes_pr1(
        &["dave"],
        "2026-01-07T00:00:00Z",
        "\"pulls-335-drop\"",
    ));
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);
    assert!(
        tombstones(&j2)
            .iter()
            .any(|t| t["deleted_id"].as_str() == Some(erin_edge_id.as_str())),
        "dropping erin tombstones her request edge"
    );

    // 3. Unchanged re-import (same [dave] payload, forced 200 via a new ETag):
    //    the reviewer set is identical, so NO tombstone is emitted and no request
    //    edge re-emits (idempotency / AC8).
    server.set_routes(reviewer_routes_pr1(
        &["dave"],
        "2026-01-07T00:00:00Z",
        "\"pulls-335-drop-again\"",
    ));
    let out3 = tmp.path().join("g3.jsonl");
    let (j3, _, ok3) = run_import(&server.base_url, &out3, &state, &[]);
    assert!(ok3);
    assert_eq!(
        tombstones(&j3).len(),
        0,
        "an unchanged reviewer set emits zero tombstones: {j3}"
    );
    assert_eq!(
        edges_of_label(&j3, "REQUESTED_REVIEW_FROM"),
        0,
        "an unchanged reviewer set re-emits no request edges"
    );

    // 4. Re-request erin ([dave, erin] again): her edge is REVIVED — re-emitted
    //    live with the SAME stable id — and, because nothing was removed, no new
    //    tombstone fires. The embedded adapter's write_edge revive-after-tombstone
    //    (a fresh edge write post-dating the tombstone) then supersedes the
    //    tombstone in a persistent store.
    server.set_routes(reviewer_routes_pr1(
        &["dave", "erin"],
        "2026-01-08T00:00:00Z",
        "\"pulls-335-readd\"",
    ));
    let out4 = tmp.path().join("g4.jsonl");
    let (j4, _, ok4) = run_import(&server.base_url, &out4, &state, &[]);
    assert!(ok4);
    assert_eq!(
        request_edge_id_for(&j4, 1, "erin"),
        erin_edge_id,
        "the revived edge carries the same stable id"
    );
    assert_eq!(
        edges_of_label(&j4, "REQUESTED_REVIEW_FROM"),
        2,
        "both reviewers (dave, erin) re-emit live edges on the re-request"
    );
    assert!(
        !tombstones(&j4)
            .iter()
            .any(|t| t["deleted_id"].as_str() == Some(erin_edge_id.as_str())),
        "re-requesting erin emits no fresh tombstone for her edge: {j4}"
    );
}

// ── Issue #335 (Codex P2): removed requested teams are tombstoned ─────────────
//
// The importer emits one `github_team_review_request_unexpanded` Diagnostic per
// team CURRENTLY in a PR's `requested_teams` — the exact sibling of the
// REQUESTED_REVIEW_FROM edge case above, and with the exact same staleness gap.
// When that team set shrinks (a team is removed or replaced) the importer is
// otherwise purely additive, so the previously-emitted diagnostic for the removed
// team would linger LIVE in a persistent store and current-state queries would
// still report the removed team's review request. The changed set must retract
// each dropped team's diagnostic via a `Tombstone(deleted_id == diagnostic_id)`,
// mirroring the reviewer-edge supersession discipline — and only the team
// diagnostic, never a reviewer edge, identity node, or REVIEWED_BY edge.

/// The `github_team_review_request_unexpanded` Diagnostic record id for team
/// `slug` on PR `n`, read from the emitted JSONL.
fn team_diagnostic_id_for(jsonl: &str, n: u64, slug: &str) -> String {
    let needle_pr = format!("PR #{n} ");
    let needle_team = format!("team '{slug}'");
    nodes_of_kind(jsonl, "Diagnostic")
        .into_iter()
        .find(|v| {
            v["summary"].as_str().is_some_and(|s| {
                s.contains("github_team_review_request_unexpanded")
                    && s.contains(&needle_pr)
                    && s.contains(&needle_team)
            })
        })
        .and_then(|v| v["id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| panic!("team diagnostic for {slug} on PR#{n} must exist"))
}

/// Count of live `github_team_review_request_unexpanded` Diagnostic nodes in the
/// JSONL (across all PRs).
fn team_diagnostic_count(jsonl: &str) -> usize {
    nodes_of_kind(jsonl, "Diagnostic")
        .into_iter()
        .filter(|v| {
            v["summary"]
                .as_str()
                .is_some_and(|s| s.contains("github_team_review_request_unexpanded"))
        })
        .count()
}

/// The tombstones whose own summary marks them as team-diagnostic supersessions.
fn team_tombstones(jsonl: &str) -> Vec<serde_json::Value> {
    tombstones(jsonl)
        .into_iter()
        .filter(|t| {
            t["summary"]
                .as_str()
                .is_some_and(|s| s.contains("team_review_request_superseded"))
        })
        .collect()
}

/// `reviewer_identity_routes` with PR #1's requested TEAMS, `updated_at`, and the
/// `/pulls` `ETag` overridden — reviewers stay [dave, erin] so the team set can be
/// shrunk or grown while the reviewer edges stay constant and PR #2 stays
/// byte-identical.
fn reviewer_routes_pr1_teams(
    teams: &[&str],
    updated_at: &str,
    pulls_etag: &str,
) -> HashMap<String, Canned> {
    let mut routes = reviewer_identity_routes();
    let team_json: Vec<serde_json::Value> = teams
        .iter()
        .map(|s| serde_json::json!({ "slug": s }))
        .collect();
    let pulls = serde_json::json!([
        {
            "number": 1, "title": "PR one", "body": null, "state": "closed",
            "merged_at": "2026-01-05T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "carol"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": updated_at,
            "requested_reviewers": [{"login": "dave"}, {"login": "erin"}],
            "requested_teams": team_json,
            "html_url": "https://github.com/o/r/pull/1"
        },
        {
            "number": 2, "title": "PR two", "body": null, "state": "closed",
            "merged_at": "2026-01-06T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "frank"},
            "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-06T00:00:00Z",
            "requested_reviewers": [{"login": "grace"}],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/2"
        }
    ])
    .to_string();
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, pulls_etag),
    );
    routes
}

#[test]
fn removed_requested_team_diagnostic_is_tombstoned_survivor_and_reviewers_untouched() {
    let server = MockServer::start(reviewer_routes_pr1_teams(
        &["backend", "frontend"],
        "2026-01-05T00:00:00Z",
        "\"pulls-335-teams-both\"",
    ));
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");

    // 1. Fresh import: PR #1 requests teams [backend, frontend] → 2 team
    //    diagnostics. Capture backend's diagnostic id.
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    assert_eq!(team_diagnostic_count(&j1), 2, "two team diagnostics: {j1}");
    let backend_diag_id = team_diagnostic_id_for(&j1, 1, "backend");
    let frontend_diag_id = team_diagnostic_id_for(&j1, 1, "frontend");

    // 2. Re-import with backend dropped from PR #1 (now [frontend] only). The
    //    removed team's diagnostic must be retracted via exactly one Tombstone;
    //    frontend's diagnostic stays live; reviewers/identities are untouched.
    server.set_routes(reviewer_routes_pr1_teams(
        &["frontend"],
        "2026-01-07T00:00:00Z",
        "\"pulls-335-teams-drop-backend\"",
    ));
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);

    let team_ts = team_tombstones(&j2);
    assert_eq!(
        team_ts.len(),
        1,
        "exactly one team Tombstone (backend's removed diagnostic) must be emitted: {j2}"
    );
    assert_eq!(
        team_ts[0]["deleted_id"].as_str(),
        Some(backend_diag_id.as_str()),
        "the Tombstone must retract backend's team-review diagnostic id"
    );
    // No REQUESTED_REVIEW_FROM edge is tombstoned — only the team diagnostic.
    assert_eq!(
        tombstones(&j2).len(),
        1,
        "only the team diagnostic is tombstoned, no reviewer edge: {j2}"
    );
    // The surviving team's diagnostic is re-emitted live with its same stable id.
    assert_eq!(
        team_diagnostic_id_for(&j2, 1, "frontend"),
        frontend_diag_id,
        "the surviving team's diagnostic keeps its stable id"
    );
    assert_eq!(
        team_diagnostic_count(&j2),
        1,
        "only the surviving team (frontend) re-emits a live diagnostic: {j2}"
    );
    // Reviewers on PR #1 are unchanged, so their identities and edges re-emit
    // exactly as before and none is tombstoned.
    assert_eq!(
        edges_of_label(&j2, "REQUESTED_REVIEW_FROM"),
        2,
        "both reviewers (dave, erin) re-emit their live edges; none is tombstoned"
    );
    let dave_edge = request_edge_id_for(&j1, 1, "dave");
    let erin_edge = request_edge_id_for(&j1, 1, "erin");
    assert!(
        !tombstones(&j2).iter().any(|t| {
            let d = t["deleted_id"].as_str();
            d == Some(dave_edge.as_str()) || d == Some(erin_edge.as_str())
        }),
        "no reviewer edge may be tombstoned: {j2}"
    );
}

#[test]
fn re_requested_team_diagnostic_is_revived_and_unchanged_import_emits_no_team_tombstones() {
    let server = MockServer::start(reviewer_routes_pr1_teams(
        &["backend", "frontend"],
        "2026-01-05T00:00:00Z",
        "\"pulls-335-teams-both-2\"",
    ));
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");

    // 1. Fresh import: PR #1 requests teams [backend, frontend].
    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    let backend_diag_id = team_diagnostic_id_for(&j1, 1, "backend");

    // 2. Drop backend → backend's diagnostic is tombstoned.
    server.set_routes(reviewer_routes_pr1_teams(
        &["frontend"],
        "2026-01-07T00:00:00Z",
        "\"pulls-335-teams-drop\"",
    ));
    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);
    assert!(
        team_tombstones(&j2)
            .iter()
            .any(|t| t["deleted_id"].as_str() == Some(backend_diag_id.as_str())),
        "dropping backend tombstones its team diagnostic"
    );

    // 3. Unchanged re-import (same [frontend] payload, forced 200 via a new ETag):
    //    the team set is identical, so NO team tombstone is emitted and no team
    //    diagnostic re-emits (idempotency).
    server.set_routes(reviewer_routes_pr1_teams(
        &["frontend"],
        "2026-01-07T00:00:00Z",
        "\"pulls-335-teams-drop-again\"",
    ));
    let out3 = tmp.path().join("g3.jsonl");
    let (j3, _, ok3) = run_import(&server.base_url, &out3, &state, &[]);
    assert!(ok3);
    assert_eq!(
        team_tombstones(&j3).len(),
        0,
        "an unchanged team set emits zero team tombstones: {j3}"
    );
    assert_eq!(
        team_diagnostic_count(&j3),
        0,
        "an unchanged team set re-emits no team diagnostics"
    );

    // 4. Re-request backend ([backend, frontend] again): its diagnostic is
    //    REVIVED — re-emitted live with the SAME stable id — and, because nothing
    //    was removed, no new team tombstone fires. The embedded adapter's
    //    revive-after-tombstone (a fresh node write post-dating the tombstone)
    //    then supersedes the tombstone in a persistent store.
    server.set_routes(reviewer_routes_pr1_teams(
        &["backend", "frontend"],
        "2026-01-08T00:00:00Z",
        "\"pulls-335-teams-readd\"",
    ));
    let out4 = tmp.path().join("g4.jsonl");
    let (j4, _, ok4) = run_import(&server.base_url, &out4, &state, &[]);
    assert!(ok4);
    assert_eq!(
        team_diagnostic_id_for(&j4, 1, "backend"),
        backend_diag_id,
        "the revived team diagnostic carries the same stable id"
    );
    assert_eq!(
        team_diagnostic_count(&j4),
        2,
        "both teams (backend, frontend) re-emit live diagnostics on the re-request"
    );
    assert!(
        !team_tombstones(&j4)
            .iter()
            .any(|t| t["deleted_id"].as_str() == Some(backend_diag_id.as_str())),
        "re-requesting backend emits no fresh tombstone for its diagnostic: {j4}"
    );
}

#[test]
fn changing_both_reviewers_and_teams_tombstones_both() {
    // Bonus combined case: a PR that changes BOTH its reviewer set and its team
    // set on one re-import must tombstone the removed reviewer's edge AND the
    // removed team's diagnostic — the two supersession lanes are independent.
    let server = MockServer::start(reviewer_routes_pr1_teams(
        &["backend", "frontend"],
        "2026-01-05T00:00:00Z",
        "\"pulls-335-both-lanes\"",
    ));
    let tmp = TempDir::new().unwrap();
    let state = tmp.path().join("state.json");

    let out1 = tmp.path().join("g1.jsonl");
    let (j1, _, ok1) = run_import(&server.base_url, &out1, &state, &[]);
    assert!(ok1);
    let erin_edge_id = request_edge_id_for(&j1, 1, "erin");
    let backend_diag_id = team_diagnostic_id_for(&j1, 1, "backend");

    // Drop erin (reviewers → [dave]) AND drop backend (teams → [frontend]).
    let mut routes = reviewer_routes_pr1_teams(
        &["frontend"],
        "2026-01-09T00:00:00Z",
        "\"pulls-335-both-lanes-v2\"",
    );
    let pulls = serde_json::json!([
        {
            "number": 1, "title": "PR one", "body": null, "state": "closed",
            "merged_at": "2026-01-05T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "carol"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-09T00:00:00Z",
            "requested_reviewers": [{"login": "dave"}],
            "requested_teams": [{"slug": "frontend"}],
            "html_url": "https://github.com/o/r/pull/1"
        },
        {
            "number": 2, "title": "PR two", "body": null, "state": "closed",
            "merged_at": "2026-01-06T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "frank"},
            "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-06T00:00:00Z",
            "requested_reviewers": [{"login": "grace"}],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/2"
        }
    ])
    .to_string();
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, "\"pulls-335-both-lanes-v2\""),
    );
    server.set_routes(routes);

    let out2 = tmp.path().join("g2.jsonl");
    let (j2, _, ok2) = run_import(&server.base_url, &out2, &state, &[]);
    assert!(ok2);

    let ts = tombstones(&j2);
    assert!(
        ts.iter()
            .any(|t| t["deleted_id"].as_str() == Some(erin_edge_id.as_str())),
        "erin's removed request edge is tombstoned: {j2}"
    );
    assert!(
        ts.iter()
            .any(|t| t["deleted_id"].as_str() == Some(backend_diag_id.as_str())),
        "backend's removed team diagnostic is tombstoned: {j2}"
    );
    assert_eq!(
        ts.len(),
        2,
        "exactly two tombstones: one reviewer edge + one team diagnostic: {j2}"
    );
}

// ── Issue #335 (Codex P2): PR-author identities are minted from `pr.user` ──────
//
// A PR whose author never appears as a requested reviewer, review author, or
// commenter still needs a citable `ExternalIdentity` so the segregation-of-duties
// join (match the PR `Task.author` login to the approver identity set) has an
// author identity to compare against. The author identity is minted from
// `pr.user` — NODE ONLY, no authorship edge — and deduplicated one-per-login by
// the run's identity seen-set, so an author who is ALSO a reviewer/requested
// reviewer still yields exactly one identity node.

/// Fixture exercising the author-identity source (issue #335, Codex P2):
///
/// - PR #10 author `ivan` — a login that appears ONLY as `pr.user` (no review,
///   no comment, not a requested reviewer); requests review from `dave`, approved
///   by `dave`.
/// - PR #11 author `dave` — `dave` is simultaneously a requested reviewer AND a
///   review author (both on PR #10) AND a PR author (PR #11), so his identity is
///   minted from three sources and MUST collapse to exactly one node.
///
/// Distinct identities: `ivan`, `dave` (2).
fn author_identity_routes() -> HashMap<String, Canned> {
    let pulls = serde_json::json!([
        {
            "number": 10, "title": "PR ten", "body": null, "state": "closed",
            "merged_at": "2026-01-05T00:00:00Z", "draft": false, "labels": [],
            "assignees": [], "user": {"login": "ivan"},
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-05T00:00:00Z",
            "requested_reviewers": [{"login": "dave"}],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/10"
        },
        {
            "number": 11, "title": "PR eleven", "body": null, "state": "open",
            "merged_at": null, "draft": false, "labels": [],
            "assignees": [], "user": {"login": "dave"},
            "created_at": "2026-01-02T00:00:00Z", "updated_at": "2026-01-06T00:00:00Z",
            "requested_reviewers": [],
            "requested_teams": [],
            "html_url": "https://github.com/o/r/pull/11"
        }
    ])
    .to_string();
    let reviews_10 = serde_json::json!([
        {
            "id": 910, "body": "LGTM", "state": "APPROVED", "user": {"login": "dave"},
            "submitted_at": "2026-01-04T00:00:00Z",
            "html_url": "https://github.com/o/r/pull/10#pullrequestreview-910"
        }
    ])
    .to_string();

    let mut routes = HashMap::new();
    routes.insert(
        "/repos/o/r".to_owned(),
        Canned::ok("{\"full_name\":\"o/r\"}", "\"repo\""),
    );
    routes.insert(
        "/repos/o/r/issues?state=all&per_page=100".to_owned(),
        Canned::ok("[]", "\"issues-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls?state=all&per_page=100".to_owned(),
        Canned::ok(&pulls, "\"pulls-335-author\""),
    );
    routes.insert(
        "/repos/o/r/labels?per_page=100".to_owned(),
        Canned::ok("[]", "\"labels-empty\""),
    );
    routes.insert(
        "/repos/o/r/issues/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"ic-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/comments?per_page=100".to_owned(),
        Canned::ok("[]", "\"prc-empty\""),
    );
    routes.insert(
        "/repos/o/r/pulls/10/reviews?per_page=100".to_owned(),
        Canned::ok(&reviews_10, "\"prr-10\""),
    );
    routes.insert(
        "/repos/o/r/pulls/11/reviews?per_page=100".to_owned(),
        Canned::ok("[]", "\"prr-11\""),
    );
    routes
}

#[test]
fn pr_author_only_login_gets_one_identity_from_pr_user() {
    let server = MockServer::start(author_identity_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("g.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok, "import should succeed");

    // `ivan` appears ONLY as pr.user, yet has exactly one citable identity with
    // the stable (system, login) id.
    let ivan_nodes: Vec<_> = nodes_of_kind(&jsonl, "ExternalIdentity")
        .into_iter()
        .filter(|v| v["author"].as_str() == Some("ivan"))
        .collect();
    assert_eq!(
        ivan_nodes.len(),
        1,
        "the author-only login ivan gets exactly one ExternalIdentity node"
    );
    assert_eq!(
        ivan_nodes[0]["id"].as_str(),
        Some(aletheia_egregore::github::records::external_identity_id("github", "ivan").as_str()),
        "ivan's author identity id is the stable (system, login) id"
    );
    // NODE ONLY: no edge references ivan (he authored no review and is not a
    // requested reviewer), so his identity exists solely because of pr.user.
    let ivan_id = aletheia_egregore::github::records::external_identity_id("github", "ivan");
    assert!(
        !jsonl
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| v["record_type"] == "edge"
                && (v["source"].as_str() == Some(&ivan_id)
                    || v["target"].as_str() == Some(&ivan_id))),
        "no edge references the author-only identity: {jsonl}"
    );

    // Segregation of duties: PR #10 approvers ({dave}) minus author ({ivan}) is
    // computable, and ivan is NOT in the approver set (non-self-approval).
    let ivan_task_author = pr_task(&jsonl, 10)["author"].as_str().map(str::to_owned);
    assert_eq!(
        ivan_task_author.as_deref(),
        Some("ivan"),
        "the PR Task carries the author login the identity is minted from"
    );
    let pr10_approvers = reviewed_by_identity_ids_for_pr(&jsonl, 10);
    assert!(
        !pr10_approvers.contains(&ivan_id),
        "PR#10 non-author approval is not flagged as self-approval"
    );

    // Byte-stable across 5 fresh runs.
    for i in 0..5 {
        let out_n = tmp.path().join(format!("g_{i}.jsonl"));
        let state_n = tmp.path().join(format!("state_{i}.json"));
        let (jn, _, okn) = run_import(&server.base_url, &out_n, &state_n, &[]);
        assert!(okn);
        assert_eq!(
            jn, jsonl,
            "import output must be byte-identical across runs"
        );
    }
}

#[test]
fn author_who_is_also_reviewer_and_review_author_yields_one_identity() {
    // `dave` is a requested reviewer (PR #10), a review author (PR #10 approval),
    // AND a PR author (PR #11). Despite three identity sources, the run's identity
    // seen-set must collapse him to exactly ONE ExternalIdentity node — zero
    // duplicates — so byte-stability and idempotency hold.
    let server = MockServer::start(author_identity_routes());
    let tmp = TempDir::new().unwrap();
    let out = tmp.path().join("g.jsonl");
    let state = tmp.path().join("state.json");
    let (jsonl, _stderr, ok) = run_import(&server.base_url, &out, &state, &[]);
    assert!(ok, "import should succeed");

    let dave_nodes = nodes_of_kind(&jsonl, "ExternalIdentity")
        .into_iter()
        .filter(|v| v["author"].as_str() == Some("dave"))
        .count();
    assert_eq!(
        dave_nodes, 1,
        "a login that is author + reviewer + requested reviewer dedups to ONE identity node"
    );

    // Exactly two distinct identities overall (ivan, dave), each minted once.
    let mut logins = identity_logins(&jsonl);
    let total = logins.len();
    logins.sort();
    logins.dedup();
    assert_eq!(
        logins,
        vec!["dave", "ivan"],
        "two distinct participant identities"
    );
    assert_eq!(
        total,
        logins.len(),
        "no duplicate identity nodes across the run"
    );
}
