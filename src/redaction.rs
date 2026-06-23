//! Redaction policy engine — `docs/schema/redaction.md` v1.
//!
//! Implements the v1 redaction policy for sensitive fields in agent-memory,
//! artifact, verification, project, and user-context graph records.
//! Code-graph records (`Repository`, `File`, `Symbol`, etc.) are explicitly
//! outside the gate.
//!
//! # Public surface
//!
//! - [`REDACTION_POLICY_VERSION`]: version string stamped on redacted records.
//! - [`SecretClass`]: the seven named secret classes.
//! - [`detect_secret`]: detect the first secret in a string value.
//! - [`redact_value`]: replace the value with a `<REDACTED:class:hash>` marker.
//! - [`is_redacted`]: check whether a value already carries a redaction marker.
//! - [`validate_record`]: gate — reject records with unredacted sensitive fields.

use crate::{
    error::{CodegraphError, Result},
    ir::{GraphRecord, NodeKind},
};

/// Redaction policy version stamped on agent-authored records that passed
/// through redaction.
///
/// Documented in `docs/schema/redaction.md`.
pub const REDACTION_POLICY_VERSION: &str = "v1";

/// Number of hex characters taken from the BLAKE3 hash as the audit prefix.
///
/// 12 hex digits = 6 bytes = 48 bits of correlation space. Not reversible to
/// the secret but sufficient for audit cross-referencing.
const HASH_PREFIX_LEN: usize = 12;

// ── Secret classes ─────────────────────────────────────────────────────────────

/// Named secret classes defined in `docs/schema/redaction.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretClass {
    /// API keys, bearer tokens, OAuth access/refresh tokens.
    ApiToken,
    /// PEM/OpenSSH private key material.
    SshPrivateKey,
    /// Database URLs with embedded credentials.
    DatabaseUrl,
    /// Cloud access keys, secret keys, service-account secrets.
    CloudCredential,
    /// Webhook signing secrets and shared callback tokens.
    WebhookSecret,
    /// Auth/session cookies and JWT session tokens.
    SessionCookie,
    /// `.env`-style `KEY=VALUE` whose key matches the secret-name allowlist.
    EnvSecret,
}

impl SecretClass {
    /// Returns the canonical class name used in redaction markers.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ApiToken => "api_token",
            Self::SshPrivateKey => "ssh_private_key",
            Self::DatabaseUrl => "database_url",
            Self::CloudCredential => "cloud_credential",
            Self::WebhookSecret => "webhook_secret",
            Self::SessionCookie => "session_cookie",
            Self::EnvSecret => "env_secret",
        }
    }
}

// ── Detection ──────────────────────────────────────────────────────────────────

/// Detects the first secret in `value`.
///
/// Returns `(class, byte_offset)` of the first match, or `None` when no secret
/// is found. Detection is pattern-based per `docs/schema/redaction.md §Secret
/// Classes`. Patterns are checked most-specific-first; the first match wins.
///
/// Code-graph records should not be scanned via this function; [`validate_record`]
/// already exempts them before calling detection.
#[must_use]
pub fn detect_secret(value: &str) -> Option<(SecretClass, usize)> {
    find_ssh_private_key(value)
        .map(|p| (SecretClass::SshPrivateKey, p))
        .or_else(|| find_database_url(value).map(|p| (SecretClass::DatabaseUrl, p)))
        .or_else(|| find_cloud_credential(value).map(|p| (SecretClass::CloudCredential, p)))
        .or_else(|| find_webhook_secret(value).map(|p| (SecretClass::WebhookSecret, p)))
        .or_else(|| find_session_cookie(value).map(|p| (SecretClass::SessionCookie, p)))
        .or_else(|| find_api_token(value).map(|p| (SecretClass::ApiToken, p)))
        .or_else(|| find_env_secret(value).map(|p| (SecretClass::EnvSecret, p)))
}

/// Returns `true` if `value` already carries a redaction marker.
///
/// A marker has the form `<REDACTED:class:hash_prefix>`. Already-redacted
/// values pass [`validate_record`] without triggering a
/// [`CodegraphError::RedactionRequired`] error.
#[must_use]
pub fn is_redacted(value: &str) -> bool {
    value.contains("<REDACTED:")
}

// ── Redaction ──────────────────────────────────────────────────────────────────

/// Applies the default v1 redaction policy to `value`.
///
/// When a secret is detected the **entire value** is replaced with:
///
/// ```text
/// <REDACTED:class:hash_prefix>
/// ```
///
/// `hash_prefix` is the first [`HASH_PREFIX_LEN`] hex characters of the BLAKE3
/// hash of the original value. It is safe to store and allows audit correlation
/// without revealing the secret.
///
/// When no secret is detected the value is returned unchanged.
///
/// This function is the canonical redaction closure used by
/// [`crate::traj::ImportOptions::default`] and
/// [`crate::codex::ImportOptions::default`].
#[must_use]
pub fn redact_value(value: &str) -> String {
    let Some((class, _)) = detect_secret(value) else {
        return value.to_owned();
    };
    let hash = blake3::hash(value.as_bytes());
    let hex = hash.to_hex();
    let prefix = &hex.as_str()[..HASH_PREFIX_LEN];
    format!("<REDACTED:{}:{}>", class.as_str(), prefix)
}

// ── Validation ─────────────────────────────────────────────────────────────────

/// Validates a graph record for unredacted sensitive fields.
///
/// Returns `Ok(())` when the record is safe to persist.
/// Returns `Err(CodegraphError::RedactionRequired)` when a sensitive field
/// contains an unredacted raw secret. The error names the field path but **never
/// echoes the raw secret value**.
///
/// **Code-graph records** (`Repository`, `File`, `Symbol`, etc.) always return
/// `Ok(())` — they are explicitly outside the redaction gate per
/// `docs/schema/redaction.md`.
///
/// **Edge and Tombstone records** carry no sensitive agent-authored fields and
/// always return `Ok(())`.
///
/// # Errors
///
/// Returns `Err(CodegraphError::RedactionRequired)` when a sensitive field on a
/// non-code-graph node contains an unredacted secret.
pub fn validate_record(record: &GraphRecord) -> Result<()> {
    let GraphRecord::Node { kind, domain, .. } = record else {
        return Ok(());
    };
    // `NodeKind::Diagnostic` is reused by agent-memory importers (traj, codex) which
    // set a non-None `domain`.  Only code-graph diagnostics (domain == None) are exempt.
    if is_code_graph_kind(*kind) && (*kind != NodeKind::Diagnostic || domain.is_none()) {
        return Ok(());
    }
    check_sensitive_fields(record)
}

/// Returns `true` when `kind` belongs to the code-graph domain.
///
/// Code-graph records are explicitly exempt from the redaction gate per
/// `docs/schema/redaction.md`.
const fn is_code_graph_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Repository
            | NodeKind::File
            | NodeKind::Module
            | NodeKind::Symbol
            | NodeKind::Import
            | NodeKind::Diagnostic
            | NodeKind::Commit
            | NodeKind::Change
            | NodeKind::SemanticDrift
            | NodeKind::EmbeddingModel
            | NodeKind::EmbeddingVector
    )
}

/// Checks all sensitive fields on a non-code-graph node.
#[allow(clippy::too_many_lines)]
fn check_sensitive_fields(record: &GraphRecord) -> Result<()> {
    let GraphRecord::Node {
        text,
        validation_summary,
        arguments_summary,
        arguments_handle,
        result_handle,
        stdout_handle,
        stderr_handle,
        patch_handle,
        title,
        body_handle,
        diff_hunk_handle,
        assignees,
        labels,
        url,
        user_context,
        redaction_policy_version,
        ..
    } = record
    else {
        return Ok(());
    };

    // Pass 1: reject raw (unredacted) secrets.
    check_field("text", text.as_deref())?;
    check_field("validation_summary", validation_summary.as_deref())?;
    check_field("arguments_summary", arguments_summary.as_deref())?;
    if let Some(h) = arguments_handle {
        check_field("arguments_handle.inline", h.inline.as_deref())?;
    }
    if let Some(h) = result_handle {
        check_field("result_handle.inline", h.inline.as_deref())?;
    }
    if let Some(h) = patch_handle {
        check_field("patch_handle.inline", h.inline.as_deref())?;
    }
    if let Some(h) = stdout_handle {
        check_field("stdout_handle.inline", h.inline.as_deref())?;
    }
    if let Some(h) = stderr_handle {
        check_field("stderr_handle.inline", h.inline.as_deref())?;
    }
    check_field("title", title.as_deref())?;
    if let Some(h) = body_handle {
        check_field("body_handle.inline", h.inline.as_deref())?;
    }
    if let Some(h) = diff_hunk_handle {
        check_field("diff_hunk_handle.inline", h.inline.as_deref())?;
    }
    check_field("url", url.as_deref())?;
    if let Some(items) = assignees {
        for (i, item) in items.iter().enumerate() {
            check_field(&format!("assignees[{i}]"), Some(item.as_str()))?;
        }
    }
    if let Some(items) = labels {
        for (i, item) in items.iter().enumerate() {
            check_field(&format!("labels[{i}]"), Some(item.as_str()))?;
        }
    }
    check_field(
        "proposed_rule_text",
        user_context.proposed_rule_text.as_deref(),
    )?;
    check_field("prompt_text", user_context.prompt_text.as_deref())?;
    check_field(
        "decision_rationale",
        user_context.decision_rationale.as_deref(),
    )?;
    check_field("edited_rule_text", user_context.edited_rule_text.as_deref())?;
    check_field("rule_text", user_context.rule_text.as_deref())?;
    check_field("action_summary", user_context.action_summary.as_deref())?;
    check_field("constraint_text", user_context.constraint_text.as_deref())?;

    // Pass 2: if any sensitive field carries a redaction marker, the node must
    // also carry `redaction_policy_version` so auditors can trace the policy
    // that was applied.  Reject marker-bearing fields when the version is absent.
    if redaction_policy_version.is_none() {
        check_marker_field("text", text.as_deref())?;
        check_marker_field("validation_summary", validation_summary.as_deref())?;
        check_marker_field("arguments_summary", arguments_summary.as_deref())?;
        if let Some(h) = arguments_handle {
            check_marker_field("arguments_handle.inline", h.inline.as_deref())?;
        }
        if let Some(h) = result_handle {
            check_marker_field("result_handle.inline", h.inline.as_deref())?;
        }
        if let Some(h) = patch_handle {
            check_marker_field("patch_handle.inline", h.inline.as_deref())?;
        }
        if let Some(h) = stdout_handle {
            check_marker_field("stdout_handle.inline", h.inline.as_deref())?;
        }
        if let Some(h) = stderr_handle {
            check_marker_field("stderr_handle.inline", h.inline.as_deref())?;
        }
        check_marker_field("title", title.as_deref())?;
        if let Some(h) = body_handle {
            check_marker_field("body_handle.inline", h.inline.as_deref())?;
        }
        if let Some(h) = diff_hunk_handle {
            check_marker_field("diff_hunk_handle.inline", h.inline.as_deref())?;
        }
        check_marker_field("url", url.as_deref())?;
        if let Some(items) = assignees {
            for (i, item) in items.iter().enumerate() {
                check_marker_field(&format!("assignees[{i}]"), Some(item.as_str()))?;
            }
        }
        if let Some(items) = labels {
            for (i, item) in items.iter().enumerate() {
                check_marker_field(&format!("labels[{i}]"), Some(item.as_str()))?;
            }
        }
        check_marker_field(
            "proposed_rule_text",
            user_context.proposed_rule_text.as_deref(),
        )?;
        check_marker_field("prompt_text", user_context.prompt_text.as_deref())?;
        check_marker_field(
            "decision_rationale",
            user_context.decision_rationale.as_deref(),
        )?;
        check_marker_field("edited_rule_text", user_context.edited_rule_text.as_deref())?;
        check_marker_field("rule_text", user_context.rule_text.as_deref())?;
        check_marker_field("action_summary", user_context.action_summary.as_deref())?;
        check_marker_field("constraint_text", user_context.constraint_text.as_deref())?;
    }

    Ok(())
}

/// Checks a single optional field value for unredacted secrets.
fn check_field(field_path: &str, value: Option<&str>) -> Result<()> {
    let Some(v) = value else { return Ok(()) };
    if detect_secret(v).is_some() {
        return Err(CodegraphError::RedactionRequired {
            field_path: field_path.to_owned(),
        });
    }
    Ok(())
}

/// Rejects a field that carries a redaction marker when the node has no policy version.
fn check_marker_field(field_path: &str, value: Option<&str>) -> Result<()> {
    let Some(v) = value else { return Ok(()) };
    if is_redacted(v) {
        return Err(CodegraphError::RedactionMetadataMissing {
            field_path: field_path.to_owned(),
        });
    }
    Ok(())
}

// ── Pattern matchers ───────────────────────────────────────────────────────────

fn find_ssh_private_key(value: &str) -> Option<usize> {
    const MARKERS: &[&str] = &[
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ];
    MARKERS.iter().find_map(|m| value.find(m))
}

fn find_database_url(value: &str) -> Option<usize> {
    const SCHEMES: &[&str] = &[
        "postgres://",
        "postgresql://",
        "mysql://",
        "mongodb://",
        "mongodb+srv://",
        "redis://",
        "mssql://",
    ];
    // Case-insensitive matching: `to_ascii_lowercase` preserves byte length for ASCII,
    // so positions in `lower` are identical to positions in `value`.
    let lower = value.to_ascii_lowercase();
    for scheme in SCHEMES {
        // Scan ALL occurrences so a non-credentialed decoy (e.g. `postgres://readonly@host/db`)
        // does not shadow a later credentialed URL with the same scheme.
        let mut search_from = 0_usize;
        while let Some(rel) = lower[search_from..].find(scheme) {
            let abs = search_from + rel;
            // Use the original value for credential extraction so we preserve the original casing.
            let after = &value[abs + scheme.len()..];
            // Limit credential search to the URL authority (everything before the first path,
            // query, fragment, or whitespace delimiter) to avoid matching an `@` that appears
            // in unrelated prose after the URL.
            let authority_end = after
                .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#'))
                .unwrap_or(after.len());
            let authority = &after[..authority_end];
            // Require user:password@host — @ must follow a colon-separated pair.
            if let Some(at) = authority.find('@') {
                let before_at = &authority[..at];
                if let Some(colon) = before_at.find(':') {
                    // Password (after colon) must be non-empty; username may be empty
                    // to cover password-only URLs like `redis://:p4ssw0rd@host`.
                    if colon + 1 < before_at.len() {
                        return Some(abs);
                    }
                }
            }
            search_from = abs + 1;
            if search_from >= value.len() {
                break;
            }
        }
    }
    None
}

fn find_cloud_credential(value: &str) -> Option<usize> {
    const PREFIXES: &[&str] = &["AKIA", "ASIA"];
    for prefix in PREFIXES {
        let mut haystack = value;
        let mut base = 0_usize;
        while let Some(rel) = haystack.find(prefix) {
            let abs = base + rel;
            let tail = &value[abs + prefix.len()..];
            // Exactly 16 uppercase alphanumeric chars must follow.
            let key16: String = tail.chars().take(16).collect();
            if key16.len() == 16
                && key16
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            {
                let end = abs + prefix.len() + 16;
                let next = value[end..].chars().next();
                let word_end = next.is_none_or(|c| !c.is_ascii_alphanumeric());
                let word_start =
                    abs == 0 || !value[..abs].ends_with(|c: char| c.is_ascii_alphanumeric());
                if word_start && word_end {
                    return Some(abs);
                }
            }
            base = abs + 1;
            if base >= value.len() {
                break;
            }
            haystack = &value[base..];
        }
    }
    None
}

fn find_webhook_secret(value: &str) -> Option<usize> {
    // Scan ALL occurrences so a short non-secret example (e.g. `whsec_test`)
    // does not shadow a later real signing secret.
    let mut search_from = 0_usize;
    while let Some(rel) = value[search_from..].find("whsec_") {
        let abs = search_from + rel;
        let after = &value[abs + 6..];
        let len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .count();
        if len >= 20 {
            return Some(abs);
        }
        search_from = abs + 1;
        if search_from >= value.len() {
            break;
        }
    }
    None
}

fn find_session_cookie(value: &str) -> Option<usize> {
    // Named session cookie prefixes (declared before any statements per Clippy).
    const SESSION_PREFIXES: &[&str] = &["sessionid=", "session=", "sid=", "connect.sid="];

    // JWT tokens always begin with eyJ (base64url of `{"`).
    // Scan ALL occurrences so a short mention ("JWTs start with eyJ") does not
    // shadow a real token that follows.
    let mut search_from = 0_usize;
    while let Some(rel) = value[search_from..].find("eyJ") {
        let abs = search_from + rel;
        let after = &value[abs + 3..];
        let len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/'))
            .count();
        if len >= 20 {
            return Some(abs);
        }
        search_from = abs + 1;
        if search_from >= value.len() {
            break;
        }
    }

    // Named session cookie prefixes — scan ALL occurrences per prefix so a short
    // decoy value ("session=test") does not block detection of the real cookie.
    for prefix in SESSION_PREFIXES {
        let mut search_from = 0_usize;
        while let Some(rel) = value[search_from..].find(prefix) {
            let abs = search_from + rel;
            let after = &value[abs + prefix.len()..];
            let len = after
                .chars()
                .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | ','))
                .count();
            if len >= 8 {
                return Some(abs);
            }
            search_from = abs + 1;
            if search_from >= value.len() {
                break;
            }
        }
    }
    None
}

fn find_api_token(value: &str) -> Option<usize> {
    // Well-known API token prefixes
    const PREFIXES: &[&str] = &[
        "sk-",         // OpenAI and compatible providers
        "sk_",         // Stripe live/test secret keys
        "rk_",         // Stripe live/test restricted keys
        "ghp_",        // GitHub personal access token (classic)
        "ghs_",        // GitHub server-to-server token
        "github_pat_", // GitHub fine-grained PAT
        "glpat-",      // GitLab personal access token
        "xoxb-",       // Slack bot token
        "xoxp-",       // Slack user token
    ];
    for prefix in PREFIXES {
        if let Some(pos) = value.find(prefix) {
            let after = &value[pos + prefix.len()..];
            let len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .count();
            if len >= 20 {
                return Some(pos);
            }
        }
    }
    // Bearer token (HTTP Authorization header) — matched case-insensitively.
    // Scan ALL occurrences so a short example ("Bearer test") does not shadow
    // a real token that appears later in the same field.
    let lower = value.to_ascii_lowercase();
    let mut search_from = 0_usize;
    while let Some(rel) = lower[search_from..].find("bearer ") {
        let abs = search_from + rel;
        let after = &value[abs + 7..];
        let len = after.chars().take_while(|c| !c.is_whitespace()).count();
        if len >= 20 {
            return Some(abs);
        }
        search_from = abs + 1;
        if search_from >= lower.len() {
            break;
        }
    }
    None
}

fn find_env_secret(value: &str) -> Option<usize> {
    // Secret-name allowlist per docs/schema/redaction.md.
    const KEYWORDS: &[&str] = &[
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASS",
        "PWD",
        "PRIVATE_KEY",
        "ACCESS_KEY",
        "API_KEY",
        "AUTH",
        "COOKIE",
        "DATABASE_URL",
        "DB_URL",
        "WEBHOOK_SECRET",
    ];

    for (eq_pos, _) in value.char_indices().filter(|(_, c)| *c == '=') {
        let key_start = env_key_start(value, eq_pos);
        let key = &value[key_start..eq_pos];

        // The key must be non-empty and consist only of uppercase letters, digits,
        // and underscores (standard env-var naming).
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        {
            continue;
        }

        if !KEYWORDS.iter().any(|kw| key.contains(kw)) {
            continue;
        }

        let val_start = eq_pos + 1;
        if val_start >= value.len() {
            continue; // Empty value — not a secret
        }
        let remaining = &value[val_start..];

        // Already redacted — skip.
        if remaining.starts_with("<REDACTED:") {
            continue;
        }

        // Value must be ≥8 non-whitespace chars with no newline/semicolon break.
        let val_len = remaining
            .chars()
            .take_while(|c| !matches!(c, '\n' | '\r' | ';' | ' ' | '\t'))
            .count();

        if val_len >= 8 {
            return Some(key_start);
        }
    }

    None
}

/// Scans backwards from `eq_pos` to find the start byte of the env key.
fn env_key_start(value: &str, eq_pos: usize) -> usize {
    value[..eq_pos]
        .rfind(|c: char| !c.is_ascii_uppercase() && !c.is_ascii_digit() && c != '_')
        .map_or(0, |p| p + 1)
}
