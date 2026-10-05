//! Integration tests for `eg schema export` / `eg schema list` (issue #226).
//!
//! SPEC: a documented command emits JSON Schema draft 2020-12 documents for
//! every persisted `(domain, kind, schema_version)` record contract under
//! `docs/schema/`, derived from the Rust types in `src/ir.rs` (not a
//! disconnected static schema). Every document declares required record,
//! provenance, and producer-envelope fields; agent-authored kinds require
//! provenance; deterministic code-graph kinds forbid subjective/agent-authored
//! fields.
//!
//! These tests are black-box over the CLI: they parse the emitted schemas and
//! validate real records (existing fixtures plus freshly produced records)
//! against them with a minimal draft-2020-12-subset validator implemented
//! below, plus negative tests proving invalid records fail.

#![allow(missing_docs)]

use assert_cmd::Command;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// The draft all emitted schemas must declare.
const DRAFT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";

/// Current codegraph schema version. Must track `SCHEMA_VERSION` in
/// `src/ir.rs` (bumped 10 -> 11 by issue #224, which added the
/// `HistoryReplayTip` node kind). Tests that assert current-version behavior
/// (e.g. the producer-envelope requirement) address this version; v10 is now
/// legacy and stays legacy per `docs/schema/producer-version.md` §6.
const CODEGRAPH_CURRENT_VERSION: u32 = 11;

fn eg() -> Command {
    Command::cargo_bin("egregore").expect("binary should be built")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

// ── CLI helpers ─────────────────────────────────────────────────────────────

/// Run `eg schema export` with the given extra args; assert success and parse
/// stdout as JSON.
fn export_json(args: &[&str]) -> Value {
    let mut cmd = eg();
    cmd.arg("schema").arg("export");
    for a in args {
        cmd.arg(a);
    }
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).expect("schema export stdout should be JSON")
}

/// Export a single document.
fn export_single(domain: &str, kind: &str, version: u32) -> Value {
    export_json(&[
        "--domain",
        domain,
        "--kind",
        kind,
        "--schema-version",
        &version.to_string(),
    ])
}

/// Export every document as a JSON array.
fn export_all() -> Vec<Value> {
    let v = export_json(&[]);
    v.as_array()
        .expect("unfiltered export should be a JSON array")
        .clone()
}

/// `eg schema list` rows.
fn schema_list() -> Vec<Value> {
    let out = eg()
        .arg("schema")
        .arg("list")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).expect("schema list stdout should be JSON");
    v.as_array()
        .expect("schema list should be a JSON array")
        .clone()
}

/// Map `$id` -> document for the full export.
fn docs_by_id() -> HashMap<String, Value> {
    export_all()
        .into_iter()
        .map(|d| {
            let id = d
                .get("$id")
                .and_then(Value::as_str)
                .expect("every schema document needs an $id")
                .to_owned();
            (id, d)
        })
        .collect()
}

// ── Minimal JSON-Schema (2020-12 subset) validator ──────────────────────────
//
// Supports exactly the constructs the generator emits: local `$ref`
// (`#/$defs/<name>`), `type` (single or array), `const`, `enum`,
// `required`, `properties`, `additionalProperties`, `items`, `anyOf`,
// `allOf`, `oneOf`, `not`, `minLength`, `pattern`.

fn resolve_ref<'a>(schema: &'a Value, root: &'a Value) -> Result<&'a Value, String> {
    let reference = schema
        .get("$ref")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing $ref".to_owned())?;
    let name = reference
        .strip_prefix("#/$defs/")
        .ok_or_else(|| format!("unsupported $ref: {reference}"))?;
    root.get("$defs")
        .and_then(|d| d.get(name))
        .ok_or_else(|| format!("unresolvable $ref: {reference}"))
}

fn check_type(type_spec: &Value, instance: &Value, path: &str) -> Result<(), String> {
    let names: Vec<&str> = match type_spec {
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => return Err(format!("{path}: bad type spec")),
    };
    for name in &names {
        if !matches!(
            *name,
            "string" | "integer" | "number" | "boolean" | "array" | "object" | "null"
        ) {
            return Err(format!("{path}: unknown type name {name}"));
        }
    }
    let ok = names.iter().any(|n| match *n {
        "string" => instance.is_string(),
        "integer" => instance.is_i64() || instance.is_u64(),
        "number" => instance.is_number(),
        "boolean" => instance.is_boolean(),
        "array" => instance.is_array(),
        "object" => instance.is_object(),
        _ => instance.is_null(),
    });
    if ok {
        Ok(())
    } else {
        Err(format!("{path}: expected type {type_spec}, got {instance}"))
    }
}

fn validate_against(
    schema: &Value,
    instance: &Value,
    root: &Value,
    path: &str,
) -> Result<(), String> {
    let schema = if schema.get("$ref").is_some() {
        resolve_ref(schema, root)?
    } else {
        schema
    };

    if let Some(t) = schema.get("type") {
        check_type(t, instance, path)?;
    }
    if let Some(c) = schema.get("const")
        && instance != c
    {
        return Err(format!("{path}: expected const {c}, got {instance}"));
    }
    if let Some(e) = schema.get("enum") {
        let allowed = e.as_array().cloned().unwrap_or_default();
        if !allowed.contains(instance) {
            return Err(format!("{path}: {instance} not in enum {e}"));
        }
    }

    if let Some(obj) = instance.as_object() {
        if let Some(req) = schema.get("required").and_then(Value::as_array) {
            for name in req.iter().filter_map(Value::as_str) {
                if !obj.contains_key(name) {
                    return Err(format!("{path}: missing required property {name}"));
                }
            }
        }
        let empty = Map::new();
        let props = schema
            .get("properties")
            .and_then(Value::as_object)
            .unwrap_or(&empty);
        for (name, value) in obj {
            if let Some(sub) = props.get(name) {
                validate_against(sub, value, root, &format!("{path}.{name}"))?;
            } else if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                return Err(format!("{path}: unexpected property {name}"));
            }
        }
    }

    if let Some(arr) = instance.as_array()
        && let Some(items) = schema.get("items")
    {
        for (i, item) in arr.iter().enumerate() {
            validate_against(items, item, root, &format!("{path}[{i}]"))?;
        }
    }

    if let Some(s) = instance.as_str() {
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
            && (s.chars().count() as u64) < min
        {
            return Err(format!("{path}: string shorter than minLength {min}"));
        }
        if let Some(pat) = schema.get("pattern").and_then(Value::as_str) {
            let re = regex_lite(pat)?;
            if !re.is_match(s) {
                return Err(format!("{path}: {s:?} does not match pattern {pat:?}"));
            }
        }
    }

    if let Some(subs) = schema.get("anyOf").and_then(Value::as_array) {
        let mut errors = Vec::new();
        for sub in subs {
            match validate_against(sub, instance, root, path) {
                Ok(()) => return Ok(()),
                Err(e) => errors.push(e),
            }
        }
        return Err(format!(
            "{path}: no anyOf branch matched: {}",
            errors.join("; ")
        ));
    }
    if let Some(subs) = schema.get("allOf").and_then(Value::as_array) {
        for sub in subs {
            validate_against(sub, instance, root, path)?;
        }
    }
    if let Some(subs) = schema.get("oneOf").and_then(Value::as_array) {
        let hits = subs
            .iter()
            .filter(|sub| validate_against(sub, instance, root, path).is_ok())
            .count();
        if hits != 1 {
            return Err(format!(
                "{path}: expected exactly one oneOf match, got {hits}"
            ));
        }
    }
    if let Some(not) = schema.get("not")
        && validate_against(not, instance, root, path).is_ok()
    {
        return Err(format!("{path}: instance matched forbidden `not` schema"));
    }
    Ok(())
}

/// Tiny regex engine for the anchored patterns the generator emits
/// (`^literal-prefix` with no other metacharacters). Full regex crates are
/// unnecessary: every emitted pattern is a literal prefix anchor.
fn regex_lite(pattern: &str) -> Result<LiteRegex, String> {
    let prefix = pattern
        .strip_prefix('^')
        .ok_or_else(|| format!("unsupported pattern {pattern:?}"))?;
    if prefix.chars().any(|c| ".+*?()[]{}|\\$".contains(c)) {
        return Err(format!("unsupported pattern {pattern:?}"));
    }
    Ok(LiteRegex {
        prefix: prefix.to_owned(),
    })
}

struct LiteRegex {
    prefix: String,
}

impl LiteRegex {
    fn is_match(&self, s: &str) -> bool {
        s.starts_with(&self.prefix)
    }
}

/// Validate one JSONL record against the schema for its tuple.
fn validate_record(
    docs: &HashMap<String, Value>,
    list: &[Value],
    record: &Value,
) -> Result<(), String> {
    let record_type = record
        .get("record_type")
        .and_then(Value::as_str)
        .ok_or_else(|| "record has no record_type".to_owned())?;
    let kind = record
        .get("kind")
        .or_else(|| record.get("label"))
        .and_then(Value::as_str)
        .ok_or_else(|| "record has no kind/label".to_owned())?;
    let version = record
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| "record has no schema_version".to_owned())?;
    let domain = record
        .get("domain")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            record
                .get("id")
                .and_then(Value::as_str)
                .and_then(|id| id.split(':').next().map(str::to_owned))
        })
        .ok_or_else(|| "record has no domain or id".to_owned())?;
    let kind_key = if record_type == "tombstone" {
        "Tombstone"
    } else {
        kind
    };
    let entry = list
        .iter()
        .find(|e| {
            e.get("domain").and_then(Value::as_str) == Some(domain.as_str())
                && e.get("record_type").and_then(Value::as_str) == Some(record_type)
                && e.get("kind").and_then(Value::as_str) == Some(kind_key)
                && e.get("schema_version").and_then(Value::as_u64) == Some(version)
        })
        .ok_or_else(|| format!("no schema for tuple ({domain}, {kind_key}, v{version})"))?;
    let id = entry
        .get("$id")
        .and_then(Value::as_str)
        .ok_or_else(|| "list entry has no $id".to_owned())?;
    let doc = docs
        .get(id)
        .ok_or_else(|| format!("export is missing document {id}"))?;
    let rec_id = record.get("id").and_then(Value::as_str).unwrap_or("?");
    validate_against(doc, record, doc, &format!("record {rec_id}"))
        .map_err(|e| format!("{rec_id}: {e}"))
}

/// Read JSONL records from a file, skipping blank lines and non-record lines
/// (e.g. producer headers / manifests).
fn read_records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).expect("fixture should be readable");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| {
            matches!(
                v.get("record_type").and_then(Value::as_str),
                Some("node" | "edge" | "tombstone")
            )
        })
        .collect()
}

// ── Command-shape tests ─────────────────────────────────────────────────────

#[test]
fn single_document_is_draft_2020_12() {
    let doc = export_single("codegraph", "Symbol", CODEGRAPH_CURRENT_VERSION);
    assert_eq!(
        doc.get("$schema").and_then(Value::as_str),
        Some(DRAFT_2020_12),
        "schema must declare draft 2020-12"
    );
    assert!(
        doc.get("$id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.contains("codegraph") && id.contains("Symbol")),
        "schema needs a stable addressable $id"
    );
    assert!(
        doc.get("title").and_then(Value::as_str).is_some(),
        "schema needs a title"
    );
    assert!(
        doc.get("description").and_then(Value::as_str).is_some(),
        "schema needs a description"
    );
    let required: Vec<&str> = doc
        .get("required")
        .and_then(Value::as_array)
        .expect("node schema needs required")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    for field in [
        "id",
        "record_type",
        "kind",
        "schema_version",
        "summary",
        "producer",
    ] {
        assert!(
            required.contains(&field),
            "codegraph Symbol v{CODEGRAPH_CURRENT_VERSION} should require {field}; got {required:?}"
        );
    }
    // Legacy versions stay legacy (docs/schema/producer-version.md §6): the
    // v10 document must NOT require the producer envelope anymore.
    let legacy_doc = export_single("codegraph", "Symbol", 10);
    let legacy_required: Vec<&str> = legacy_doc
        .get("required")
        .and_then(Value::as_array)
        .expect("legacy node schema needs required")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        !legacy_required.contains(&"producer"),
        "codegraph Symbol v10 is legacy and must not require producer; got {legacy_required:?}"
    );
    // Kind and version are pinned to the addressable tuple.
    let props = doc.get("properties").expect("needs properties");
    assert_eq!(
        props
            .get("kind")
            .and_then(|k| k.get("const"))
            .and_then(Value::as_str),
        Some("Symbol")
    );
    assert_eq!(
        props
            .get("schema_version")
            .and_then(|v| v.get("const"))
            .and_then(Value::as_u64),
        Some(u64::from(CODEGRAPH_CURRENT_VERSION))
    );
}

#[test]
fn export_all_count_matches_list() {
    let docs = export_all();
    let list = schema_list();
    assert_eq!(
        docs.len(),
        list.len(),
        "every listed tuple must have exactly one exported document"
    );
    assert!(
        docs.len() >= 300,
        "expected full coverage of the record contract space, got {} documents",
        docs.len()
    );
    // $id uniqueness: each document is addressable exactly once.
    let mut ids: Vec<&str> = docs
        .iter()
        .filter_map(|d| d.get("$id").and_then(Value::as_str))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), docs.len(), "$ids must be unique");
    // Every document is draft 2020-12.
    for doc in &docs {
        assert_eq!(
            doc.get("$schema").and_then(Value::as_str),
            Some(DRAFT_2020_12),
            "document {} must declare draft 2020-12",
            doc.get("$id").and_then(Value::as_str).unwrap_or("?")
        );
    }
}

#[test]
fn export_is_deterministic() {
    let first = export_json(&[]);
    let second = export_json(&[]);
    assert_eq!(first, second, "schema export must be deterministic");
}

#[test]
fn list_contains_expected_tuples() {
    let list = schema_list();
    let has = |domain: &str, record_type: &str, kind: &str, version: u64| {
        list.iter().any(|e| {
            e.get("domain").and_then(Value::as_str) == Some(domain)
                && e.get("record_type").and_then(Value::as_str) == Some(record_type)
                && e.get("kind").and_then(Value::as_str) == Some(kind)
                && e.get("schema_version").and_then(Value::as_u64) == Some(version)
        })
    };
    // Spot-check every documented domain at its current version.
    assert!(has("codegraph", "node", "Symbol", 10));
    assert!(has("codegraph", "edge", "CALLS", 10));
    assert!(has("codegraph", "tombstone", "Tombstone", 10));
    assert!(has("agent_memory", "node", "Observation", 1));
    assert!(has("agent_memory", "node", "ToolCall", 1));
    assert!(has("verification", "node", "TestRun", 1));
    assert!(has("artifact", "node", "PatchArtifact", 1));
    assert!(has("project", "node", "Task", 1));
    assert!(has("semantic", "node", "SemanticDrift", 1));
    assert!(has("user_context", "node", "Preference", 1));
    assert!(has("log", "node", "LogSource", 3));
    assert!(has("log", "edge", "FINGERPRINTED_AS", 3));
    // Reader-accepted historical versions stay addressable.
    assert!(has("codegraph", "node", "Symbol", 5));
    assert!(has("log", "node", "LogEvent", 2));
}

#[test]
fn unknown_tuple_fails_cleanly() {
    // Unknown domain: non-zero exit, machine-readable JSON error on stderr.
    eg().arg("schema")
        .arg("export")
        .arg("--domain")
        .arg("nope")
        .assert()
        .failure()
        .stderr(predicate_contains(r#""ok":false"#));
    // Unknown kind in a known domain.
    eg().arg("schema")
        .arg("export")
        .arg("--domain")
        .arg("codegraph")
        .arg("--kind")
        .arg("Nope")
        .assert()
        .failure()
        .stderr(predicate_contains(r#""ok":false"#));
    // Unknown version for a known tuple.
    eg().arg("schema")
        .arg("export")
        .arg("--domain")
        .arg("codegraph")
        .arg("--kind")
        .arg("Symbol")
        .arg("--schema-version")
        .arg("999")
        .assert()
        .failure()
        .stderr(predicate_contains(r#""ok":false"#));
}

fn predicate_contains(needle: &str) -> predicates::str::ContainsPredicate {
    predicates::str::contains(needle)
}

#[test]
fn export_out_dir_writes_files() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("schemas");
    eg().arg("schema")
        .arg("export")
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    let entries: Vec<_> = fs::read_dir(&out)
        .expect("out dir should exist")
        .filter_map(Result::ok)
        .collect();
    let list = schema_list();
    assert_eq!(
        entries.len(),
        list.len(),
        "one file per addressable tuple; dir has {}, list has {}",
        entries.len(),
        list.len()
    );
    let sample = out.join("codegraph.node.Symbol.v10.schema.json");
    assert!(
        sample.is_file(),
        "expected deterministic file name {sample:?}"
    );
    let doc: Value = serde_json::from_str(&fs::read_to_string(&sample).expect("read sample"))
        .expect("valid JSON");
    assert_eq!(
        doc.get("$schema").and_then(Value::as_str),
        Some(DRAFT_2020_12)
    );
}

// ── Conformance: fixture records validate ───────────────────────────────────

#[test]
fn fixture_seed_graph_records_validate() {
    // tests/fixtures/evidence_pack/seed.graph.jsonl holds 55 legacy
    // codegraph v5 records — they must validate against the v5 schemas.
    let docs = docs_by_id();
    let list = schema_list();
    let records = read_records(&fixtures_dir().join("evidence_pack/seed.graph.jsonl"));
    assert!(!records.is_empty(), "seed fixture should have records");
    for record in &records {
        validate_record(&docs, &list, record)
            .unwrap_or_else(|e| panic!("fixture record failed to validate: {e}"));
    }
}

// ── Conformance: freshly produced records validate ──────────────────────────

/// Build a tiny Rust repo exercising extractor surface (symbols, unwrap,
/// unsafe, TODO debt marker, deprecated, test fn), scan it, and validate
/// every emitted record.
fn scan_fixture_repo() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("src")).expect("mkdir");
    fs::write(
        repo.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write Cargo.toml");
    fs::write(
        repo.join("src/lib.rs"),
        r#"/// Adds one.
pub fn add_one(x: i32) -> i32 {
    x + 1
}

/// Risky unwrap.
pub fn risky(opt: Option<i32>) -> i32 {
    opt.unwrap()
}

/// Unsafe block.
// TODO: replace with safe abstraction
pub unsafe fn raw() {}

#[deprecated(note = "use add_one")]
pub fn old_add(x: i32) -> i32 {
    add_one(x)
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert_eq!(super::add_one(1), 2);
    }
}
"#,
    )
    .expect("write lib.rs");
    let out = temp.path().join("graph.jsonl");
    eg().arg("scan")
        .arg(&repo)
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg("schema-export-fixture")
        .assert()
        .success();
    (temp, out)
}

#[test]
fn producer_scan_records_validate() {
    let (_temp, out) = scan_fixture_repo();
    let docs = docs_by_id();
    let list = schema_list();
    let records = read_records(&out);
    assert!(records.len() > 10, "scan should emit many records");
    let kinds: Vec<&str> = records
        .iter()
        .filter_map(|r| r.get("kind").and_then(Value::as_str))
        .collect();
    for probe in ["Symbol", "PanicRiskSite", "UnsafeSite", "DebtMarker"] {
        assert!(kinds.contains(&probe), "scan fixture should emit {probe}");
    }
    for record in &records {
        validate_record(&docs, &list, record)
            .unwrap_or_else(|e| panic!("scan record failed to validate: {e}"));
    }
}

#[test]
fn producer_import_codex_records_validate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("agent.jsonl");
    eg().arg("import-codex")
        .arg(fixtures_dir().join("agent_memory/codex_session/session.jsonl"))
        .arg("--out")
        .arg(&out)
        .assert()
        .success();
    let docs = docs_by_id();
    let list = schema_list();
    let records = read_records(&out);
    assert!(!records.is_empty(), "codex import should emit records");
    let kinds: Vec<&str> = records
        .iter()
        .filter_map(|r| r.get("kind").and_then(Value::as_str))
        .collect();
    assert!(
        kinds.contains(&"ToolCall"),
        "expected ToolCall records, got {kinds:?}"
    );
    for record in &records {
        validate_record(&docs, &list, record)
            .unwrap_or_else(|e| panic!("codex record failed to validate: {e}"));
    }
}

#[test]
fn producer_capture_tests_records_validate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("verify.jsonl");
    let input = fixtures_dir()
        .join("capture_tests/passing.json")
        .to_str()
        .expect("utf8")
        .to_owned();
    eg().arg("capture-tests")
        .arg("--input")
        .arg(input)
        .arg("--out")
        .arg(&out)
        .arg("--session-id")
        .arg("schema-export")
        .arg("--commit")
        .arg("abc123def456")
        .arg("--suite")
        .arg("mini-unit")
        .arg("--command")
        .arg("cargo test -- --format json")
        .arg("--exit-code")
        .arg("0")
        .arg("--executed-at")
        .arg("2026-07-19T12:00:00Z")
        .assert()
        .success();
    let docs = docs_by_id();
    let list = schema_list();
    let records = read_records(&out);
    assert!(!records.is_empty(), "capture-tests should emit records");
    for record in &records {
        validate_record(&docs, &list, record)
            .unwrap_or_else(|e| panic!("capture-tests record failed to validate: {e}"));
    }
}

#[test]
fn producer_scan_logs_records_validate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let log = temp.path().join("app.log");
    fs::write(
        &log,
        "2026-01-02T03:00:00Z [ERROR] request 00000001-e29b-41d4-a716-446655440000 failed\n\
         2026-01-02T03:01:00Z [ERROR] request 00000002-e29b-41d4-a716-446655440000 failed\n\
         2026-01-02T03:02:00Z [WARN] disk usage at 91 percent\n",
    )
    .expect("write log");
    let out = temp.path().join("logs.jsonl");
    eg().arg("scan-logs")
        .arg(&log)
        .arg("--repo-path")
        .arg(temp.path())
        .arg("--out")
        .arg(&out)
        .arg("--repo-id-override")
        .arg("schema-export-logs")
        .assert()
        .success();
    let docs = docs_by_id();
    let list = schema_list();
    let records = read_records(&out);
    assert!(
        records.len() >= 3,
        "scan-logs should emit source+signatures, got {}",
        records.len()
    );
    let kinds: Vec<&str> = records
        .iter()
        .filter_map(|r| r.get("kind").and_then(Value::as_str))
        .collect();
    assert!(
        kinds.contains(&"LogSource"),
        "expected LogSource, got {kinds:?}"
    );
    for record in &records {
        validate_record(&docs, &list, record)
            .unwrap_or_else(|e| panic!("scan-logs record failed to validate: {e}"));
    }
}

#[test]
fn write_observation_records_validate() {
    let temp = tempfile::tempdir().expect("temp dir");
    let out = temp.path().join("obs.jsonl");
    eg().args([
        "write",
        "observation",
        "--agent-id",
        "agent-1",
        "--agent-kind",
        "other",
        "--session-id",
        "s1",
        "--observed-at",
        "2026-05-30T10:00:00Z",
        "--source-handle",
        "src/lib.rs:sha256:abc",
        "--text",
        "the symbol exists",
        "--evidence-target",
        "codegraph:v10:abc",
        "--out",
        out.to_str().expect("utf8"),
    ])
    .assert()
    .success();
    let docs = docs_by_id();
    let list = schema_list();
    let records = read_records(&out);
    // `write observation` emits the Observation node plus its supporting
    // Agent, AgentSession, and edge records; all must validate.
    assert!(!records.is_empty(), "write observation should emit records");
    assert!(
        records
            .iter()
            .any(|r| r.get("kind").and_then(Value::as_str) == Some("Observation")),
        "write observation should emit an Observation node"
    );
    for record in &records {
        validate_record(&docs, &list, record)
            .unwrap_or_else(|e| panic!("observation record failed to validate: {e}"));
    }
}

// ── Negative tests: invalid records must fail ───────────────────────────────

/// Build a minimal record for a tuple, starting from all-absent optionals.
fn base_node(domain: &str, kind: &str, version: u64) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("record_type".into(), Value::String("node".into()));
    m.insert(
        "id".into(),
        Value::String(format!("{domain}:v{version}:test01")),
    );
    m.insert("kind".into(), Value::String(kind.into()));
    m.insert("schema_version".into(), Value::Number(version.into()));
    m.insert("summary".into(), Value::String("test record".into()));
    m
}

fn producer_envelope() -> Value {
    json!({
        "egregore_version": "0.1.0",
        "producer_kind": "code_graph_extractor",
        "producer_components": {"egregore": "0.1.0"},
        "producer_started_at": "2026-09-19T00:00:00Z"
    })
}

fn assert_valid(docs: &HashMap<String, Value>, list: &[Value], record: &Value) {
    validate_record(docs, list, record)
        .unwrap_or_else(|e| panic!("expected record to validate: {e}\nrecord: {record}"));
}

fn assert_invalid(docs: &HashMap<String, Value>, list: &[Value], record: &Value) {
    assert!(
        validate_record(docs, list, record).is_err(),
        "expected record to FAIL validation, but it passed:\n{record}"
    );
}

#[test]
fn negative_missing_required_record_field() {
    let docs = docs_by_id();
    let list = schema_list();
    // A codegraph Symbol missing `summary` (a base required field) fails.
    let mut rec = base_node("codegraph", "Symbol", 10);
    rec.remove("summary");
    rec.insert("producer".into(), producer_envelope());
    assert_invalid(&docs, &list, &Value::Object(rec));
}

#[test]
fn negative_missing_producer_envelope() {
    let docs = docs_by_id();
    let list = schema_list();
    // Current-version records require the producer envelope.
    let rec = base_node("codegraph", "Symbol", u64::from(CODEGRAPH_CURRENT_VERSION));
    assert_invalid(&docs, &list, &Value::Object(rec));
    // ...but legacy versions predate the envelope (docs/schema/producer-version.md §4).
    let mut legacy = base_node("codegraph", "Symbol", 5);
    legacy.insert("id".into(), Value::String("codegraph:v5:test01".into()));
    assert_valid(&docs, &list, &Value::Object(legacy));
    // v10 was current before the #224 schema bump and is legacy now (§6):
    // a producer-less v10 record validates.
    let legacy_v10 = base_node("codegraph", "Symbol", 10);
    assert_valid(&docs, &list, &Value::Object(legacy_v10));
}

#[test]
fn negative_codegraph_forbids_subjective_fields() {
    let docs = docs_by_id();
    let list = schema_list();
    // Deterministic code facts must not carry agent-authored provenance.
    let mut rec = base_node("codegraph", "Symbol", 10);
    rec.insert("producer".into(), producer_envelope());
    rec.insert("agent_id".into(), Value::String("agent-1".into()));
    rec.insert("text".into(), Value::String("an agent wrote this".into()));
    assert_invalid(&docs, &list, &Value::Object(rec));

    // The same shape without subjective fields validates.
    let mut clean = base_node("codegraph", "Symbol", 10);
    clean.insert("producer".into(), producer_envelope());
    assert_valid(&docs, &list, &Value::Object(clean));
}

#[test]
fn negative_agent_memory_requires_provenance() {
    let docs = docs_by_id();
    let list = schema_list();
    // An Observation without agent provenance fails ...
    let mut rec = base_node("agent_memory", "Observation", 1);
    rec.insert("id".into(), Value::String("agent_memory:v1:test01".into()));
    rec.insert("producer".into(), producer_envelope());
    rec.insert("text".into(), Value::String("saw a thing".into()));
    rec.insert("confidence".into(), json!("0.9"));
    rec.insert(
        "evidence_links".into(),
        json!([{
            "target_record_id": "codegraph:v10:abc",
            "target_domain": "codegraph",
            "relation": "OBSERVES",
            "confidence": "0.9"
        }]),
    );
    assert_invalid(&docs, &list, &Value::Object(rec.clone()));

    // ... and validates once provenance is present.
    for (k, v) in [
        ("agent_id", json!("agent-1")),
        ("agent_kind", json!("other")),
        ("session_id", json!("s1")),
        ("observed_at", json!("2026-05-30T10:00:00Z")),
        ("ingested_at", json!("2026-05-30T10:00:01Z")),
    ] {
        rec.insert(k.into(), v);
    }
    assert_valid(&docs, &list, &Value::Object(rec));
}

#[test]
fn negative_verification_requires_evidence_handle() {
    let docs = docs_by_id();
    let list = schema_list();
    // A verification TestRun must cite at least one evidence handle.
    let mut rec = base_node("verification", "TestRun", 1);
    rec.insert("id".into(), Value::String("verification:v1:test01".into()));
    rec.insert("domain".into(), Value::String("verification".into()));
    rec.insert("producer".into(), producer_envelope());
    assert_invalid(&docs, &list, &Value::Object(rec.clone()));

    rec.insert(
        "source_artifact_path".into(),
        Value::String("tests/fixtures/capture_tests/passing.json".into()),
    );
    assert_valid(&docs, &list, &Value::Object(rec));
}

#[test]
fn negative_wrong_tuple_values_fail() {
    let docs = docs_by_id();
    let list = schema_list();
    // schema_version must equal the addressed version.
    let mut rec = base_node("codegraph", "Symbol", 10);
    rec.insert("producer".into(), producer_envelope());
    rec.insert("schema_version".into(), json!(9));
    assert_invalid(&docs, &list, &Value::Object(rec.clone()));

    // kind must equal the addressed kind.
    let mut rec2 = base_node("codegraph", "Symbol", 10);
    rec2.insert("producer".into(), producer_envelope());
    rec2.insert("kind".into(), Value::String("File".into()));
    assert_invalid(&docs, &list, &Value::Object(rec2));
}

#[test]
fn every_schema_self_describes() {
    // Every document carries the tuple it addresses plus trust/provenance
    // documentation, so consumers never need out-of-band knowledge.
    for doc in export_all() {
        let id = doc.get("$id").and_then(Value::as_str).unwrap_or("?");
        let desc = doc
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            desc.contains("domain") || desc.contains("Domain"),
            "{id}: description should name the domain"
        );
        assert!(
            doc.get("properties")
                .is_some_and(|p| p.get("producer").is_some()),
            "{id}: schema must declare the producer envelope"
        );
    }
}
