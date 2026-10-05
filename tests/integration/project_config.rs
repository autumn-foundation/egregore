//! Acceptance tests for issue #261: checked-in `egregore.toml` project config.
//!
//! Covers: auto-discovery by walking up from the working directory, the pinned
//! fields (data-dir, repo identity override, embedding model, scan scope,
//! redaction policy), CLI > config > default precedence, byte-identical
//! default behavior with no config file, fail-fast machine-readable
//! diagnostics for malformed files / unknown keys, `eg config show`, and the
//! two-environments-one-config byte-identical fixture test.

#![allow(missing_docs)]

use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;
use predicates::prelude::*;

use aletheia_egregore::project_config::{
    CONFIG_FILE_NAME, LoadedConfig, ValueSource, default_data_dir, default_embedding_model,
    default_redaction_policy, discover_and_load, discover_config_file, load_config_file,
    resolve_flag, resolve_opt, resolve_value,
};

fn fixture_repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rust_basic")
}

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join(CONFIG_FILE_NAME);
    fs::write(&path, body).expect("config file should be writable");
    path
}

/// Run the built binary with `cwd` as its working directory.
fn eg_in(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("egregore").expect("binary should run");
    cmd.current_dir(cwd);
    cmd
}

fn config_show_json(cwd: &Path) -> serde_json::Value {
    let assert = eg_in(cwd).arg("config").arg("show").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).expect("utf8 stdout");
    serde_json::from_str(&stdout).expect("config show should print JSON")
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[test]
fn config_file_absent_is_not_an_error() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    assert_eq!(discover_config_file(temp.path()), None);
    assert!(discover_and_load(temp.path()).expect("ok").is_none());
}

#[test]
fn discovery_walks_up_from_working_directory() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let config_path = write_config(temp.path(), "repo_id_override = \"up-walk\"\n");
    let deep = temp.path().join("a").join("b").join("c");
    fs::create_dir_all(&deep).expect("nested dirs should be created");

    assert_eq!(discover_config_file(&deep), Some(config_path.clone()));

    let loaded: LoadedConfig = discover_and_load(&deep)
        .expect("load should succeed")
        .expect("config should be found");
    assert_eq!(loaded.path, config_path);
    assert_eq!(loaded.config.repo_id_override.as_deref(), Some("up-walk"));
}

#[test]
fn empty_config_file_parses_to_all_defaults() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "# nothing pinned yet\n");
    let config = load_config_file(&path).expect("empty config should parse");
    assert_eq!(config.data_dir, None);
    assert_eq!(config.repo_id_override, None);
    assert_eq!(config.embeddings.model, None);
    assert!(config.scan.include.is_none());
    assert!(config.scan.exclude.is_none());
    assert_eq!(config.scan.transaction_time, None);
    assert_eq!(config.redaction.policy, None);
    assert_eq!(config.redaction.raw_literals, None);
}

// ---------------------------------------------------------------------------
// Parsing: all pinnable fields
// ---------------------------------------------------------------------------

#[test]
fn config_parses_every_pinnable_field() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(
        temp.path(),
        r#"
data_dir = "custom-store"
repo_id_override = "pinned-repo"

[embeddings]
model = "sentence-transformers/all-MiniLM-L6-v2"

[scan]
include = ["src/**", "crates/**"]
exclude = ["vendor/**"]
transaction_time = "2026-05-19T00:00:00Z"

[redaction]
policy = "v1"
raw_literals = true
"#,
    );
    let config = load_config_file(&path).expect("full config should parse");
    assert_eq!(
        config.data_dir,
        Some(temp.path().join("custom-store")),
        "relative data_dir resolves against the config file directory"
    );
    assert_eq!(config.repo_id_override.as_deref(), Some("pinned-repo"));
    assert_eq!(
        config.embeddings.model.as_deref(),
        Some("sentence-transformers/all-MiniLM-L6-v2")
    );
    assert_eq!(
        config.scan.include,
        Some(vec!["src/**".to_owned(), "crates/**".to_owned()])
    );
    assert_eq!(config.scan.exclude, Some(vec!["vendor/**".to_owned()]));
    assert_eq!(
        config.scan.transaction_time.as_deref(),
        Some("2026-05-19T00:00:00Z")
    );
    assert_eq!(config.redaction.policy.as_deref(), Some("v1"));
    assert_eq!(config.redaction.raw_literals, Some(true));
}

#[test]
fn absolute_data_dir_is_kept_verbatim() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "data_dir = \"/var/lib/egregore\"\n");
    let config = load_config_file(&path).expect("config should parse");
    assert_eq!(config.data_dir, Some(PathBuf::from("/var/lib/egregore")));
}

// ---------------------------------------------------------------------------
// Fail-fast validation
// ---------------------------------------------------------------------------

#[test]
fn malformed_toml_fails_with_machine_readable_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "data_dir = [unclosed\n");
    let err = load_config_file(&path).expect_err("malformed TOML must fail");
    assert_eq!(err.code(), "config_invalid");
    let diag = err.diagnostic();
    assert_eq!(diag["code"], "config_invalid");
    assert!(diag["message"].is_string(), "diagnostic: {diag}");
}

#[test]
fn unknown_top_level_key_fails_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "bogus_key = 1\n");
    let err = load_config_file(&path).expect_err("unknown key must fail");
    let diag = err.diagnostic();
    assert_eq!(diag["code"], "config_invalid");
    let message = diag["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("bogus_key"),
        "diagnostic names the key: {message}"
    );
}

#[test]
fn unknown_nested_key_fails_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "[scan]\nbogus = [\"x\"]\n");
    let err = load_config_file(&path).expect_err("unknown nested key must fail");
    assert_eq!(err.diagnostic()["code"], "config_invalid");
}

#[test]
fn unknown_table_fails_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "[daemon]\nport = 8080\n");
    let err = load_config_file(&path).expect_err("unknown table must fail");
    assert_eq!(err.diagnostic()["code"], "config_invalid");
}

#[test]
fn wrong_type_fails_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "data_dir = 42\n");
    let err = load_config_file(&path).expect_err("wrong type must fail");
    assert_eq!(err.diagnostic()["code"], "config_invalid");
}

#[test]
fn unknown_redaction_policy_fails_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "[redaction]\npolicy = \"v99\"\n");
    let err = load_config_file(&path).expect_err("unknown policy must fail");
    assert_eq!(err.diagnostic()["code"], "config_invalid");
}

#[test]
fn invalid_transaction_time_fails_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = write_config(temp.path(), "[scan]\ntransaction_time = \"not-a-time\"\n");
    let err = load_config_file(&path).expect_err("bad timestamp must fail");
    assert_eq!(err.diagnostic()["code"], "config_invalid");
}

#[test]
fn missing_config_file_is_an_io_diagnostic_not_a_panic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let err = load_config_file(&temp.path().join("nope.toml")).expect_err("missing file must fail");
    assert_eq!(err.diagnostic()["code"], "config_invalid");
}

// ---------------------------------------------------------------------------
// Resolution / precedence helpers
// ---------------------------------------------------------------------------

#[test]
fn precedence_is_cli_over_config_over_default() {
    // CLI wins over config.
    assert_eq!(
        resolve_opt(Some("cli"), Some("config")),
        (Some("cli"), ValueSource::Cli)
    );
    // Config wins over default.
    assert_eq!(
        resolve_opt(None, Some("config")),
        (Some("config"), ValueSource::Config)
    );
    // Default when neither is set.
    assert_eq!(
        resolve_opt::<String>(None, None),
        (None, ValueSource::Default)
    );
    // Value resolution with a built-in default.
    assert_eq!(
        resolve_value(None, Some("config"), "default"),
        ("config", ValueSource::Config)
    );
    assert_eq!(
        resolve_value(None, None, "default"),
        ("default", ValueSource::Default)
    );
    assert_eq!(
        resolve_value(Some("cli"), Some("config"), "default"),
        ("cli", ValueSource::Cli)
    );
    // Bool flags: an explicit flag always wins; otherwise config, else default.
    assert_eq!(resolve_flag(true, Some(true)), (true, ValueSource::Cli));
    assert_eq!(resolve_flag(true, None), (true, ValueSource::Cli));
    assert_eq!(resolve_flag(false, Some(true)), (true, ValueSource::Config));
    assert_eq!(resolve_flag(false, None), (false, ValueSource::Default));
}

#[test]
fn builtin_defaults_match_documented_values() {
    assert_eq!(default_data_dir(), PathBuf::from(".egregore"));
    assert_eq!(
        default_embedding_model(),
        "sentence-transformers/all-MiniLM-L6-v2"
    );
    assert_eq!(default_redaction_policy(), "v1");
}

// ---------------------------------------------------------------------------
// `eg config show`
// ---------------------------------------------------------------------------

#[test]
fn config_show_reports_defaults_when_no_file() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let value = config_show_json(temp.path());
    assert_eq!(value["config_file"], serde_json::Value::Null);
    assert_eq!(value["data_dir"]["value"], ".egregore");
    assert_eq!(value["data_dir"]["source"], "default");
    assert_eq!(value["repo_id_override"]["value"], serde_json::Value::Null);
    assert_eq!(value["repo_id_override"]["source"], "default");
    assert_eq!(
        value["embeddings"]["model"]["value"],
        "sentence-transformers/all-MiniLM-L6-v2"
    );
    assert_eq!(value["embeddings"]["model"]["source"], "default");
    assert_eq!(value["scan"]["include"]["value"], serde_json::json!([]));
    assert_eq!(
        value["scan"]["transaction_time"]["value"],
        serde_json::Value::Null
    );
    assert_eq!(value["redaction"]["policy"]["value"], "v1");
    assert_eq!(value["redaction"]["policy"]["source"], "default");
    assert_eq!(value["redaction"]["raw_literals"]["value"], false);
}

#[test]
fn config_show_reports_pinned_values_with_config_source() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let config_path = write_config(
        temp.path(),
        r#"
data_dir = "pinned-store"
repo_id_override = "pinned-repo"

[scan]
include = ["src/**"]
exclude = ["vendor/**"]

[redaction]
raw_literals = true
"#,
    );
    let value = config_show_json(temp.path());
    assert_eq!(
        value["config_file"],
        serde_json::json!(config_path.to_string_lossy())
    );
    assert_eq!(
        value["data_dir"]["value"],
        serde_json::json!(temp.path().join("pinned-store").to_string_lossy())
    );
    assert_eq!(value["data_dir"]["source"], "config");
    assert_eq!(value["repo_id_override"]["value"], "pinned-repo");
    assert_eq!(value["repo_id_override"]["source"], "config");
    assert_eq!(
        value["scan"]["include"]["value"],
        serde_json::json!(["src/**"])
    );
    assert_eq!(value["scan"]["include"]["source"], "config");
    assert_eq!(
        value["scan"]["exclude"]["value"],
        serde_json::json!(["vendor/**"])
    );
    // Unset fields still report built-in defaults.
    assert_eq!(
        value["embeddings"]["model"]["value"],
        "sentence-transformers/all-MiniLM-L6-v2"
    );
    assert_eq!(value["embeddings"]["model"]["source"], "default");
    assert_eq!(value["redaction"]["raw_literals"]["value"], true);
    assert_eq!(value["redaction"]["raw_literals"]["source"], "config");
}

/// An explicitly set-but-empty `include = []` is a pin, not an absence:
/// `eg config show` reports its source as `"config"` (presence, not
/// emptiness, determines the source).
#[test]
fn explicitly_empty_scope_arrays_report_config_source() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "[scan]\ninclude = []\n");
    let value = config_show_json(temp.path());
    assert_eq!(value["scan"]["include"]["value"], serde_json::json!([]));
    assert_eq!(value["scan"]["include"]["source"], "config");
    assert_eq!(value["scan"]["exclude"]["value"], serde_json::json!([]));
    assert_eq!(value["scan"]["exclude"]["source"], "default");
}

#[test]
fn config_show_finds_config_by_walking_up() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "repo_id_override = \"up-walk\"\n");
    let deep = temp.path().join("sub").join("dir");
    fs::create_dir_all(&deep).expect("nested dirs should be created");
    let value = config_show_json(&deep);
    assert_eq!(value["repo_id_override"]["value"], "up-walk");
    assert_eq!(value["repo_id_override"]["source"], "config");
}

// ---------------------------------------------------------------------------
// CLI fail-fast behavior
// ---------------------------------------------------------------------------

#[test]
fn malformed_config_makes_scan_fail_fast_with_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "data_dir = [unclosed\n");
    let graph_path = temp.path().join("graph.jsonl");

    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains(r#""code":"config_invalid""#));
    assert!(
        !graph_path.exists(),
        "failed scan must not leave a partial graph"
    );
}

#[test]
fn unknown_key_makes_scan_fail_fast_with_diagnostic() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "mystery_option = true\n");
    let graph_path = temp.path().join("graph.jsonl");

    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .failure()
        .stderr(predicate::str::contains(r#""code":"config_invalid""#))
        .stderr(predicate::str::contains("mystery_option"));
}

#[test]
fn malformed_config_makes_query_fail_fast() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "[scan]\ninclude = \"not-an-array\"\n");

    eg_in(temp.path())
        .args(["query", "symbol", "whatever", "--graph", "graph.jsonl"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(r#""code":"config_invalid""#));
}

// ---------------------------------------------------------------------------
// Config values take effect; CLI flags override them
// ---------------------------------------------------------------------------

#[test]
fn config_repo_id_override_applies_to_scan() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "repo_id_override = \"config-pinned-repo\"\n");
    let graph_path = temp.path().join("graph.jsonl");

    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");
    assert!(
        jsonl.contains("config-pinned-repo"),
        "Repository node should carry the config-pinned identity"
    );
}

#[test]
fn cli_flag_overrides_config_value_for_scan() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "repo_id_override = \"config-pinned-repo\"\n");
    let graph_path = temp.path().join("graph.jsonl");

    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .arg("--repo-id-override")
        .arg("cli-wins-repo")
        .assert()
        .success();

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");
    assert!(
        jsonl.contains("cli-wins-repo"),
        "explicit --repo-id-override must win over the config value"
    );
    assert!(
        !jsonl.contains("config-pinned-repo"),
        "config value must not leak through when the flag is given"
    );
}

#[test]
fn config_transaction_time_pins_scan_output() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(
        temp.path(),
        "[scan]\ntransaction_time = \"2026-05-19T00:00:00Z\"\n",
    );
    let graph_path = temp.path().join("graph.jsonl");

    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    let jsonl = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");
    assert!(
        jsonl.contains("2026-05-19T00:00:00Z"),
        "pinned transaction time should stamp the scan output"
    );
    assert!(
        !jsonl.contains("2026-09-15"),
        "wall-clock time must not leak into a time-pinned scan"
    );
}

// ---------------------------------------------------------------------------
// Purely additive: no config file -> behavior unchanged
// ---------------------------------------------------------------------------

/// The new explicit-time scan entry point must produce byte-for-byte the same
/// JSONL as the pre-existing entry point for identical inputs (issue #261:
/// the config layer is purely additive).
#[test]
fn explicit_time_scan_entry_point_matches_legacy_output_byte_for_byte() {
    use aletheia_egregore::{scan_repository_at_with_exclusions, scan_repository_at_with_override};
    let repo = fixture_repo();
    let fixed_time = "2026-05-19T00:00:00Z";
    let legacy = scan_repository_at_with_override(&repo, fixed_time, None)
        .expect("legacy scan should succeed")
        .to_jsonl()
        .expect("legacy graph should serialize");
    let via_new = scan_repository_at_with_exclusions(&repo, fixed_time, None, &[])
        .expect("new scan entry point should succeed")
        .to_jsonl()
        .expect("new graph should serialize");
    assert_eq!(
        via_new, legacy,
        "the config-threaded scan path must not alter output when no config values are set"
    );
}

/// Matches `YYYY-MM-DDTHH:MM:SS` with an optional fractional part and `Z`.
/// Timestamp shapes come from `chrono::SecondsFormat::{Secs, Nanos}`.
fn is_timestamp_at(bytes: &[u8], i: usize) -> bool {
    let digit = |j: usize| bytes.get(j).is_some_and(u8::is_ascii_digit);
    // Need at least `YYYY-MM-DDTHH:MM:SSZ` (20 bytes).
    if bytes.len() < i + 20 {
        return false;
    }
    digit(i)
        && digit(i + 1)
        && digit(i + 2)
        && digit(i + 3)
        && bytes[i + 4] == b'-'
        && digit(i + 5)
        && digit(i + 6)
        && bytes[i + 7] == b'-'
        && digit(i + 8)
        && digit(i + 9)
        && bytes[i + 10] == b'T'
        && digit(i + 11)
        && digit(i + 12)
        && bytes[i + 13] == b':'
        && digit(i + 14)
        && digit(i + 15)
        && bytes[i + 16] == b':'
        && digit(i + 17)
        && digit(i + 18)
}

/// Length of the timestamp at `i`, including an optional `.fraction` and the
/// trailing `Z`. Callers must check [`is_timestamp_at`] first.
fn timestamp_len(bytes: &[u8], i: usize) -> usize {
    let mut len = 19; // `YYYY-MM-DDTHH:MM:SS`
    if bytes.get(i + 19) == Some(&b'.') {
        len += 1;
        while bytes.get(i + len).is_some_and(u8::is_ascii_digit) {
            len += 1;
        }
    }
    if bytes.get(i + len) == Some(&b'Z') {
        len += 1;
    }
    len
}

/// A config file that pins nothing (all defaults) must not change scan output:
/// timestamps normalized, the record stream is identical to a no-config run.
#[test]
fn all_defaults_config_leaves_scan_output_unchanged() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let no_config_dir = temp.path().join("no-config");
    let with_config_dir = temp.path().join("with-config");
    fs::create_dir_all(&no_config_dir).expect("dir should be created");
    fs::create_dir_all(&with_config_dir).expect("dir should be created");
    // An empty config file: every field falls back to its built-in default.
    write_config(&with_config_dir, "# all defaults\n");

    let normalize = |jsonl: &str| {
        // Collapse every RFC 3339 timestamp (seconds or fractional-seconds
        // precision) so two runs at different wall-clock instants compare on
        // record content. No regex dependency: scan for the fixed shape.
        let bytes = jsonl.as_bytes();
        let mut out = String::with_capacity(jsonl.len());
        let mut i = 0;
        while i < bytes.len() {
            if is_timestamp_at(bytes, i) {
                out.push_str("NORMALIZED-TIME");
                i += timestamp_len(bytes, i);
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        out
    };

    let mut outputs = Vec::new();
    for dir in [&no_config_dir, &with_config_dir] {
        let graph_path = dir.join("graph.jsonl");
        eg_in(dir)
            .arg("scan")
            .arg(fixture_repo())
            .arg("--out")
            .arg(&graph_path)
            .assert()
            .success();
        outputs.push(normalize(
            &fs::read_to_string(&graph_path).expect("scan should write graph JSONL"),
        ));
    }
    assert_eq!(
        outputs[0], outputs[1],
        "an all-defaults config file must not change scan output"
    );
}

// ---------------------------------------------------------------------------
// Two environments, one checked-in config -> byte-identical graph JSONL
// ---------------------------------------------------------------------------

#[test]
fn two_environments_sharing_config_produce_byte_identical_jsonl() {
    let pinned_config = r#"
repo_id_override = "fixture-stable-repo"

[scan]
transaction_time = "2026-05-19T00:00:00Z"
"#;
    let mut outputs = Vec::new();
    // Two "environments": distinct working directories (different absolute
    // paths, as on a developer machine vs CI), sharing the checked-in config.
    // Tempdirs are kept alive in `envs` until after both runs are read.
    let mut envs = Vec::new();
    for env_name in ["dev-machine", "ci-runner"] {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let env_dir = temp.path().join(env_name);
        fs::create_dir_all(&env_dir).expect("env dir should be created");
        write_config(&env_dir, pinned_config);
        let graph_path = env_dir.join("graph.jsonl");

        eg_in(&env_dir)
            .arg("scan")
            .arg(fixture_repo())
            .arg("--out")
            .arg(&graph_path)
            // A stray env var must not perturb the scan either.
            .env("EGREGORE_UNRELATED_ENV", "noise")
            .assert()
            .success()
            .stderr(predicate::str::is_empty());

        let jsonl = fs::read_to_string(&graph_path).expect("scan should write graph JSONL");
        outputs.push(jsonl);
        envs.push(temp);
    }
    assert_eq!(
        outputs[0], outputs[1],
        "two environments sharing one checked-in config must produce byte-for-byte identical graph JSONL"
    );
    drop(envs);
}

// ---------------------------------------------------------------------------
// Ingest honors the pinned data-dir; CLI flag overrides it
// ---------------------------------------------------------------------------

/// `eg ingest` without `--data-dir` uses the config-pinned store directory;
/// an explicit `--data-dir` flag wins.
#[test]
#[cfg(feature = "embedded-aletheiadb")]
fn ingest_uses_config_data_dir_unless_flag_overrides() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "data_dir = \"pinned-store\"\n");

    // First scan a graph to ingest (no config interference expected).
    let graph_path = temp.path().join("graph.jsonl");
    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    // Ingest with no --data-dir flag: the config-pinned directory is used.
    eg_in(temp.path())
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .assert()
        .success();
    assert!(
        temp.path().join("pinned-store").exists(),
        "ingest should use the config-pinned data-dir"
    );
    assert!(
        !temp.path().join(".egregore").exists(),
        "the built-in default must not be used when the config pins data-dir"
    );

    // Ingest with an explicit flag: the flag wins over the config.
    eg_in(temp.path())
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg("flag-store")
        .assert()
        .success();
    assert!(
        temp.path().join("flag-store").exists(),
        "explicit --data-dir must win over the config value"
    );
}

/// `eg query` without `--data-dir` reads the config-pinned store.
#[test]
#[cfg(feature = "embedded-aletheiadb")]
fn query_reads_config_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "data_dir = \"pinned-store\"\n");

    let graph_path = temp.path().join("graph.jsonl");
    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();
    eg_in(temp.path())
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .assert()
        .success();

    // No --data-dir and no --graph: the config-pinned store answers.
    eg_in(temp.path())
        .args(["query", "symbol", "answer", "--format", "text"])
        .assert()
        .success()
        .stdout(predicate::str::contains("answer"));
}

/// A `--graph` file read plus a config-pinned `data_dir` is not "both
/// provided": the explicit graph wins and the query succeeds instead of
/// failing with the both-inputs error.
#[test]
#[cfg(feature = "embedded-aletheiadb")]
fn query_graph_flag_wins_over_config_data_dir() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "data_dir = \"pinned-store\"\n");

    let graph_path = temp.path().join("graph.jsonl");
    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success();

    eg_in(temp.path())
        .args([
            "query",
            "symbol",
            "answer",
            "--graph",
            graph_path.to_str().expect("graph path should be UTF-8"),
            "--format",
            "text",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("answer"));
}

// ---------------------------------------------------------------------------
// stderr contract: scan stays quiet unless the config says otherwise
// ---------------------------------------------------------------------------

#[test]
fn scan_with_unconsumed_scope_keys_warns_once_on_stderr() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(temp.path(), "[scan]\ninclude = [\"src/**\"]\n");
    let graph_path = temp.path().join("graph.jsonl");

    // include/exclude are reserved for the issue #99 scoping engine; until it
    // lands, scan must not silently ignore them.
    eg_in(temp.path())
        .arg("scan")
        .arg(fixture_repo())
        .arg("--out")
        .arg(&graph_path)
        .assert()
        .success()
        .stderr(predicate::str::contains("not yet consumed"));
}

// ---------------------------------------------------------------------------
// Embedding model pin: config value is honored without a network download
// ---------------------------------------------------------------------------

#[cfg(feature = "embeddings")]
use aletheia_egregore::{Graph, GraphRecord, NodeKind, stable_id};

/// Write a graph with zero embedding candidates: `ingest --embed` runs
/// against it never load the model, so config-model tests need no network.
#[cfg(feature = "embeddings")]
fn write_repo_only_graph(path: &Path) {
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "repo-only"]),
        NodeKind::Repository,
        None,
        None,
        Some("repo-only".to_owned()),
        "Repository repo-only".to_owned(),
    ));
    fs::write(
        path,
        graph.to_jsonl().expect("repo-only graph should serialize"),
    )
    .expect("repo-only graph should be written");
}

/// A config-pinned embedding model is HONORED, not refused (issue #261): the
/// `--embed` run succeeds and the store's vector-index identity records the
/// pinned model name.
///
/// Uses a repository-only graph so `generate_embeddings` short-circuits with
/// zero candidates and never downloads a model — the identity must still name
/// the resolved model, because the index itself is still created.
#[test]
#[cfg(all(feature = "embeddings", feature = "embedded-aletheiadb"))]
fn embed_honors_config_pinned_model() {
    use aletheia_egregore::{
        adapters::EmbeddedAletheiaSink,
        embeddings::{
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS, embedding_model_identity, indexed_identities,
        },
    };

    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(
        temp.path(),
        "[embeddings]\nmodel = \"other-org/other-model\"\n",
    );
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    write_repo_only_graph(&graph_path);
    let data_dir = temp.path().join("pinned-model-store");

    eg_in(temp.path())
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .assert()
        .success();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    let identities = indexed_identities(&records);
    assert_eq!(
        identities,
        vec![embedding_model_identity(
            "other-org/other-model",
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS
        )],
        "the vector-index identity must name the config-pinned model"
    );
}

/// `--embed-model` overrides a config-pinned model (issue #261): CLI >
/// config > default. The store's identity records the CLI model, not the pin.
#[test]
#[cfg(all(feature = "embeddings", feature = "embedded-aletheiadb"))]
fn embed_model_flag_overrides_config_pin() {
    use aletheia_egregore::{
        adapters::EmbeddedAletheiaSink,
        embeddings::{
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS, embedding_model_identity, indexed_identities,
        },
    };

    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_config(
        temp.path(),
        "[embeddings]\nmodel = \"config-org/config-model\"\n",
    );
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    write_repo_only_graph(&graph_path);
    let data_dir = temp.path().join("flag-overrides-store");

    eg_in(temp.path())
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .arg("--embed-model")
        .arg("cli-org/cli-model")
        .assert()
        .success();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    let identities = indexed_identities(&records);
    assert_eq!(
        identities,
        vec![embedding_model_identity(
            "cli-org/cli-model",
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS
        )],
        "the vector-index identity must name the --embed-model flag value"
    );
}

/// With no config pin and no flag, `--embed` records the built-in default
/// model (issue #261): the default leg of the precedence chain.
#[test]
#[cfg(all(feature = "embeddings", feature = "embedded-aletheiadb"))]
fn embed_uses_default_model_when_nothing_pinned() {
    use aletheia_egregore::{
        adapters::EmbeddedAletheiaSink,
        embeddings::{
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS, default_embedding_model_identity,
            indexed_identities,
        },
    };

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let graph_path = temp.path().join("repo-only.graph.jsonl");
    write_repo_only_graph(&graph_path);
    let data_dir = temp.path().join("default-model-store");

    eg_in(temp.path())
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .assert()
        .success();

    let sink = EmbeddedAletheiaSink::open(&data_dir).expect("store reopens");
    let records = sink.read_all_records().expect("store reads back");
    let identities = indexed_identities(&records);
    assert_eq!(
        identities,
        vec![default_embedding_model_identity(
            DEFAULT_EMBEDDING_MODEL_DIMENSIONS
        )],
        "the vector-index identity must name the built-in default model"
    );
}
