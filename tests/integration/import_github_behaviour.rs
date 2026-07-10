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
