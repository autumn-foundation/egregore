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
