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
//! - [`parse_redaction_markers`]: extract embedded `<REDACTED:...>` markers.
//! - [`sensitive_fields`]: enumerate present sensitive fields on a record.
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

/// Placeholder written in place of a detected secret in code-graph text fields.
///
/// Exposed so callers (e.g. the incremental refresh path) can detect whether an
/// assembled graph still carries masked literals without re-running detection.
pub const REDACTION_MARKER: &str = "«redacted:secret»";

/// Number of hex characters taken from the BLAKE3 hash as the audit prefix.
///
/// 12 hex digits = 6 bytes = 48 bits of correlation space. Not reversible to
/// the secret but sufficient for audit cross-referencing.
const HASH_PREFIX_LEN: usize = 12;

// ── Secret classes ─────────────────────────────────────────────────────────────

/// Named secret classes defined in `docs/schema/redaction.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    /// Email addresses (PII).
    Email,
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
            Self::Email => "email",
        }
    }

    /// Resolves a canonical class name back to its [`SecretClass`].
    ///
    /// Returns `None` for any name outside the closed class set defined in
    /// `docs/schema/redaction.md §Secret Classes`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "api_token" => Some(Self::ApiToken),
            "ssh_private_key" => Some(Self::SshPrivateKey),
            "database_url" => Some(Self::DatabaseUrl),
            "cloud_credential" => Some(Self::CloudCredential),
            "webhook_secret" => Some(Self::WebhookSecret),
            "session_cookie" => Some(Self::SessionCookie),
            "env_secret" => Some(Self::EnvSecret),
            "email" => Some(Self::Email),
            _ => None,
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
        .or_else(|| find_email(value).map(|p| (SecretClass::Email, p)))
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

/// Parses every well-formed redaction marker embedded in `value`.
///
/// A well-formed marker is `<REDACTED:secret_class:hash_prefix>` where
/// `secret_class` is one of the named classes in `docs/schema/redaction.md`
/// and `hash_prefix` is a lowercase-hex BLAKE3 prefix of exactly
/// [`HASH_PREFIX_LEN`] characters — the length [`redact_value`] always emits.
/// Marker-shaped substrings with an unknown class, a non-hex hash, or a
/// wrong-length hash are ignored, so a placeholder merely mentioned in a
/// transcript cannot be counted as a redaction. Returns the
/// `(class, hash_prefix)` pairs in order of appearance.
///
/// Used by the at-import redaction report (issue #266) to tie report entries
/// to the markers actually stored on emitted records.
#[must_use]
pub fn parse_redaction_markers(value: &str) -> Vec<(SecretClass, String)> {
    const PREFIX: &str = "<REDACTED:";
    let mut markers = Vec::new();
    let mut search_from = 0_usize;
    while let Some(rel) = value[search_from..].find(PREFIX) {
        let start = search_from + rel;
        let body = &value[start + PREFIX.len()..];
        let Some(end) = body.find('>') else { break };
        let inner = &body[..end];
        if let Some((class_name, hash_prefix)) = inner.split_once(':')
            && let Some(class) = SecretClass::from_name(class_name)
            && hash_prefix.len() == HASH_PREFIX_LEN
            && hash_prefix
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        {
            markers.push((class, hash_prefix.to_owned()));
            search_from = start + PREFIX.len() + end + 1;
        } else {
            search_from = start + PREFIX.len();
        }
    }
    markers
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

/// Builds the `<REDACTED:class:hash_prefix>` marker for an already-located secret
/// span whose [`SecretClass`] the caller obtained from [`detect_secret_span`].
///
/// [`redact_value`] re-detects the class from the value it is given, which is
/// wrong for a caller that has isolated the exact detected span: some classes are
/// only recognizable with surrounding context the span omits (an [`SecretClass::EnvSecret`]
/// span covers only the value bytes, not the `KEY=` that identifies it, so
/// re-detection over the bare value returns `None` and the secret would pass
/// through unredacted). This helper collapses the exact secret slice to one
/// marker using the class the span detector already reported, with the same
/// BLAKE3 hash-prefix scheme [`redact_value`] emits. Byte-identical across runs.
#[must_use]
pub fn redact_span(class: SecretClass, secret_slice: &str) -> String {
    let hash = blake3::hash(secret_slice.as_bytes());
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
pub(crate) const fn is_code_graph_kind(kind: NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::Repository
            | NodeKind::File
            | NodeKind::Module
            | NodeKind::Symbol
            | NodeKind::Import
            | NodeKind::Diagnostic
            | NodeKind::PanicRiskSite
            | NodeKind::DebtMarker
            | NodeKind::UnsafeSite
            | NodeKind::Commit
            | NodeKind::Change
            | NodeKind::SemanticDrift
            | NodeKind::EmbeddingModel
            | NodeKind::EmbeddingVector
    )
}

/// Enumerates the present sensitive field `(field_path, value)` pairs on a
/// node record, in canonical field order.
///
/// This is the single source of truth for the sensitive-field index documented
/// in `docs/cli/redaction.md §Sensitive field index`. Absent fields are never
/// enumerated, so a caller can rely on the absent-vs-present distinction.
/// Edge and Tombstone records enumerate no fields.
///
/// Note: enumeration does **not** apply the code-graph exemption —
/// [`validate_record`] exempts code-graph kinds before checking, and the
/// redaction report (issue #266) deliberately scans every emitted record.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn sensitive_fields(record: &GraphRecord) -> Vec<(String, &str)> {
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
        ..
    } = record
    else {
        return Vec::new();
    };

    let mut fields: Vec<(String, &str)> = Vec::new();
    add(&mut fields, "text", text.as_deref());
    add(
        &mut fields,
        "validation_summary",
        validation_summary.as_deref(),
    );
    add(
        &mut fields,
        "arguments_summary",
        arguments_summary.as_deref(),
    );
    if let Some(h) = arguments_handle {
        add(&mut fields, "arguments_handle.inline", h.inline.as_deref());
    }
    if let Some(h) = result_handle {
        add(&mut fields, "result_handle.inline", h.inline.as_deref());
    }
    if let Some(h) = patch_handle {
        add(&mut fields, "patch_handle.inline", h.inline.as_deref());
    }
    if let Some(h) = stdout_handle {
        add(&mut fields, "stdout_handle.inline", h.inline.as_deref());
    }
    if let Some(h) = stderr_handle {
        add(&mut fields, "stderr_handle.inline", h.inline.as_deref());
    }
    add(&mut fields, "title", title.as_deref());
    if let Some(h) = body_handle {
        add(&mut fields, "body_handle.inline", h.inline.as_deref());
    }
    if let Some(h) = diff_hunk_handle {
        add(&mut fields, "diff_hunk_handle.inline", h.inline.as_deref());
    }
    add(&mut fields, "url", url.as_deref());
    if let Some(items) = assignees {
        for (i, item) in items.iter().enumerate() {
            add(&mut fields, &format!("assignees[{i}]"), Some(item.as_str()));
        }
    }
    if let Some(items) = labels {
        for (i, item) in items.iter().enumerate() {
            add(&mut fields, &format!("labels[{i}]"), Some(item.as_str()));
        }
    }
    add(
        &mut fields,
        "proposed_rule_text",
        user_context.proposed_rule_text.as_deref(),
    );
    add(
        &mut fields,
        "prompt_text",
        user_context.prompt_text.as_deref(),
    );
    add(
        &mut fields,
        "decision_rationale",
        user_context.decision_rationale.as_deref(),
    );
    add(
        &mut fields,
        "edited_rule_text",
        user_context.edited_rule_text.as_deref(),
    );
    add(&mut fields, "rule_text", user_context.rule_text.as_deref());
    add(
        &mut fields,
        "action_summary",
        user_context.action_summary.as_deref(),
    );
    add(
        &mut fields,
        "constraint_text",
        user_context.constraint_text.as_deref(),
    );

    fields
}

/// Appends a `(field_path, value)` pair when the field is present.
fn add<'a>(fields: &mut Vec<(String, &'a str)>, path: &str, value: Option<&'a str>) {
    if let Some(v) = value {
        fields.push((path.to_owned(), v));
    }
}

/// Checks all sensitive fields on a non-code-graph node.
fn check_sensitive_fields(record: &GraphRecord) -> Result<()> {
    let fields = sensitive_fields(record);

    // Pass 1: reject raw (unredacted) secrets.
    for (field_path, value) in &fields {
        check_field(field_path, Some(value))?;
    }

    // Pass 2: if any sensitive field carries a redaction marker, the node must
    // also carry `redaction_policy_version` so auditors can trace the policy
    // that was applied.  Reject marker-bearing fields when the version is absent.
    let GraphRecord::Node {
        redaction_policy_version,
        ..
    } = record
    else {
        return Ok(());
    };
    if redaction_policy_version.is_none() {
        for (field_path, value) in &fields {
            check_marker_field(field_path, Some(value))?;
        }
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
                .find(|c: char| c.is_whitespace() || matches!(c, ';' | ','))
                .unwrap_or(after.len());
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
        let mut search_from = 0_usize;
        while let Some(rel) = value[search_from..].find(prefix) {
            let abs = search_from + rel;
            let after = &value[abs + prefix.len()..];
            let len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .count();
            if len >= 20 {
                return Some(abs);
            }
            search_from = abs + prefix.len();
            if search_from >= value.len() {
                break;
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
        let len = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
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
        if remaining.starts_with("<REDACTED:") || remaining.starts_with("«redacted:secret»") {
            continue;
        }

        // Value must be ≥8 non-whitespace chars with no newline/semicolon break.
        let val_len = remaining
            .find(['\n', '\r', ';', ' ', '\t'])
            .unwrap_or(remaining.len());

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
        .map_or(0, |p| {
            let ch = value[p..].chars().next().unwrap_or(' ');
            p + ch.len_utf8()
        })
}

fn find_email(value: &str) -> Option<usize> {
    const BLOCKED_EXTENSIONS: &[&str] = &[
        "js", "go", "ts", "cpp", "rb", "json", "yaml", "yml", "toml", "txt", "html", "css", "bat",
        "lock", "class",
    ];

    for (idx, c) in value.char_indices() {
        if c == '@' {
            let before = &value[..idx];
            let username_len = before
                .chars()
                .rev()
                .take_while(|&ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '%' | '+' | '-')
                })
                .count();
            if username_len == 0 {
                continue;
            }

            // Context-aware check: reject if username is preceded by a path separator
            let start_idx = idx - username_len;
            if start_idx > 0 {
                let prev_char = value[..start_idx].chars().next_back();
                if prev_char == Some('/') || prev_char == Some('\\') {
                    continue;
                }
            }

            // Parse domain walking forward
            let after = &value[idx + 1..];
            let mut domain_len = 0;
            for ch in after.chars() {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                    domain_len += ch.len_utf8();
                } else {
                    break;
                }
            }

            // Context-aware check: reject if domain is followed by a path separator
            let next_char = after[domain_len..].chars().next();
            if next_char == Some('/') || next_char == Some('\\') {
                continue;
            }
            if next_char == Some(':') {
                let after_colon = after[domain_len + 1..].chars().next();
                if after_colon.is_some_and(|ch| !ch.is_whitespace()) {
                    continue;
                }
            }

            let mut domain_str = &after[..domain_len];
            while domain_str.ends_with('.') || domain_str.ends_with('-') {
                domain_str = &domain_str[..domain_str.len() - 1];
            }
            let labels: Vec<&str> = domain_str.split('.').collect();

            if labels.len() >= 2 && labels.iter().all(|l| !l.is_empty()) {
                let last_label = labels.last().unwrap();
                let is_punycode = last_label.to_lowercase().starts_with("xn--")
                    && last_label.len() >= 6
                    && last_label[4..]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-');
                let is_alphabetic =
                    last_label.len() >= 2 && last_label.chars().all(|c| c.is_ascii_alphabetic());

                if is_alphabetic || is_punycode {
                    let tld_lower = last_label.to_lowercase();
                    if !BLOCKED_EXTENSIONS.contains(&tld_lower.as_str()) {
                        return Some(start_idx);
                    }
                }
            }
        }
    }
    None
}

// ── Span Matchers ─────────────────────────────────────────────────────────────

fn find_ssh_private_key_span(value: &str) -> Option<(usize, usize)> {
    const MARKERS: &[&str] = &[
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN ENCRYPTED PRIVATE KEY-----",
    ];
    const END_MARKERS: &[&str] = &[
        "-----END RSA PRIVATE KEY-----",
        "-----END OPENSSH PRIVATE KEY-----",
        "-----END EC PRIVATE KEY-----",
        "-----END PRIVATE KEY-----",
        "-----END ENCRYPTED PRIVATE KEY-----",
    ];
    for (i, m) in MARKERS.iter().enumerate() {
        if let Some(start) = value.find(m) {
            let end_m = END_MARKERS[i];
            if let Some(end_pos) = value[start..].find(end_m) {
                let end = start + end_pos + end_m.len();
                return Some((start, end - start));
            }
            let len = value.len() - start;
            return Some((start, len));
        }
    }
    None
}

fn find_database_url_span(value: &str) -> Option<(usize, usize)> {
    const SCHEMES: &[&str] = &[
        "postgres://",
        "postgresql://",
        "mysql://",
        "mongodb://",
        "mongodb+srv://",
        "redis://",
        "mssql://",
    ];
    let lower = value.to_ascii_lowercase();
    for scheme in SCHEMES {
        let mut search_from = 0_usize;
        while let Some(rel) = lower[search_from..].find(scheme) {
            let abs = search_from + rel;
            let after = &value[abs + scheme.len()..];
            let authority_end = after
                .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#'))
                .unwrap_or(after.len());
            let authority = &after[..authority_end];
            if let Some(at) = authority.find('@') {
                let before_at = &authority[..at];
                if let Some(colon) = before_at.find(':')
                    && colon + 1 < before_at.len()
                {
                    let url_len = value[abs..]
                        .find(|c: char| {
                            c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | ',' | ';')
                        })
                        .unwrap_or_else(|| value[abs..].len());
                    return Some((abs, url_len));
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

fn find_cloud_credential_span(value: &str) -> Option<(usize, usize)> {
    const PREFIXES: &[&str] = &["AKIA", "ASIA"];
    for prefix in PREFIXES {
        let mut haystack = value;
        let mut base = 0_usize;
        while let Some(rel) = haystack.find(prefix) {
            let abs = base + rel;
            let tail = &value[abs + prefix.len()..];
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
                    return Some((abs, prefix.len() + 16));
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

fn find_webhook_secret_span(value: &str) -> Option<(usize, usize)> {
    let mut search_from = 0_usize;
    while let Some(rel) = value[search_from..].find("whsec_") {
        let abs = search_from + rel;
        let after = &value[abs + 6..];
        let len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .count();
        if len >= 20 {
            return Some((abs, 6 + len));
        }
        search_from = abs + 1;
        if search_from >= value.len() {
            break;
        }
    }
    None
}

fn find_session_cookie_span(value: &str) -> Option<(usize, usize)> {
    const SESSION_PREFIXES: &[&str] = &["sessionid=", "session=", "sid=", "connect.sid="];
    let mut search_from = 0_usize;
    while let Some(rel) = value[search_from..].find("eyJ") {
        let abs = search_from + rel;
        let after = &value[abs + 3..];
        let len = after
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/'))
            .count();
        if len >= 20 {
            return Some((abs, 3 + len));
        }
        search_from = abs + 1;
        if search_from >= value.len() {
            break;
        }
    }
    for prefix in SESSION_PREFIXES {
        let mut search_from = 0_usize;
        while let Some(rel) = value[search_from..].find(prefix) {
            let abs = search_from + rel;
            let after = &value[abs + prefix.len()..];
            let len = after
                .find(|c: char| c.is_whitespace() || matches!(c, ';' | ',' | '"' | '\''))
                .unwrap_or(after.len());
            if len >= 8 {
                return Some((abs, prefix.len() + len));
            }
            search_from = abs + 1;
            if search_from >= value.len() {
                break;
            }
        }
    }
    None
}

fn find_api_token_span(value: &str) -> Option<(usize, usize)> {
    const PREFIXES: &[&str] = &[
        "sk-",
        "sk_",
        "rk_",
        "ghp_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
    ];
    for prefix in PREFIXES {
        let mut search_from = 0_usize;
        while let Some(rel) = value[search_from..].find(prefix) {
            let abs = search_from + rel;
            let after = &value[abs + prefix.len()..];
            let len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
                .count();
            if len >= 20 {
                return Some((abs, prefix.len() + len));
            }
            search_from = abs + prefix.len();
            if search_from >= value.len() {
                break;
            }
        }
    }
    let lower = value.to_ascii_lowercase();
    let mut search_from = 0_usize;
    while let Some(rel) = lower[search_from..].find("bearer ") {
        let abs = search_from + rel;
        let after = &value[abs + 7..];
        let len = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\''))
            .unwrap_or(after.len());
        if len >= 20 {
            return Some((abs, 7 + len));
        }
        search_from = abs + 1;
        if search_from >= lower.len() {
            break;
        }
    }
    None
}

fn find_email_span(value: &str) -> Option<(usize, usize)> {
    const BLOCKED_EXTENSIONS: &[&str] = &[
        "js", "go", "ts", "cpp", "rb", "json", "yaml", "yml", "toml", "txt", "html", "css", "bat",
        "lock", "class",
    ];
    for (idx, c) in value.char_indices() {
        if c == '@' {
            let before = &value[..idx];
            let username_len = before
                .chars()
                .rev()
                .take_while(|&ch| {
                    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '%' | '+' | '-')
                })
                .count();
            if username_len == 0 {
                continue;
            }
            let start_idx = idx - username_len;
            if start_idx > 0 {
                let prev_char = value[..start_idx].chars().next_back();
                if prev_char == Some('/') || prev_char == Some('\\') {
                    continue;
                }
            }
            let after = &value[idx + 1..];
            let mut domain_len = 0;
            for ch in after.chars() {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                    domain_len += ch.len_utf8();
                } else {
                    break;
                }
            }
            let next_char = after[domain_len..].chars().next();
            if next_char == Some('/') || next_char == Some('\\') {
                continue;
            }
            if next_char == Some(':') {
                let after_colon = after[domain_len + 1..].chars().next();
                if after_colon.is_some_and(|ch| !ch.is_whitespace()) {
                    continue;
                }
            }
            let mut domain_str = &after[..domain_len];
            while domain_str.ends_with('.') || domain_str.ends_with('-') {
                domain_str = &domain_str[..domain_str.len() - 1];
            }
            let labels: Vec<&str> = domain_str.split('.').collect();
            if labels.len() >= 2 && labels.iter().all(|l| !l.is_empty()) {
                let last_label = labels.last().unwrap();
                let is_punycode = last_label.to_lowercase().starts_with("xn--")
                    && last_label.len() >= 6
                    && last_label[4..]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-');
                let is_alphabetic =
                    last_label.len() >= 2 && last_label.chars().all(|c| c.is_ascii_alphabetic());
                if is_alphabetic || is_punycode {
                    let tld_lower = last_label.to_lowercase();
                    if !BLOCKED_EXTENSIONS.contains(&tld_lower.as_str()) {
                        let total_len = idx + 1 + domain_str.len() - start_idx;
                        return Some((start_idx, total_len));
                    }
                }
            }
        }
    }
    None
}

/// Returns true when `token` is EXACTLY one redaction placeholder and nothing
/// else — either the `«redacted:secret»` [`REDACTION_MARKER`] or a well-formed
/// `<REDACTED:…>` marker (matching `^<REDACTED:[^>]*>$`) with no bytes after the
/// closing `>`. The env-secret span matcher uses this to skip a value that is
/// ALREADY fully redacted, while still redacting a value that merely BEGINS with a
/// placeholder and hides a secret tail after it (e.g. `<REDACTED:secret>!tail`,
/// produced when `redact_code_text` redacts a leading higher-priority API token
/// first). A prefix-only `starts_with("<REDACTED:")` guard leaked that tail
/// (Codex round-9 P1).
fn is_exact_placeholder(token: &str) -> bool {
    if token == REDACTION_MARKER {
        return true;
    }
    token
        .strip_prefix("<REDACTED:")
        .is_some_and(|rest| rest.ends_with('>') && !rest[..rest.len() - 1].contains('>'))
}

fn find_env_secret_span(value: &str) -> Option<(usize, usize)> {
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
    // Delimiters that terminate an UNQUOTED env value (and the fallback boundary for
    // an unbalanced quote).
    const DELIMS: [char; 5] = ['\n', '\r', ';', ' ', '\t'];
    for (eq_pos, _) in value.char_indices().filter(|(_, c)| *c == '=') {
        let key_start = env_key_start(value, eq_pos);
        let key = &value[key_start..eq_pos];
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
            continue;
        }
        let remaining = &value[val_start..];
        // Determine where the value token ends. For a QUOTED value the whole quoted
        // run — internal whitespace and other delimiters included — is the value, so
        // the boundary is the matching CLOSING quote, not the first delimiter. A
        // delimiter-only boundary truncated `PASSWORD="correct horse battery staple"`
        // at the first space, redacting only `correct`; the next pass then read the
        // `"<REDACTED:secret>` head as an already-redacted exact placeholder and left
        // `horse battery staple"` in the #321 protected-capture blob (Codex round-10
        // P1). If the opening quote has no matching close on the line, fall back to
        // the delimiter boundary so a stray/unbalanced quote can't swallow the rest
        // of the line. For an UNQUOTED value the delimiter set (newline, CR, `;`,
        // space, tab) terminates the token exactly as before — unquoted handling is
        // unchanged, so a space still ends `PASSWORD=secretvalue nextword`.
        let delim_boundary = || remaining.find(DELIMS).unwrap_or(remaining.len());
        let opening_quote = remaining.chars().next().filter(|&c| c == '"' || c == '\'');
        let val_len = opening_quote.map_or_else(delim_boundary, |quote| {
            remaining[quote.len_utf8()..]
                .find(quote)
                .map_or_else(delim_boundary, |rel_close| {
                    quote.len_utf8() + rel_close + quote.len_utf8()
                })
        });
        if val_len < 8 {
            continue;
        }
        // Strip a wrapping quote pair so the returned span covers the secret VALUE
        // bytes and leaves the structural quotes in place — the quote-as-delimiter
        // convention already used by the database-url and session-cookie span
        // matchers. The unquoted path is unchanged (span == the whole value token).
        let token = &remaining[..val_len];
        let (span_start, span_len) = opening_quote.map_or((val_start, val_len), |quote| {
            // Drop the leading quote; drop the trailing quote too only when the
            // token is actually closed within this value token.
            let closed = token.len() >= 2 && token.ends_with(quote);
            (val_start + 1, val_len - 1 - usize::from(closed))
        });
        if span_len == 0 {
            continue;
        }
        // Skip (treat as already-redacted) ONLY when the ENTIRE value token — after
        // stripping any wrapping quote pair — is EXACTLY one placeholder. A value
        // that merely BEGINS with a placeholder but carries secret bytes AFTER the
        // closing `>` (e.g. `PASSWORD=<REDACTED:secret>!tail`, produced when
        // `redact_code_text` redacts a leading higher-priority API token first) is
        // NOT already redacted: return a span covering the whole value so the
        // re-scan loop collapses token + tail into one placeholder. A prefix-only
        // guard (`remaining.starts_with("<REDACTED:")`) leaked that tail into the
        // #321 protected-capture blob (Codex round-9 P1). Returning the whole value
        // still terminates: each such pass either replaces real secret bytes or,
        // once the value is exactly a placeholder, hits this skip.
        if is_exact_placeholder(&value[span_start..span_start + span_len]) {
            continue;
        }
        return Some((span_start, span_len));
    }
    None
}

/// Detects the span (class, start byte offset, and length) of the first secret in `value`.
#[must_use]
pub fn detect_secret_span(value: &str) -> Option<(SecretClass, usize, usize)> {
    find_ssh_private_key_span(value)
        .map(|(start, len)| (SecretClass::SshPrivateKey, start, len))
        .or_else(|| {
            find_database_url_span(value).map(|(start, len)| (SecretClass::DatabaseUrl, start, len))
        })
        .or_else(|| {
            find_cloud_credential_span(value)
                .map(|(start, len)| (SecretClass::CloudCredential, start, len))
        })
        .or_else(|| {
            find_webhook_secret_span(value)
                .map(|(start, len)| (SecretClass::WebhookSecret, start, len))
        })
        .or_else(|| {
            find_session_cookie_span(value)
                .map(|(start, len)| (SecretClass::SessionCookie, start, len))
        })
        .or_else(|| {
            find_api_token_span(value).map(|(start, len)| (SecretClass::ApiToken, start, len))
        })
        .or_else(|| find_email_span(value).map(|(start, len)| (SecretClass::Email, start, len)))
        .or_else(|| {
            find_env_secret_span(value).map(|(start, len)| (SecretClass::EnvSecret, start, len))
        })
}

/// Redacts all secrets in `value` by replacing them with `placeholder`.
/// Returns the redacted string and a map of counts per secret class detected.
#[must_use]
pub fn redact_code_text(
    mut value: String,
    placeholder: &str,
) -> (String, std::collections::HashMap<SecretClass, usize>) {
    let mut counts = std::collections::HashMap::new();
    // Byte cursor into `value`. Advances only PAST an already-inserted placeholder
    // that was re-detected as a no-op match; a real replacement leaves it in place
    // so overlapping/lower-priority secrets that just became contiguous are caught.
    let mut scan_from = 0;
    while let Some((class, rel_start, len)) = detect_secret_span(&value[scan_from..]) {
        let start = scan_from + rel_start;
        let end = start + len;
        // Forward-progress guard: some span matchers (e.g. `find_env_secret_span`)
        // strip wrapping quotes and re-detect the placeholder they just wrote as
        // the env value on a later iteration — e.g. `KEY="<REDACTED:secret>"`.
        // Replacing that slice with the identical placeholder is a no-op, so
        // re-scanning from the same cursor would spin forever. When the matched
        // slice is already exactly the placeholder, SKIP past it and keep scanning
        // the rest of the text — do NOT `break`, or a second, independent secret
        // after an already-redacted quoted placeholder (e.g.
        // `API_KEY="a" PASSWORD=b`) would never be scanned and would survive into
        // the protected log payload and every other `redact_code_text` caller.
        if &value[start..end] == placeholder {
            scan_from = end;
            continue;
        }
        value.replace_range(start..end, placeholder);
        *counts.entry(class).or_insert(0) += 1;
        // Do NOT advance `scan_from` here: re-detect from the same cursor so an
        // overlapping lower-priority secret that just became contiguous (its span
        // now containing this placeholder) is caught and fully redacted. The next
        // iteration re-detects the placeholder as a no-op and advances the cursor,
        // so the loop still makes forward progress and always terminates.
    }
    (value, counts)
}

/// Redacts secrets in every source-bearing text field of a code-graph node.
///
/// Scrubs the `summary` plus the issue #124 `signature`/`doc` capture fields in
/// place — each embeds raw source and can therefore carry an inlined secret.
/// Returns the accumulated per-class literal counts across all fields (empty when
/// the node was clean).
#[must_use]
pub fn redact_node_text_fields(
    summary: &mut String,
    signature: &mut Option<String>,
    doc: &mut Option<String>,
    placeholder: &str,
) -> std::collections::HashMap<SecretClass, usize> {
    let mut counts: std::collections::HashMap<SecretClass, usize> =
        std::collections::HashMap::new();

    let (clean, summary_counts) = redact_code_text(std::mem::take(summary), placeholder);
    *summary = clean;
    for (class, count) in summary_counts {
        *counts.entry(class).or_insert(0) += count;
    }

    for text in [signature, doc].into_iter().flatten() {
        let (clean, field_counts) = redact_code_text(std::mem::take(text), placeholder);
        *text = clean;
        for (class, count) in field_counts {
            *counts.entry(class).or_insert(0) += count;
        }
    }

    counts
}

/// Post-processes and redacts all code-graph records in place, appending a
/// `Diagnostic` node recording the redaction evidence count/classes.
#[allow(clippy::too_many_lines)]
pub fn redact_code_graph(records: &mut Vec<GraphRecord>, raw_literals: bool, repository_id: &str) {
    use crate::ir::TemporalMetadata;
    use std::collections::{BTreeMap, HashMap};

    if raw_literals {
        return;
    }

    let existing_producer = records.iter().find_map(|r| match r {
        GraphRecord::Node { producer, .. }
        | GraphRecord::Edge { producer, .. }
        | GraphRecord::Tombstone { producer, .. } => producer.clone(),
    });

    let mut existing_valid_time = None;
    let mut existing_valid_time_source = None;
    for r in &*records {
        if let GraphRecord::Node {
            valid_time: Some(vt),
            valid_time_source,
            ..
        } = r
        {
            existing_valid_time = Some(vt.clone());
            existing_valid_time_source.clone_from(valid_time_source);
            break;
        }
    }

    // Ordered maps keyed on the temporal group so the per-commit diagnostics are
    // built in a deterministic order regardless of record traversal, preserving
    // the byte-identical scan contract across process runs (Codex C3). The final
    // `diags.sort_by(id)` still normalizes emission order, but the ordered map
    // also removes the residual same-id tie ambiguity a `HashMap` could expose.
    let mut group_counts: BTreeMap<Option<TemporalMetadata>, HashMap<SecretClass, usize>> =
        BTreeMap::new();
    let mut group_masked_nodes: BTreeMap<Option<TemporalMetadata>, usize> = BTreeMap::new();

    for record in records.iter_mut() {
        if let GraphRecord::Node {
            kind,
            temporal,
            summary,
            signature,
            doc,
            ..
        } = record
            && is_code_graph_kind(*kind)
        {
            let counts = redact_node_text_fields(summary, signature, doc, "«redacted:secret»");
            if !counts.is_empty() {
                let t_key = temporal.clone();
                *group_masked_nodes.entry(t_key.clone()).or_insert(0) += 1;
                let sub_map = group_counts.entry(t_key).or_default();
                for (class, count) in counts {
                    *sub_map.entry(class).or_insert(0) += count;
                }
            }
        }
    }

    let mut diags = Vec::new();
    for (temporal, counts) in group_counts {
        if counts.is_empty() {
            continue;
        }
        let total_nodes = group_masked_nodes.get(&temporal).copied().unwrap_or(0);
        let total_literals: usize = counts.values().sum();

        let mut class_details = counts
            .iter()
            .map(|(class, count)| format!("{}: {}", class.as_str(), count))
            .collect::<Vec<_>>();
        class_details.sort();
        let class_details_str = class_details.join(", ");

        let summary = format!(
            "Redacted {total_literals} literals across {total_nodes} nodes. Detector classes: {class_details_str}"
        );

        let commit_sha = temporal.as_ref().map(|t| t.git_commit.as_str());
        let diag_id = commit_sha.map_or_else(
            || crate::stable_id(&["node", "diagnostic", "redaction_evidence", repository_id]),
            |sha| {
                crate::stable_id(&[
                    "node",
                    "diagnostic",
                    "redaction_evidence",
                    repository_id,
                    sha,
                ])
            },
        );

        let mut diag = GraphRecord::node(
            diag_id,
            NodeKind::Diagnostic,
            None,
            None,
            Some("redaction_evidence".to_owned()),
            summary,
        );

        if let GraphRecord::Node {
            redaction_policy_version,
            ..
        } = &mut diag
        {
            *redaction_policy_version = Some(crate::redaction::REDACTION_POLICY_VERSION.to_owned());
        }

        if let Some(t) = temporal {
            diag = diag.with_temporal(t);
        } else if let GraphRecord::Node {
            valid_time,
            valid_time_source,
            ..
        } = &mut diag
        {
            valid_time.clone_from(&existing_valid_time);
            valid_time_source.clone_from(&existing_valid_time_source);
        }

        if let Some(ref prod) = existing_producer {
            diag = diag.with_producer(prod.clone());
        }

        diags.push(diag);
    }

    diags.sort_by(|a, b| a.id().cmp(b.id()));
    records.extend(diags);
}

#[cfg(test)]
mod redact_code_text_termination_tests {
    use super::{SecretClass, redact_code_text};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    const SECRET: &str = "hunterSECRETtokenValueLong";

    /// Runs `redact_code_text` on a background thread with a hard deadline so a
    /// termination regression FAILS the test instead of hanging the suite. Before
    /// the forward-progress guard, a quoted env secret spun forever here: the env
    /// span matcher strips the wrapping quotes, re-detects the `<REDACTED:secret>`
    /// placeholder it just wrote as the value, and re-replaces it with the
    /// identical placeholder, so `value` never changes.
    fn redact_with_deadline(
        input: &str,
    ) -> (String, std::collections::HashMap<SecretClass, usize>) {
        let owned = input.to_owned();
        let (tx, rx) = mpsc::channel();
        let handle = thread::spawn(move || {
            let out = redact_code_text(owned, "<REDACTED:secret>");
            let _ = tx.send(out);
        });
        let got = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("redact_code_text must terminate (no infinite loop) on a quoted env secret");
        handle.join().expect("worker thread joins");
        got
    }

    #[test]
    fn terminates_and_redacts_double_quoted_env_secret() {
        let input = format!("API_KEY=\"{SECRET}\" trailing");
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains(SECRET),
            "double-quoted env secret must be fully redacted: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED:"),
            "a redaction marker must be present: {redacted}"
        );
    }

    #[test]
    fn terminates_and_redacts_single_quoted_env_secret() {
        let input = format!("API_KEY='{SECRET}' trailing");
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains(SECRET),
            "single-quoted env secret must be fully redacted: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED:"),
            "a redaction marker must be present: {redacted}"
        );
    }

    #[test]
    fn continues_redacting_after_quoted_placeholder() {
        // Two INDEPENDENT env secrets, the first quoted. After the first is
        // redacted the loop re-detects the `<REDACTED:secret>` placeholder it just
        // wrote as the (quote-stripped) env value. The old forward-progress guard
        // `break`-ed out of the whole loop there, so the SECOND, unquoted secret
        // after it was never scanned and survived into the protected log payload
        // (Codex round-8 P1). The cursor fix must skip PAST the placeholder and
        // keep scanning, redacting the second secret too.
        let input = "API_KEY=\"firstSecretValue\" PASSWORD=secondSecretValue".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("firstSecretValue"),
            "the first (quoted) env secret must be redacted: {redacted}"
        );
        assert!(
            !redacted.contains("secondSecretValue"),
            "the second env secret after the quoted placeholder must ALSO be redacted: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED:"),
            "a redaction marker must be present: {redacted}"
        );
    }

    #[test]
    fn terminates_and_fully_redacts_overlapping_env_value_with_suffix() {
        // Unquoted env value whose bytes CONTAIN a higher-priority API token plus a
        // trailing suffix. The re-scan-from-0 loop must collapse the whole value
        // (prefix + token + suffix) and still terminate.
        let input = "PASSWORD=abcdefgh-sk-abcdefghijklmnopqrst!tail rest".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("!tail"),
            "the trailing suffix of the overlapping env value must not survive: {redacted}"
        );
        assert!(
            redacted.contains("<REDACTED:"),
            "a redaction marker must be present: {redacted}"
        );
        assert!(
            redacted.contains("rest"),
            "text after the env value token is preserved: {redacted}"
        );
    }

    #[test]
    fn terminates_and_fully_redacts_env_value_starting_with_token() {
        // Round-9 Codex P1: the env value BEGINS with a higher-priority API token
        // (no prefix before it) and carries a secret suffix after it. Pass 1 redacts
        // only the leading token, leaving `PASSWORD=<REDACTED:secret>!TAILLEAKMARKER`.
        // A prefix-only "already redacted" guard then skips the assignment because
        // the value STARTS WITH `<REDACTED:`, and the tail leaks into the protected
        // blob. The exact-placeholder guard must re-detect the whole value and
        // collapse prefix + token + tail into one placeholder — and terminate.
        let input = "PASSWORD=sk-abcdefghijklmnopqrst!TAILLEAKMARKER".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("TAILLEAKMARKER"),
            "the secret tail after the leading token must not survive: {redacted}"
        );
        assert!(
            !redacted.contains('!'),
            "no byte of the env value may survive: {redacted}"
        );
        assert_eq!(
            redacted, "PASSWORD=<REDACTED:secret>",
            "the whole env value collapses to exactly one placeholder: {redacted}"
        );
    }

    #[test]
    fn terminates_and_fully_redacts_quoted_env_value_starting_with_token() {
        // Case 4: the quoted env value begins with the token and hides a tail after
        // it. Quotes are preserved; every secret byte inside them is redacted.
        let input = "PASSWORD=\"sk-abcdefghijklmnopqrst!TAILLEAKMARKER\"".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("TAILLEAKMARKER"),
            "the secret tail inside the quotes must not survive: {redacted}"
        );
        assert_eq!(
            redacted, "PASSWORD=\"<REDACTED:secret>\"",
            "the quoted env value collapses to exactly one placeholder: {redacted}"
        );
    }

    #[test]
    fn does_not_re_redact_exact_placeholder_env_value() {
        // Case 3: an env value that is ALREADY exactly one placeholder must be left
        // untouched and MUST terminate — the exact-placeholder skip, not a prefix
        // heuristic, is what prevents an infinite re-redaction loop here.
        let input = "PASSWORD=<REDACTED:secret>".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert_eq!(
            redacted, input,
            "a value that is exactly a placeholder is not re-redacted: {redacted}"
        );
    }

    #[test]
    fn terminates_and_redacts_double_quoted_env_secret_with_spaces() {
        // Round-10 Codex P1: a quoted passphrase whose value contains INTERNAL
        // whitespace. The delimiter-only value boundary truncated the span at the
        // first space, redacting only `correct`; the next pass then read the
        // `"<REDACTED:secret>` head as an already-redacted exact placeholder and left
        // `horse PASSPHRASELEAK staple` in the protected-capture blob. The quote-aware
        // boundary must run the value span to the matching CLOSING quote so the whole
        // passphrase — spaces included — collapses to exactly one placeholder.
        let input = "PASSWORD=\"correct horse PASSPHRASELEAK staple\"".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("PASSPHRASELEAK"),
            "the multi-word passphrase must not survive past the first space: {redacted}"
        );
        assert!(
            !redacted.contains("horse") && !redacted.contains("staple"),
            "no whitespace-separated word of the quoted value may survive: {redacted}"
        );
        assert_eq!(
            redacted, "PASSWORD=\"<REDACTED:secret>\"",
            "the whole quoted passphrase collapses to exactly one placeholder: {redacted}"
        );
    }

    #[test]
    fn terminates_and_redacts_single_quoted_env_secret_with_spaces() {
        // Same round-10 leak, single-quoted. Internal whitespace is part of the value;
        // the matching single quote closes it.
        let input = "PASSWORD='correct horse PASSPHRASELEAK staple'".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("PASSPHRASELEAK"),
            "the single-quoted passphrase must be fully redacted: {redacted}"
        );
        assert!(
            !redacted.contains("horse") && !redacted.contains("staple"),
            "no whitespace-separated word of the single-quoted value may survive: {redacted}"
        );
        assert_eq!(
            redacted, "PASSWORD='<REDACTED:secret>'",
            "the whole single-quoted passphrase collapses to exactly one placeholder: {redacted}"
        );
    }

    #[test]
    fn terminates_and_redacts_quoted_env_secret_with_token_and_trailing_words() {
        // Round-10, trace 3: a quoted value that BEGINS with a higher-priority API
        // token, then whitespace, then a secret tail. Pass 1 redacts the token; the
        // quote-aware env boundary must then collapse the whole quoted run — token
        // placeholder AND the trailing words — to one placeholder.
        let input = "PASSWORD=\"sk-abcdefghijklmnopqrst PASSPHRASELEAK words\"".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("PASSPHRASELEAK"),
            "the trailing words after the in-value token must not survive: {redacted}"
        );
        assert!(
            !redacted.contains("words"),
            "no trailing word inside the quotes may survive: {redacted}"
        );
        assert_eq!(
            redacted, "PASSWORD=\"<REDACTED:secret>\"",
            "the whole quoted value collapses to exactly one placeholder: {redacted}"
        );
    }

    #[test]
    fn unquoted_env_value_space_still_delimits() {
        // Regression guard for the quote-aware fix: an UNQUOTED env value must keep the
        // existing behavior exactly — a space genuinely ends the value, so a following
        // word is NOT part of the secret and must be preserved verbatim.
        let input = "PASSWORD=secretvaluelong nextword".to_owned();
        let (redacted, _counts) = redact_with_deadline(&input);
        assert!(
            !redacted.contains("secretvaluelong"),
            "the unquoted secret value must be redacted: {redacted}"
        );
        assert_eq!(
            redacted, "PASSWORD=<REDACTED:secret> nextword",
            "the word after an unquoted env value is preserved (space delimits): {redacted}"
        );
    }
}
