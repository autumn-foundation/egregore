#![allow(missing_docs)]

use aletheia_egregore::{
    GraphRecord, NodeKind,
    redaction::{detect_secret_span, redact_code_graph, redact_code_text, redact_value},
};
use assert_cmd::Command as CargoCommand;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
};

#[test]
fn test_detect_secret_span_api_token() {
    let text = "pub const KEY: &str = \"sk-live-123456789012345678901234567890\";";
    let res = detect_secret_span(text);
    assert!(res.is_some());
    let (class, start, len) = res.unwrap();
    assert_eq!(class.as_str(), "api_token");
    assert_eq!(
        &text[start..start + len],
        "sk-live-123456789012345678901234567890"
    );
}

// Regression (issue #321, Codex P1 "preserve v1 redaction coverage for quoted
// env secrets"): `redact_value` already redacts a quoted `.env`-style secret, but
// the byte-span capture path relies only on `detect_secret_span`, whose EnvSecret
// span matcher treated the opening `"` as a value delimiter and returned no span.
// The two detectors must AGREE: the span matcher must return a span covering the
// secret value bytes (excluding the wrapping quotes, matching the quote-as-
// delimiter convention of the database-url/session-cookie span matchers).
#[test]
fn test_detect_secret_span_quoted_env_secret_double_quote() {
    let text = "API_KEY=\"hunterSECRETtokenValueLong\"";
    let res = detect_secret_span(text);
    assert!(
        res.is_some(),
        "quoted env secret must produce a span (parity with redact_value)"
    );
    let (class, start, len) = res.unwrap();
    assert_eq!(class.as_str(), "env_secret");
    assert_eq!(&text[start..start + len], "hunterSECRETtokenValueLong");
    // `redact_value` redacts the same input — the two detectors agree.
    assert!(redact_value(text).contains("<REDACTED:env_secret:"));
}

#[test]
fn test_detect_secret_span_quoted_env_secret_single_quote() {
    let text = "API_KEY='hunterSECRETtokenValueLong'";
    let res = detect_secret_span(text);
    assert!(
        res.is_some(),
        "single-quoted env secret must produce a span (parity with redact_value)"
    );
    let (class, start, len) = res.unwrap();
    assert_eq!(class.as_str(), "env_secret");
    assert_eq!(&text[start..start + len], "hunterSECRETtokenValueLong");
    assert!(redact_value(text).contains("<REDACTED:env_secret:"));
}

#[test]
fn test_redact_code_text_replaces_secrets() {
    let text = "const KEY: &str = \"sk-live-123456789012345678901234567890\";\nconst PAT: &str = \"ghp_1234567890123456789012345678901234567890\";";
    let (redacted, counts) = redact_code_text(text.to_owned(), "«redacted:secret»");
    assert_eq!(
        redacted,
        "const KEY: &str = \"«redacted:secret»\";\nconst PAT: &str = \"«redacted:secret»\";"
    );
    assert_eq!(
        *counts
            .get(&aletheia_egregore::redaction::SecretClass::ApiToken)
            .unwrap_or(&0),
        2
    );
}

#[test]
fn test_redact_code_graph_appends_diagnostic() {
    let mut records = vec![GraphRecord::syntax_symbol(
        "node:symbol:1".to_owned(),
        "const",
        "src/lib.rs".to_owned(),
        aletheia_egregore::ir::SourceSpan {
            start_byte: 0,
            end_byte: 10,
            start_line: 1,
            end_line: 1,
        },
        "MY_KEY".to_owned(),
        "rust",
        0,
        "const MY_KEY: &str = \"sk-live-123456789012345678901234567890\";".to_owned(),
    )];

    redact_code_graph(&mut records, false, "repo-123");

    // The symbol summary should be redacted
    let GraphRecord::Node { summary, .. } = &records[0] else {
        panic!("not a node")
    };
    assert_eq!(summary, "const MY_KEY: &str = \"«redacted:secret»\";");

    // There should be a diagnostic node appended to records
    assert_eq!(records.len(), 2);
    let GraphRecord::Node {
        kind,
        name,
        summary: diag_summary,
        ..
    } = &records[1]
    else {
        panic!("not a node")
    };
    assert_eq!(*kind, NodeKind::Diagnostic);
    assert_eq!(name.as_deref(), Some("redaction_evidence"));
    assert!(diag_summary.contains("Redacted 1 literals across 1 nodes"));
    assert!(diag_summary.contains("api_token: 1"));
}

#[test]
fn test_scan_and_scan_history_redact_secrets() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    // Initialize git repo
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // Write file with secrets (copy from corpus/fixtures/secrets_fixture)
    let fixture_src =
        fs::read_to_string("corpus/fixtures/secrets_fixture/src/lib.rs").expect("read fixture");
    write(&repo, "src/lib.rs", &fixture_src);

    // Commit secrets
    commit(&repo, "introduce secrets", "2026-06-30T12:00:00Z");

    // Let's run scan
    let scan_out = temp.path().join("scan.graph.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    let scan_jsonl = fs::read_to_string(&scan_out).expect("read scan output");
    assert!(
        !scan_jsonl.contains("sk-live-55555"),
        "must not contain raw API key"
    );
    assert!(
        !scan_jsonl.contains("ghp_1234567"),
        "must not contain raw GitHub PAT"
    );
    assert!(
        !scan_jsonl.contains("BEGIN PRIVATE KEY"),
        "must not contain raw private key"
    );
    assert!(
        scan_jsonl.contains("«redacted:secret»"),
        "must contain redacted placeholder"
    );
    assert!(
        scan_jsonl.contains(r#""name":"redaction_evidence""#),
        "must contain redaction evidence record"
    );

    // Let's run scan with escape hatch --raw-literals
    let scan_raw_out = temp.path().join("scan_raw.graph.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_raw_out)
        .arg("--raw-literals")
        .assert()
        .success();

    let scan_raw_jsonl = fs::read_to_string(&scan_raw_out).expect("read scan raw output");
    assert!(
        scan_raw_jsonl.contains("sk-live-55555"),
        "must contain raw API key when escape hatch is used"
    );
    assert!(
        scan_raw_jsonl.contains("ghp_1234567"),
        "must contain raw GitHub PAT when escape hatch is used"
    );
    assert!(
        scan_raw_jsonl.contains("BEGIN PRIVATE KEY"),
        "must contain raw private key when escape hatch is used"
    );
    assert!(
        !scan_raw_jsonl.contains("«redacted:secret»"),
        "must not contain redacted placeholder"
    );

    // Let's run scan-history
    let history_out = temp.path().join("history.graph.jsonl");
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan-history")
        .arg(&repo)
        .arg("--out")
        .arg(&history_out)
        .assert()
        .success();

    let history_jsonl = fs::read_to_string(&history_out).expect("read history output");
    assert!(
        !history_jsonl.contains("sk-live-55555"),
        "history must not contain raw API key"
    );
    assert!(
        !history_jsonl.contains("ghp_1234567"),
        "history must not contain raw GitHub PAT"
    );
    assert!(
        !history_jsonl.contains("BEGIN PRIVATE KEY"),
        "history must not contain raw private key"
    );
    assert!(
        history_jsonl.contains("«redacted:secret»"),
        "history must contain redacted placeholder"
    );
    assert!(
        history_jsonl.contains(r#""name":"redaction_evidence""#),
        "history must contain redaction evidence record"
    );
}

#[test]
fn test_api_token_scan_continues_after_short_prefix() {
    let text = "const DECOY: &str = \"sk-test\";\nconst REAL: &str = \"sk-live-123456789012345678901234567890\";";
    let (redacted, counts) = redact_code_text(text.to_owned(), "«redacted:secret»");
    assert_eq!(
        redacted,
        "const DECOY: &str = \"sk-test\";\nconst REAL: &str = \"«redacted:secret»\";"
    );
    assert_eq!(
        *counts
            .get(&aletheia_egregore::redaction::SecretClass::ApiToken)
            .unwrap_or(&0),
        1
    );
}

#[test]
fn test_detect_secret_span_private_key_missing_footer() {
    let text = "-----BEGIN RSA PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0\n";
    let (redacted, counts) = redact_code_text(text.to_owned(), "«redacted:secret»");
    assert_eq!(redacted, "«redacted:secret»");
    assert_eq!(
        *counts
            .get(&aletheia_egregore::redaction::SecretClass::SshPrivateKey)
            .unwrap_or(&0),
        1
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_refresh_redacts_secrets() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    // Initialize git repo
    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // Write initial safe file
    write(&repo, "src/lib.rs", "pub fn hello() {}");
    commit(&repo, "initial commit", "2026-06-30T12:00:00Z");

    // Initial scan and ingest
    let scan_out = temp.path().join("scan.graph.jsonl");
    let data_dir = temp.path().join(".egregore");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&scan_out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Now, write a secret into src/lib.rs
    write(
        &repo,
        "src/lib.rs",
        "pub const OPENAI_KEY: &str = \"sk-live-5555566666777778888899999000001111122222\";",
    );

    // Run refresh (by default, raw_literals is false)
    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Verify cache file has redacted content (P1)
    let cache_content =
        fs::read_to_string(data_dir.join("codegraph-cache.json")).expect("read cache");
    assert!(
        !cache_content.contains("sk-live-55555"),
        "cache must not contain raw secret"
    );
    assert!(
        cache_content.contains("«redacted:secret»"),
        "cache must contain redacted placeholder"
    );

    // Verify in database that the newly added secret is redacted
    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("open store");
    let records = reopened.read_all_records().expect("read all records");

    let mut found_symbol = false;
    let mut found_diag = false;
    for record in &records {
        if let GraphRecord::Node { name, summary, .. } = record
            && name.as_deref() == Some("OPENAI_KEY")
        {
            found_symbol = true;
            assert!(
                !summary.contains("sk-live-55555"),
                "refreshed symbol summary must be redacted"
            );
            assert!(
                summary.contains("«redacted:secret»"),
                "refreshed symbol summary must contain placeholder"
            );
        }

        if let GraphRecord::Node { name, producer, .. } = record
            && name.as_deref() == Some("redaction_evidence")
        {
            found_diag = true;
            assert!(
                producer.is_some(),
                "redaction diagnostic must carry producer info"
            );
            let prod = producer.as_ref().unwrap();
            assert_eq!(prod.producer_kind.as_str(), "incremental_cache");
        }
    }
    assert!(found_symbol, "must have found refreshed symbol node");
    assert!(
        found_diag,
        "must have emitted redaction_evidence diagnostic node"
    );
}

fn write(repo: &Path, relative: &str, contents: &str) {
    let path = repo.join(relative);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dir");
    fs::write(path, contents).expect("write file");
}

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git should execute");
    assert!(
        output.status.success(),
        "git command failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn commit(repo: &Path, message: &str, date: &str) {
    git(repo, ["add", "."]);
    let status = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["commit", "-m", message])
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .stdin(Stdio::null())
        .output()
        .expect("git commit should execute");
    assert!(status.status.success());
}

#[test]
fn test_detect_secret_span_ignores_already_redacted_env_var() {
    let text = "const PASSWORD: &str = \"«redacted:secret»\";";
    let (redacted, counts) = redact_code_text(text.to_owned(), "«redacted:secret»");
    assert_eq!(redacted, text);
    assert!(
        counts.is_empty(),
        "should not redact already redacted env var"
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_refresh_redacts_reused_raw_cache_records() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;

    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // Write initial safe file
    write(&repo, "src/lib.rs", "pub fn hello() {}");
    commit(&repo, "initial commit", "2026-06-30T12:00:00Z");

    let scan_out = temp.path().join("scan.graph.jsonl");
    let data_dir = temp.path().join(".egregore");

    // Perform initial scan and ingest
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("ingest")
        .arg(&scan_out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Now write a secret
    write(
        &repo,
        "src/lib.rs",
        "pub const KEY: &str = \"sk-live-5555566666777778888899999000001111122222\";",
    );

    // Run refresh with --raw-literals to write raw secrets to the incremental cache file
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--raw-literals")
        .assert()
        .success();

    // The cache file under .egregore contains raw secrets right now.
    let cache_path = data_dir.join("codegraph-cache.json");
    let raw_cache = fs::read_to_string(&cache_path).expect("read raw cache");
    assert!(
        raw_cache.contains("sk-live-55555"),
        "raw cache must contain raw secret"
    );

    // Now run eg refresh WITHOUT raw_literals (default). The file is UNCHANGED,
    // so it will hit the cache. We want refresh to redact it anyway!
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Verify cache file has now been redacted and saved (self-healing)
    let redacted_cache = fs::read_to_string(&cache_path).expect("read redacted cache");
    assert!(
        !redacted_cache.contains("sk-live-55555"),
        "cache must now be redacted"
    );

    // Verify in database that it is redacted
    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("open store");
    let records = reopened.read_all_records().expect("read all records");

    let mut found_symbol = false;
    for record in &records {
        if let GraphRecord::Node { name, summary, .. } = record
            && name.as_deref() == Some("KEY")
        {
            found_symbol = true;
            assert!(
                !summary.contains("sk-live-55555"),
                "summary must be redacted"
            );
            assert!(
                summary.contains("«redacted:secret»"),
                "summary must contain placeholder"
            );
        }
    }
    assert!(found_symbol, "must have found refreshed symbol node");
}

#[test]
fn test_detect_secret_span_cookie_with_multibyte_chars() {
    let text = "const COOKIE: &str = \"session=ééééééééa\";";
    let (redacted, counts) = redact_code_text(text.to_owned(), "«redacted:secret»");
    assert_eq!(redacted, "const COOKIE: &str = \"«redacted:secret»\";");
    assert_eq!(
        *counts
            .get(&aletheia_egregore::redaction::SecretClass::SessionCookie)
            .unwrap_or(&0),
        1
    );
}

#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_refresh_rebuilds_when_raw_literals_mode_toggled() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;

    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    // Write a file with a secret
    write(
        &repo,
        "src/lib.rs",
        "pub const KEY: &str = \"sk-live-5555566666777778888899999000001111122222\";",
    );
    commit(&repo, "initial commit", "2026-06-30T12:00:00Z");

    let scan_out = temp.path().join("scan.graph.jsonl");
    let data_dir = temp.path().join(".egregore");

    // Perform initial scan & ingest with default redacted mode
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("ingest")
        .arg(&scan_out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Run refresh with default redacted mode (creates redacted cache)
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Verify cache file has redacted content (P1)
    let cache_path = data_dir.join("codegraph-cache.json");
    let cache_content = fs::read_to_string(&cache_path).expect("read cache");
    assert!(
        !cache_content.contains("sk-live-55555"),
        "cache must be redacted"
    );

    // Now run eg refresh WITH raw_literals. The file is UNCHANGED,
    // but because raw_literals mode is toggled, it must invalidate/rebuild the cached files
    // and ingest raw literals!
    CargoCommand::cargo_bin("egregore")
        .expect("binary")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--raw-literals")
        .assert()
        .success();

    // Verify cache file has now been saved with raw literals (P2)
    let raw_cache = fs::read_to_string(&cache_path).expect("read cache");
    assert!(
        raw_cache.contains("sk-live-55555"),
        "cache must now contain raw secrets"
    );

    // Verify in database that it is raw
    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("open store");
    let records = reopened.read_all_records().expect("read all records");

    let mut found_symbol = false;
    for record in &records {
        if let GraphRecord::Node { name, summary, .. } = record
            && name.as_deref() == Some("KEY")
        {
            found_symbol = true;
            assert!(
                summary.contains("sk-live-55555"),
                "summary must contain raw secret"
            );
            assert!(
                !summary.contains("«redacted:secret»"),
                "summary must not be redacted"
            );
        }
    }
    assert!(found_symbol, "must have found refreshed symbol node");
}

#[test]
fn test_detect_secret_span_bearer_with_multibyte_chars() {
    let text = "const BEARER: &str = \"Bearer éééééééééééééééééééé\";";
    let (redacted, counts) = redact_code_text(text.to_owned(), "«redacted:secret»");
    assert_eq!(redacted, "const BEARER: &str = \"«redacted:secret»\";");
    assert_eq!(
        *counts
            .get(&aletheia_egregore::redaction::SecretClass::ApiToken)
            .unwrap_or(&0),
        1
    );
}

#[test]
fn test_redact_code_graph_appends_diagnostic_with_valid_time() {
    use aletheia_egregore::ir::NodeKind;

    let mut records = vec![
        GraphRecord::node(
            "node:1".to_owned(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            None,
            "openai API key: sk-live-5555566666777778888899999000001111122222".to_owned(),
        )
        .with_valid_time_inferred("2026-01-01T00:00:00Z"),
    ];

    redact_code_graph(&mut records, false, "repo-123");

    let mut found_diag = false;
    for record in &records {
        if let GraphRecord::Node {
            name,
            valid_time,
            valid_time_source,
            ..
        } = record
            && name.as_deref() == Some("redaction_evidence")
        {
            found_diag = true;
            assert_eq!(valid_time.as_deref(), Some("2026-01-01T00:00:00Z"));
            assert_eq!(
                valid_time_source.as_deref(),
                Some("inferred_from_transaction_time")
            );
        }
    }
    assert!(
        found_diag,
        "redaction_evidence diagnostic node must be appended"
    );
}

/// Regression for Codex C9: a refresh-emitted `redaction_evidence` diagnostic
/// must carry `valid_time` (inferred from this refresh's transaction time) just
/// like the sibling Repository/File/Symbol nodes in the same batch, so temporal
/// selectors and audits can place it consistently.
#[cfg(feature = "embedded-aletheiadb")]
#[test]
fn test_refresh_redaction_diagnostic_carries_valid_time() {
    use aletheia_egregore::adapters::EmbeddedAletheiaSink;

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).expect("repo dir");

    git(&repo, ["init"]);
    git(&repo, ["config", "user.email", "test@example.invalid"]);
    git(&repo, ["config", "user.name", "Test User"]);
    git(&repo, ["config", "core.autocrlf", "false"]);
    git(&repo, ["config", "commit.gpgsign", "false"]);

    write(&repo, "src/lib.rs", "pub fn hello() {}");
    commit(&repo, "initial commit", "2026-06-30T12:00:00Z");

    let scan_out = temp.path().join("scan.graph.jsonl");
    let data_dir = temp.path().join(".egregore");

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&scan_out)
        .assert()
        .success();

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&scan_out)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    // Introduce a secret and refresh (default redacting mode).
    write(
        &repo,
        "src/lib.rs",
        "pub const OPENAI_KEY: &str = \"sk-live-5555566666777778888899999000001111122222\";",
    );

    CargoCommand::cargo_bin("egregore")
        .expect("binary should run")
        .arg("refresh")
        .arg(&repo)
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .success();

    let reopened = EmbeddedAletheiaSink::open(&data_dir).expect("open store");
    let records = reopened.read_all_records().expect("read all records");

    let mut found_diag = false;
    for record in &records {
        if let GraphRecord::Node {
            name,
            valid_time,
            valid_time_source,
            ..
        } = record
            && name.as_deref() == Some("redaction_evidence")
        {
            found_diag = true;
            assert!(
                valid_time.is_some(),
                "refresh redaction_evidence diagnostic must carry valid_time"
            );
            assert_eq!(
                valid_time_source.as_deref(),
                Some("inferred_from_transaction_time"),
                "valid_time_source must mark it as inferred from transaction time"
            );
        }
    }
    assert!(
        found_diag,
        "must have emitted redaction_evidence diagnostic node"
    );
}
