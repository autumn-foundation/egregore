//! Integration tests for the v1 redaction policy engine (`src/redaction.rs`).
//!
//! Covers AC from issue #41: `detect_secret`, `is_redacted`, `redact_value`,
//! `validate_record`, `with_redaction_policy_version`, and `ImportOptions`
//! default/passthrough behaviour for `traj` and `codex` importers.

use aletheia_egregore::ir::OutputHandle;
use aletheia_egregore::{
    CodegraphError, EdgeLabel, GraphRecord, NodeKind, PatchHandle, UserContextFields,
    redaction::{
        REDACTION_POLICY_VERSION, SecretClass, detect_secret, is_redacted, redact_value,
        validate_record,
    },
};

// ── Secret fixtures ──────────────────────────────────────────────────────────
//
// These constants hold real secret-shaped values used by the detectors.
// Declared as constants (not doc comments) so they are inert to Clippy's
// sensitive-value lints while still being importable by each test.

const SECRET_API_TOKEN_SK: &str = "sk-prod-abcdefghijklmnopqrstuvwxyz1234567890XXXX";
const SECRET_API_TOKEN_GH: &str = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ1234567890";
const SECRET_API_TOKEN_BEARER: &str =
    "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456789XXYY";
const SECRET_SSH_PRIVATE: &str = "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0Z3VS5JJcds3xHn";
const SECRET_DATABASE_URL: &str =
    "postgres://dbadmin:S3cr3tPa55word@prod-db.example.com:5432/myapp";
const SECRET_CLOUD_CRED: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET_WEBHOOK: &str = "whsec_abcdefghijklmnopqrstuvwxyz01234567890A";
const SECRET_SESSION_JWT: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9\
     .eyJzdWIiOiIxMjM0NTY3ODkwIn0\
     .SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV";
const SECRET_SESSION_NAMED: &str = "connect.sid=s%3AabcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN";
const SECRET_ENV: &str = "DATABASE_PASSWORD=SuperSecretDbPassword123!";

// ── Decoy fixtures (must NOT trigger detection) ───────────────────────────────

const DECOY_PLAIN_URL: &str = "postgres://localhost:5432/testdb";
const DECOY_COMMENT: &str = "# replace TOKEN with your actual token value";
const DECOY_EMPTY_KEY: &str = "SECRET_KEY=";
const DECOY_SHORT_VALUE: &str = "API_KEY=short";
const DECOY_PLAIN_TEXT: &str = "hello world no secrets here";

// ── Helpers ──────────────────────────────────────────────────────────────────

fn base_agent_node() -> GraphRecord {
    GraphRecord::node(
        "test-node-id".to_owned(),
        NodeKind::AgentSession,
        None,
        None,
        None,
        "test summary".to_owned(),
    )
}

fn parse_marker(marker: &str) -> Option<(&str, &str)> {
    let inner = marker.strip_prefix('<')?.strip_suffix('>')?;
    let mut parts = inner.splitn(3, ':');
    let tag = parts.next()?;
    if tag != "REDACTED" {
        return None;
    }
    let class = parts.next()?;
    let hash = parts.next()?;
    Some((class, hash))
}

// ── Detection tests (AC1-AC2: all 7 secret classes detected) ─────────────────

#[test]
fn detect_api_token_sk_prefix() {
    let (class, _) = detect_secret(SECRET_API_TOKEN_SK).expect("must detect sk- token");
    assert_eq!(class, SecretClass::ApiToken);
}

#[test]
fn detect_api_token_ghp_prefix() {
    let (class, _) = detect_secret(SECRET_API_TOKEN_GH).expect("must detect ghp_ token");
    assert_eq!(class, SecretClass::ApiToken);
}

#[test]
fn detect_api_token_bearer_header() {
    let (class, _) = detect_secret(SECRET_API_TOKEN_BEARER).expect("must detect Bearer token");
    assert_eq!(class, SecretClass::ApiToken);
}

#[test]
fn detect_ssh_private_key() {
    let (class, _) = detect_secret(SECRET_SSH_PRIVATE).expect("must detect SSH private key");
    assert_eq!(class, SecretClass::SshPrivateKey);
}

#[test]
fn detect_database_url_with_credentials() {
    let (class, _) =
        detect_secret(SECRET_DATABASE_URL).expect("must detect database URL with creds");
    assert_eq!(class, SecretClass::DatabaseUrl);
}

#[test]
fn detect_cloud_credential_akia() {
    let (class, _) = detect_secret(SECRET_CLOUD_CRED).expect("must detect AKIA cloud credential");
    assert_eq!(class, SecretClass::CloudCredential);
}

#[test]
fn detect_webhook_secret_whsec_prefix() {
    let (class, _) = detect_secret(SECRET_WEBHOOK).expect("must detect whsec_ webhook secret");
    assert_eq!(class, SecretClass::WebhookSecret);
}

#[test]
fn detect_session_cookie_jwt_eyj_prefix() {
    let (class, _) = detect_secret(SECRET_SESSION_JWT).expect("must detect eyJ JWT session token");
    assert_eq!(class, SecretClass::SessionCookie);
}

#[test]
fn detect_session_cookie_named_prefix() {
    let (class, _) = detect_secret(SECRET_SESSION_NAMED).expect("must detect sessionid= cookie");
    assert_eq!(class, SecretClass::SessionCookie);
}

#[test]
fn detect_env_secret_db_password() {
    let (class, _) = detect_secret(SECRET_ENV).expect("must detect DATABASE_PASSWORD env secret");
    assert_eq!(class, SecretClass::EnvSecret);
}

// ── No-match tests (AC2: decoys must not be flagged) ─────────────────────────

#[test]
fn detect_none_plain_postgres_url_no_credentials() {
    assert!(
        detect_secret(DECOY_PLAIN_URL).is_none(),
        "postgres URL without credentials must not be flagged"
    );
}

#[test]
fn detect_none_token_mention_in_comment() {
    assert!(
        detect_secret(DECOY_COMMENT).is_none(),
        "plain English mention of token must not be flagged"
    );
}

#[test]
fn detect_none_empty_env_value() {
    assert!(
        detect_secret(DECOY_EMPTY_KEY).is_none(),
        "SECRET_KEY with empty value must not be flagged"
    );
}

#[test]
fn detect_none_short_env_value() {
    assert!(
        detect_secret(DECOY_SHORT_VALUE).is_none(),
        "API_KEY= with fewer than 8 chars must not be flagged"
    );
}

#[test]
fn detect_none_plain_text() {
    assert!(
        detect_secret(DECOY_PLAIN_TEXT).is_none(),
        "plain English text must not be flagged"
    );
}

// ── is_redacted tests (AC3) ───────────────────────────────────────────────────

#[test]
fn is_redacted_true_for_marker_value() {
    let marker = "<REDACTED:api_token:abc123def456>";
    assert!(
        is_redacted(marker),
        "marker must be recognized as already-redacted"
    );
}

#[test]
fn is_redacted_false_for_clean_value() {
    assert!(
        !is_redacted(DECOY_PLAIN_TEXT),
        "clean value must not be recognized as redacted"
    );
}

#[test]
fn is_redacted_false_for_raw_secret() {
    assert!(
        !is_redacted(SECRET_API_TOKEN_SK),
        "raw secret must not be recognized as already-redacted"
    );
}

// ── redact_value tests (AC4: marker format, class names, hash prefix) ─────────

#[test]
fn redact_value_api_token_produces_api_token_class_marker() {
    let marker = redact_value(SECRET_API_TOKEN_SK);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "api_token");
}

#[test]
fn redact_value_ssh_key_produces_ssh_private_key_marker() {
    let marker = redact_value(SECRET_SSH_PRIVATE);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "ssh_private_key");
}

#[test]
fn redact_value_database_url_produces_database_url_marker() {
    let marker = redact_value(SECRET_DATABASE_URL);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "database_url");
}

#[test]
fn redact_value_cloud_credential_produces_cloud_credential_marker() {
    let marker = redact_value(SECRET_CLOUD_CRED);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "cloud_credential");
}

#[test]
fn redact_value_webhook_produces_webhook_secret_marker() {
    let marker = redact_value(SECRET_WEBHOOK);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "webhook_secret");
}

#[test]
fn redact_value_session_cookie_produces_session_cookie_marker() {
    let marker = redact_value(SECRET_SESSION_JWT);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "session_cookie");
}

#[test]
fn redact_value_env_secret_produces_env_secret_marker() {
    let marker = redact_value(SECRET_ENV);
    let (class, _) = parse_marker(&marker).expect("must produce a valid marker");
    assert_eq!(class, "env_secret");
}

#[test]
fn redact_value_clean_value_returned_unchanged() {
    let result = redact_value(DECOY_PLAIN_TEXT);
    assert_eq!(
        result, DECOY_PLAIN_TEXT,
        "clean value must be returned unchanged"
    );
}

#[test]
fn redact_value_already_redacted_marker_passes_through_unchanged() {
    let marker = "<REDACTED:api_token:abc123def456>";
    let result = redact_value(marker);
    assert_eq!(
        result, marker,
        "already-redacted marker must be returned unchanged"
    );
}

// ── validate_record exempt kinds (AC5: code-graph records are exempt) ─────────

#[test]
fn validate_record_ok_for_all_code_graph_kinds() {
    use NodeKind::{
        Change, Commit, Diagnostic, EmbeddingModel, EmbeddingVector, File, Import, Module,
        Repository, SemanticDrift, Symbol,
    };
    let kinds = [
        Repository,
        File,
        Module,
        Symbol,
        Import,
        Diagnostic,
        Commit,
        Change,
        SemanticDrift,
        EmbeddingModel,
        EmbeddingVector,
    ];
    for kind in kinds {
        let mut r = GraphRecord::node("id".to_owned(), kind, None, None, None, "s".to_owned());
        if let GraphRecord::Node { text: t, .. } = &mut r {
            *t = Some(SECRET_API_TOKEN_SK.to_owned());
        }
        validate_record(&r)
            .unwrap_or_else(|_| panic!("{kind:?} must be exempt from the redaction gate"));
    }
}

#[test]
fn validate_record_ok_for_edge_record() {
    let edge = GraphRecord::edge(
        EdgeLabel::AuthoredBy,
        "src-id".to_owned(),
        "tgt-id".to_owned(),
        None,
        "test edge".to_owned(),
    );
    validate_record(&edge).expect("edge records are exempt from the redaction gate");
}

// ── validate_record field-specific error tests (AC5-AC6) ─────────────────────

#[test]
fn validate_record_err_text_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some(SECRET_API_TOKEN_SK.to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "text");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_validation_summary_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node {
        validation_summary, ..
    } = &mut r
    {
        *validation_summary = Some(SECRET_DATABASE_URL.to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "validation_summary");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_arguments_summary_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node {
        arguments_summary, ..
    } = &mut r
    {
        *arguments_summary = Some(SECRET_API_TOKEN_SK.to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "arguments_summary");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_arguments_handle_inline_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node {
        arguments_handle, ..
    } = &mut r
    {
        *arguments_handle = Some(Box::new(OutputHandle {
            inline: Some(SECRET_API_TOKEN_SK.to_owned()),
            hash: "placeholder".to_owned(),
            bytes: 3,
        }));
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "arguments_handle.inline");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_result_handle_inline_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { result_handle, .. } = &mut r {
        *result_handle = Some(Box::new(OutputHandle {
            inline: Some(SECRET_API_TOKEN_SK.to_owned()),
            hash: "placeholder".to_owned(),
            bytes: 3,
        }));
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "result_handle.inline");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_patch_handle_inline_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { patch_handle, .. } = &mut r {
        *patch_handle = Some(Box::new(PatchHandle {
            path: "artifact://test-patch".to_owned(),
            inline: Some(SECRET_API_TOKEN_SK.to_owned()),
        }));
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "patch_handle.inline");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_stdout_handle_inline_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { stdout_handle, .. } = &mut r {
        *stdout_handle = Some(Box::new(OutputHandle {
            inline: Some(SECRET_API_TOKEN_SK.to_owned()),
            hash: "placeholder".to_owned(),
            bytes: 3,
        }));
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "stdout_handle.inline");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_stderr_handle_inline_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { stderr_handle, .. } = &mut r {
        *stderr_handle = Some(Box::new(OutputHandle {
            inline: Some(SECRET_API_TOKEN_SK.to_owned()),
            hash: "placeholder".to_owned(),
            bytes: 3,
        }));
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "stderr_handle.inline");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_title_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { title, .. } = &mut r {
        *title = Some(SECRET_API_TOKEN_SK.to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "title");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_body_handle_inline_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { body_handle, .. } = &mut r {
        *body_handle = Some(Box::new(OutputHandle {
            inline: Some(SECRET_API_TOKEN_SK.to_owned()),
            hash: "placeholder".to_owned(),
            bytes: 3,
        }));
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "body_handle.inline");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_url_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { url, .. } = &mut r {
        *url = Some(SECRET_DATABASE_URL.to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "url");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_assignees_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { assignees, .. } = &mut r {
        *assignees = Some(vec![SECRET_API_TOKEN_SK.to_owned()]);
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "assignees[0]");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

#[test]
fn validate_record_err_labels_field() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { labels, .. } = &mut r {
        *labels = Some(vec![SECRET_API_TOKEN_SK.to_owned()]);
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "labels[0]");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

// ── User-context field tests (AC6: all 7 user-context sensitive fields) ───────

#[test]
fn validate_record_err_for_all_user_context_fields() {
    let make = |setter: fn(&mut UserContextFields)| {
        let mut r = base_agent_node();
        if let GraphRecord::Node { user_context, .. } = &mut r {
            setter(user_context);
        }
        r
    };

    let domains: Vec<(&str, GraphRecord)> = vec![
        (
            "proposed_rule_text",
            make(|uc| uc.proposed_rule_text = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
        (
            "prompt_text",
            make(|uc| uc.prompt_text = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
        (
            "decision_rationale",
            make(|uc| uc.decision_rationale = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
        (
            "edited_rule_text",
            make(|uc| uc.edited_rule_text = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
        (
            "rule_text",
            make(|uc| uc.rule_text = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
        (
            "action_summary",
            make(|uc| uc.action_summary = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
        (
            "constraint_text",
            make(|uc| uc.constraint_text = Some(SECRET_API_TOKEN_SK.to_owned())),
        ),
    ];

    for (expected_field, record) in &domains {
        match validate_record(record) {
            Ok(()) => {
                panic!("user_context field '{expected_field}' with raw secret must be rejected")
            }
            Err(CodegraphError::RedactionRequired { field_path }) => {
                assert_eq!(
                    &field_path, expected_field,
                    "wrong field path for user_context.{expected_field}"
                );
            }
            Err(other) => panic!("expected RedactionRequired for {expected_field}, got {other:?}"),
        }
    }
}

// ── Already-redacted marker passes gate when policy version is stamped (AC5) ────

#[test]
fn validate_record_ok_when_text_field_carries_redaction_marker() {
    let mut r = base_agent_node();
    if let GraphRecord::Node {
        text,
        redaction_policy_version,
        ..
    } = &mut r
    {
        *text = Some("<REDACTED:api_token:abc123def456>".to_owned());
        *redaction_policy_version = Some("v1".to_owned());
    }
    validate_record(&r)
        .expect("already-redacted marker with policy version must pass the validation gate");
}

// ── Error safety: RedactionRequired never echoes the raw secret (AC6) ─────────

#[test]
fn validate_record_error_does_not_echo_raw_secret_value() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some(SECRET_API_TOKEN_SK.to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    let err_display = err.to_string();
    assert!(
        !err_display.contains(SECRET_API_TOKEN_SK),
        "RedactionRequired error must not echo the raw secret value; got: {err_display}"
    );
    // Error must name the field path instead.
    assert!(
        err_display.contains("text"),
        "RedactionRequired error must name the field path; got: {err_display}"
    );
}

// ── with_redaction_policy_version builder (AC4) ───────────────────────────────

#[test]
fn with_redaction_policy_version_stamps_field_on_node() {
    let r = base_agent_node().with_redaction_policy_version(REDACTION_POLICY_VERSION);
    match r {
        GraphRecord::Node {
            redaction_policy_version,
            ..
        } => {
            assert_eq!(
                redaction_policy_version.as_deref(),
                Some("v1"),
                "with_redaction_policy_version must stamp 'v1' on the node"
            );
        }
        other => panic!("expected Node variant, got {other:?}"),
    }
}

// ── ImportOptions default / passthrough (AC3, AC4) ────────────────────────────

#[test]
fn import_traj_options_default_applies_v1_redaction() {
    let opts = aletheia_egregore::traj::ImportOptions::default();
    let result = (opts.redact)(SECRET_API_TOKEN_SK);
    assert!(
        result.starts_with("<REDACTED:"),
        "traj ImportOptions::default() must apply v1 redaction; got: {result}"
    );
    assert!(
        !result.contains(SECRET_API_TOKEN_SK),
        "traj ImportOptions::default() must not pass through raw secret; got: {result}"
    );
}

#[test]
fn import_codex_options_default_applies_v1_redaction() {
    let opts = aletheia_egregore::codex::ImportOptions::default();
    let result = (opts.redact)(SECRET_API_TOKEN_SK);
    assert!(
        result.starts_with("<REDACTED:"),
        "codex ImportOptions::default() must apply v1 redaction; got: {result}"
    );
    assert!(
        !result.contains(SECRET_API_TOKEN_SK),
        "codex ImportOptions::default() must not pass through raw secret; got: {result}"
    );
}

#[test]
fn import_options_passthrough_does_not_redact() {
    let traj_pass = aletheia_egregore::traj::ImportOptions::passthrough();
    let result = (traj_pass.redact)(SECRET_API_TOKEN_SK);
    assert_eq!(
        result, SECRET_API_TOKEN_SK,
        "traj ImportOptions::passthrough() must not redact"
    );

    let codex_pass = aletheia_egregore::codex::ImportOptions::passthrough();
    let result2 = (codex_pass.redact)(SECRET_API_TOKEN_SK);
    assert_eq!(
        result2, SECRET_API_TOKEN_SK,
        "codex ImportOptions::passthrough() must not redact"
    );
}

// ── Review-fix tests ──────────────────────────────────────────────────────────

// Fix 1: agent-memory Diagnostic nodes must pass through the gate.
#[test]
fn validate_record_err_agent_memory_diagnostic_with_raw_secret() {
    let mut r = GraphRecord::node(
        "diag-id".to_owned(),
        NodeKind::Diagnostic,
        None,
        None,
        None,
        "diagnostic summary".to_owned(),
    );
    if let GraphRecord::Node { text, domain, .. } = &mut r {
        *text = Some(SECRET_API_TOKEN_SK.to_owned());
        *domain = Some("agent_memory".to_owned());
    }
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionRequired { field_path } => {
            assert_eq!(field_path, "text");
        }
        other => panic!("expected RedactionRequired, got {other:?}"),
    }
}

// Fix 1 (complement): code-graph Diagnostic (no domain) must still be exempt.
#[test]
fn validate_record_ok_for_code_graph_diagnostic_no_domain() {
    let mut r = GraphRecord::node(
        "diag-id".to_owned(),
        NodeKind::Diagnostic,
        None,
        None,
        None,
        "parse error".to_owned(),
    );
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some(SECRET_API_TOKEN_SK.to_owned());
    }
    // domain is None (code-graph diagnostic) — must be exempt
    validate_record(&r).expect("code-graph Diagnostic with no domain must be exempt");
}

// Fix 2: default ImportOptions stamps policy_version on every node.
#[test]
fn import_options_default_sets_policy_version_field() {
    let opts = aletheia_egregore::traj::ImportOptions::default();
    assert_eq!(
        opts.policy_version,
        Some("v1"),
        "default ImportOptions must carry policy_version v1"
    );
    let codex_opts = aletheia_egregore::codex::ImportOptions::default();
    assert_eq!(
        codex_opts.policy_version,
        Some("v1"),
        "codex default ImportOptions must carry policy_version v1"
    );
}

// Fix 2 (complement): passthrough ImportOptions must NOT carry a policy_version.
#[test]
fn import_options_passthrough_has_no_policy_version() {
    let opts = aletheia_egregore::traj::ImportOptions::passthrough();
    assert!(
        opts.policy_version.is_none(),
        "passthrough ImportOptions must not set policy_version"
    );
}

// Fix 4: password-only Redis URL (empty user) must be detected as a database credential.
#[test]
fn detect_database_url_redis_password_only_no_username() {
    let url = "redis://:p4ssw0rd@cache.example.com:6379/0";
    let (class, _) = detect_secret(url).expect("redis password-only URL must be detected");
    assert_eq!(class, SecretClass::DatabaseUrl);
}

// Fix 5: Bearer token detection is case-insensitive.
#[test]
fn detect_api_token_bearer_lowercase() {
    let header = "authorization: bearer abcdefghijklmnopqrstuvwxyz123456789XXYY";
    let (class, _) = detect_secret(header).expect("lowercase bearer token must be detected");
    assert_eq!(class, SecretClass::ApiToken);
}

#[test]
fn detect_api_token_bearer_uppercase() {
    let header = "AUTHORIZATION: BEARER ABCDEFGHIJKLMNOPQRSTUVWXYZ123456789XXYY";
    let (class, _) = detect_secret(header).expect("uppercase BEARER token must be detected");
    assert_eq!(class, SecretClass::ApiToken);
}

// ── Round 2 review-fix tests ──────────────────────────────────────────────────

// Fix R2-1: marker-bearing field without policy version is rejected.
#[test]
fn validate_record_err_marker_field_without_policy_version() {
    let mut r = base_agent_node();
    if let GraphRecord::Node { text, .. } = &mut r {
        *text = Some("<REDACTED:api_token:abc123def456>".to_owned());
    }
    // redaction_policy_version is None (not set) — must be rejected
    let err = validate_record(&r).unwrap_err();
    match err {
        CodegraphError::RedactionMetadataMissing { field_path } => {
            assert_eq!(field_path, "text");
        }
        other => panic!("expected RedactionMetadataMissing, got {other:?}"),
    }
}

// Fix R2-1 (complement): marker + policy version present → Ok.
#[test]
fn validate_record_ok_marker_field_with_policy_version_set() {
    let mut r = base_agent_node();
    if let GraphRecord::Node {
        text,
        redaction_policy_version,
        ..
    } = &mut r
    {
        *text = Some("<REDACTED:api_token:abc123def456>".to_owned());
        *redaction_policy_version = Some("v1".to_owned());
    }
    validate_record(&r)
        .expect("marker field with redaction_policy_version set must pass the validation gate");
}

// Fix R2-2: database URL scanner catches credentialed URL after a username-only decoy.
// The decoy `postgres://readonly@host/db` has a username but NO password (no `:` in userinfo).
// With only first-occurrence scanning the second credentialed URL is never examined.
#[test]
fn detect_database_url_second_occurrence_after_decoy() {
    let value =
        "config: postgres://readonly@localhost/dev prod: postgres://admin:S3cr3t@prod.example.com/myapp";
    let (class, _) = detect_secret(value).expect(
        "credentialed postgres:// after a username-only decoy must still be detected",
    );
    assert_eq!(class, SecretClass::DatabaseUrl);
}

// Fix R2-2: redact_value also catches the credentialed second occurrence.
#[test]
fn redact_value_detects_second_database_url_occurrence() {
    let value =
        "config: postgres://readonly@localhost/test secret: postgres://u:p4ssw0rd@db.example.com/prod";
    let result = redact_value(value);
    assert!(
        result.starts_with("<REDACTED:"),
        "redact_value must redact the credentialed postgres:// even after a username-only decoy; got: {result}"
    );
}

// Fix R2-3: webhook scanner catches long secret after a short non-secret prefix occurrence.
#[test]
fn detect_webhook_secret_second_occurrence_after_short() {
    // First whsec_ candidate is shorter than 20 chars; second is the real secret.
    let value = "example whsec_short whsec_abcdefghijklmnopqrstuvwxyz012345";
    let (class, _) = detect_secret(value)
        .expect("real whsec_ secret after a short decoy occurrence must still be detected");
    assert_eq!(class, SecretClass::WebhookSecret);
}

// Fix R2-4: MongoDB Atlas SRV connection strings are detected as database credentials.
#[test]
fn detect_database_url_mongodb_srv_scheme() {
    let url = "mongodb+srv://atlasUser:AtlasP4ssw0rd@cluster0.mongodb.net/mydb?retryWrites=true";
    let (class, _) =
        detect_secret(url).expect("mongodb+srv:// URL with credentials must be detected");
    assert_eq!(class, SecretClass::DatabaseUrl);
}

// Fix R2-5: GitHub fine-grained PAT prefix (github_pat_) is detected as api_token.
#[test]
fn detect_api_token_github_fine_grained_pat() {
    let token = "github_pat_abcdefghijklmnopqrstuvwxyz1234567890ABCDEFGHIJ";
    let (class, _) = detect_secret(token).expect("github_pat_ token must be detected");
    assert_eq!(class, SecretClass::ApiToken);
}
