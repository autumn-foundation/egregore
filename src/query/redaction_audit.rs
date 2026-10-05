//! Resting-store secret audit (issue #244).
//!
//! Read-only verification counterpart to the write-boundary redaction gate
//! ([`crate::redaction::validate_record`]): sweeps every persisted queryable
//! string field across all domains and reports secret-shaped values that
//! bypassed redaction — without ever surfacing a raw value.
//!
//! Detection is two-layered:
//!
//! - (a) Known token patterns via [`crate::redaction::detect_secret_span`],
//!   aligned to the `secret_class` taxonomy in `docs/schema/redaction.md`
//!   (`api_token`, `ssh_private_key`, `database_url`, `cloud_credential`,
//!   `webhook_secret`, `session_cookie`, `env_secret`, `email`).
//! - (b) Generic high-entropy tokens: maximal runs of token characters with
//!   length ≥ [`MIN_HIGH_ENTROPY_TOKEN_LEN`] and Shannon entropy ≥
//!   [`MIN_HIGH_ENTROPY_BITS_PER_CHAR`] bits per character (both documented in
//!   `docs/cli/redaction-audit.md`).
//!
//! Already-handled values are never findings:
//!
//! - records stamped `redaction_policy_version: "v1"` are skipped wholesale;
//! - `<REDACTED:secret_class:hash_prefix>` markers never match a secret
//!   pattern, so marker-only values are silent (a raw secret sharing a field
//!   with a marker is still flagged — mirroring the gate, which rejects such
//!   fields);
//! - the documented allowlist ([`ALLOWLIST_EXACT`], [`ALLOWLIST_PREFIXES`])
//!   suppresses public sample keys and test-fixture tokens.
//!
//! Each [`AuditFinding`] carries `record_id`, `domain`, `field_path`,
//! `classification` (`secret_class` or `high_entropy`), and a BLAKE3
//! `hash_prefix` — never the raw value. Findings are returned in canonical
//! order: `(record_id, field_path, classification, hash_prefix)`.

use std::collections::HashSet;

use serde::Serialize;

use crate::{
    ir::{Domain, GraphRecord, LogPayload, NodeKind},
    redaction,
};

// ── Detection thresholds (documented in docs/cli/redaction-audit.md) ─────────

/// Minimum token length (characters) for high-entropy detection.
///
/// Below this length even a perfectly random token is too short to be
/// distinguished from an ordinary identifier; 20 keeps UUIDs (36 chars but
/// ~4.1 bits/char) and SHA-1/256 hex digests (~4.0 bits/char) silent while
/// catching base32/base64-style key material.
pub const MIN_HIGH_ENTROPY_TOKEN_LEN: usize = 20;

/// Minimum Shannon entropy (bits per character) for high-entropy detection.
///
/// 4.2 sits above uniform hex (4.0) and UUID-with-dashes (~4.08) but below
/// uniform base32 (5.0), base62 (~5.95), and base64 (6.0), so random-looking
/// key material is flagged while content hashes pass through.
pub const MIN_HIGH_ENTROPY_BITS_PER_CHAR: f64 = 4.2;

// ── Allowlist (documented in docs/cli/redaction-audit.md §Allowlist) ──────────

/// Exact known-safe values: publicly documented sample keys and the harness's
/// own documented fixture token. A matched value equal to one of these is
/// never a finding.
pub const ALLOWLIST_EXACT: &[&str] = &[
    // AWS IAM documentation example access key ID.
    "AKIAIOSFODNN7EXAMPLE",
    // AWS documentation example secret access key.
    "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
    // Documented CI-harness token used by this repo's own audit fixtures.
    "dGhpcy1pcy1hLXRlc3QtZml4dHVyZS10b2tlbi0wMTIz",
];

/// Prefixes of well-known test/sample tokens.
///
/// A matched value starting with one of these prefixes is never a finding —
/// this keeps test fixtures and documented sample keys CI-clean. (A genuinely
/// leaked *test* key is suppressed by design; the audit targets
/// production-shaped secrets.)
pub const ALLOWLIST_PREFIXES: &[&str] = &[
    // Stripe test secret keys (docs.stripe.com).
    "sk-test-", // Stripe test restricted keys.
    "rk-test-", // Stripe test publishable keys.
    "pk-test-",
];

/// Returns `true` when `candidate` is a documented known-safe value.
#[must_use]
pub fn is_allowlisted(candidate: &str) -> bool {
    ALLOWLIST_EXACT.contains(&candidate)
        || ALLOWLIST_PREFIXES
            .iter()
            .any(|prefix| candidate.starts_with(prefix))
}

// ── Findings ─────────────────────────────────────────────────────────────────

/// Classification of one audit finding: a named `secret_class` from the
/// redaction taxonomy, or `high_entropy` for a generic high-entropy token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingClassification {
    /// A known token pattern (`api_token`, `ssh_private_key`, …).
    SecretClass(redaction::SecretClass),
    /// A generic token above the documented entropy + length threshold.
    HighEntropy,
}

impl FindingClassification {
    /// Canonical classification label used in findings output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SecretClass(class) => class.as_str(),
            Self::HighEntropy => "high_entropy",
        }
    }
}

/// One secret-shaped value found at rest.
///
/// Carries only the citable handle (`record_id`, `domain`, `field_path`), the
/// `classification`, and a non-reversible BLAKE3 `hash_prefix` for audit
/// correlation — never the raw value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditFinding {
    /// Stable record ID — the citable handle.
    pub record_id: String,
    /// Record domain (`agent_memory`, `codegraph`, `log`, …, or `unknown`).
    pub domain: String,
    /// Swept field path, e.g. `text`, `stdout_handle.inline`, `summary`,
    /// `log.template_excerpt`.
    pub field_path: String,
    /// `secret_class` name or `high_entropy`.
    pub classification: &'static str,
    /// First 12 lowercase-hex chars of the BLAKE3 hash of the detected value
    /// (same scheme as `<REDACTED:class:hash_prefix>` markers).
    pub hash_prefix: String,
}

/// The audit outcome: canonical-ordered findings plus sweep tallies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReport {
    /// Findings in canonical order: `(record_id, field_path, classification,
    /// hash_prefix)`.
    pub findings: Vec<AuditFinding>,
    /// Records swept (after any caller-side repo scoping).
    pub records_scanned: usize,
    /// Swept field instances across all records.
    pub fields_scanned: usize,
}

// ── Swept-field census ───────────────────────────────────────────────────────

/// `(field_path, value)` pairs the audit sweeps on one node record, in
/// canonical field order. This is the audit's field census, documented in
/// `docs/cli/redaction-audit.md §Swept fields`:
///
/// - every field of the canonical sensitive-field index
///   ([`redaction::sensitive_fields`]) — the redactable agent-memory,
///   artifact, verification, project, and user-context domains;
/// - log-graph excerpt fields (`ErrorSignature.template_excerpt`,
///   `LogEvent.event_excerpt`);
/// - code-graph free-text fields (`summary` — the symbol body — plus
///   `signature`, `doc`, `note`, `name`, `deprecated.since`,
///   `deprecated.note`, `author_name`), which are exempt from the write gate
///   and are the prime unredacted-at-rest risk.
///
/// `Commit.author_email` is deliberately excluded: a redaction-off local store
/// retains the raw author email by design
/// (`docs/schema/redaction.md`); export-time redaction is the bundle lane's
/// contract. Structural identifiers (record IDs, hashes, paths, spans, enum
/// labels) are not queryable free text and are not swept.
#[must_use]
pub fn audit_fields(record: &GraphRecord) -> Vec<(String, &str)> {
    let GraphRecord::Node {
        summary,
        name,
        signature,
        doc,
        note,
        deprecated,
        author_name,
        log,
        ..
    } = record
    else {
        return Vec::new();
    };

    let mut fields: Vec<(String, &str)> = redaction::sensitive_fields(record);

    if let Some(payload) = log {
        match payload.as_ref() {
            LogPayload::ErrorSignature(p) => {
                fields.push((
                    "log.template_excerpt".to_owned(),
                    p.template_excerpt.as_str(),
                ));
            }
            LogPayload::LogEvent(p) => {
                fields.push(("log.event_excerpt".to_owned(), p.event_excerpt.as_str()));
            }
            LogPayload::LogSource(_) | LogPayload::LogOccurrenceBucket(_) => {}
        }
    }

    // Code-graph free text. `summary` is required on every node; the rest are
    // optional and only present on code-graph kinds.
    fields.push(("summary".to_owned(), summary.as_str()));
    if let Some(n) = name {
        fields.push(("name".to_owned(), n.as_str()));
    }
    if let Some(s) = signature {
        fields.push(("signature".to_owned(), s.as_str()));
    }
    if let Some(d) = doc {
        fields.push(("doc".to_owned(), d.as_str()));
    }
    if let Some(n) = note {
        fields.push(("note".to_owned(), n.as_str()));
    }
    if let Some(mark) = deprecated {
        if let Some(since) = mark.since.as_deref() {
            fields.push(("deprecated.since".to_owned(), since));
        }
        if let Some(note_text) = mark.note.as_deref() {
            fields.push(("deprecated.note".to_owned(), note_text));
        }
    }
    if let Some(author) = author_name {
        fields.push(("author_name".to_owned(), author.as_str()));
    }

    fields
}

/// Domain label for a finding: the record's stamped `domain` when present,
/// else derived from the node kind (`codegraph` for code-graph kinds, `log`
/// for log-graph kinds, `unknown` otherwise).
#[must_use]
pub fn domain_label(record: &GraphRecord) -> String {
    let GraphRecord::Node { domain, kind, .. } = record else {
        return "unknown".to_owned();
    };
    if let Some(stamped) = domain {
        return stamped.clone();
    }
    if redaction::is_code_graph_kind(*kind) {
        return Domain::CodeGraph.as_str().to_owned();
    }
    if matches!(
        kind,
        NodeKind::LogSource
            | NodeKind::ErrorSignature
            | NodeKind::LogEvent
            | NodeKind::LogOccurrenceBucket
    ) {
        return Domain::Log.as_str().to_owned();
    }
    "unknown".to_owned()
}

/// Returns `true` when the record is already handled: stamped
/// `redaction_policy_version: "v1"`, so the write-boundary gate accepted it.
fn is_already_handled(record: &GraphRecord) -> bool {
    let GraphRecord::Node {
        redaction_policy_version,
        ..
    } = record
    else {
        return false;
    };
    redaction_policy_version.as_deref() == Some(redaction::REDACTION_POLICY_VERSION)
}

// ── Detection ────────────────────────────────────────────────────────────────

/// Shannon entropy of `token` in bits per character (byte-level distribution;
/// tokens are ASCII by construction).
#[must_use]
pub fn shannon_entropy(token: &str) -> f64 {
    let mut counts = [0_u32; 256];
    let mut len = 0_u32;
    for byte in token.bytes() {
        counts[usize::from(byte)] += 1;
        len += 1;
    }
    if len == 0 {
        return 0.0;
    }
    let n = f64::from(len);
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = f64::from(c) / n;
            -p * p.log2()
        })
        .sum()
}

/// Characters that may appear inside a high-entropy token: alphanumerics plus
/// the base64/base32/hex-with-separators punctuation.
const fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '.' | '_' | '-')
}

/// Maximal token-character runs in `value` as `(byte_start, byte_end)` spans.
fn token_spans(value: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    for (idx, c) in value.char_indices() {
        if is_token_char(c) {
            if start.is_none() {
                start = Some(idx);
            }
        } else if let Some(s) = start.take() {
            spans.push((s, idx));
        }
    }
    if let Some(s) = start {
        spans.push((s, value.len()));
    }
    spans
}

/// Audits one swept field, returning its findings (possibly empty).
///
/// Pattern pass first: every [`redaction::detect_secret_span`] match becomes a
/// finding (unless allowlisted); matched spans are masked with spaces so a
/// later, lower-priority pattern starting earlier in the field cannot be
/// shadowed, and so the entropy pass cannot double-count the same bytes.
/// Entropy pass second: every token above the documented threshold becomes a
/// `high_entropy` finding. Identical `(classification, hash_prefix)` findings
/// within one field are deduplicated — the handle is the citable unit.
fn audit_field(record_id: &str, domain: &str, field_path: &str, value: &str) -> Vec<AuditFinding> {
    if is_allowlisted(value) {
        return Vec::new();
    }
    let mut findings: Vec<AuditFinding> = Vec::new();
    let mut seen: HashSet<(&'static str, String)> = HashSet::new();
    let mut push = |classification: FindingClassification, secret: &str| {
        if is_allowlisted(secret) {
            return;
        }
        let label = classification.as_str();
        let hash_prefix = redaction::hash_prefix(secret);
        if seen.insert((label, hash_prefix.clone())) {
            findings.push(AuditFinding {
                record_id: record_id.to_owned(),
                domain: domain.to_owned(),
                field_path: field_path.to_owned(),
                classification: label,
                hash_prefix,
            });
        }
    };

    // Pattern pass: mask each matched span with spaces (same byte length, so
    // offsets stay valid) and re-scan until no pattern matches.
    let mut masked = value.to_owned();
    while let Some((class, start, len)) = redaction::detect_secret_span(&masked) {
        if len == 0 {
            break;
        }
        let end = start + len;
        let secret = masked[start..end].to_owned();
        push(FindingClassification::SecretClass(class), &secret);
        masked.replace_range(start..end, &" ".repeat(len));
    }

    // Entropy pass over the masked value: pattern spans are spaces now, so
    // tokens cannot overlap a pattern finding.
    for (start, end) in token_spans(&masked) {
        let token = &masked[start..end];
        if token.chars().count() >= MIN_HIGH_ENTROPY_TOKEN_LEN
            && shannon_entropy(token) >= MIN_HIGH_ENTROPY_BITS_PER_CHAR
        {
            push(FindingClassification::HighEntropy, token);
        }
    }

    findings
}

/// Sweeps every record's queryable string fields for secret-shaped values.
///
/// Records stamped `redaction_policy_version: "v1"` are skipped as
/// already-handled. Findings are returned in canonical order —
/// `(record_id, field_path, classification, hash_prefix)` — so repeated runs
/// over an unchanged store are byte-identical. The audit never surfaces a raw
/// value: findings carry only the citable handle plus classification and
/// BLAKE3 hash prefix.
#[must_use]
pub fn redaction_audit<'a>(records: impl IntoIterator<Item = &'a GraphRecord>) -> AuditReport {
    let mut findings: Vec<AuditFinding> = Vec::new();
    let mut records_scanned = 0_usize;
    let mut fields_scanned = 0_usize;
    for record in records {
        records_scanned += 1;
        if is_already_handled(record) {
            continue;
        }
        let record_id = record.id().to_owned();
        let domain = domain_label(record);
        for (field_path, value) in audit_fields(record) {
            fields_scanned += 1;
            findings.extend(audit_field(&record_id, &domain, &field_path, value));
        }
    }
    findings.sort_by(|a, b| {
        (
            a.record_id.as_str(),
            a.field_path.as_str(),
            a.classification,
            a.hash_prefix.as_str(),
        )
            .cmp(&(
                b.record_id.as_str(),
                b.field_path.as_str(),
                b.classification,
                b.hash_prefix.as_str(),
            ))
    });
    AuditReport {
        findings,
        records_scanned,
        fields_scanned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_with_text(id: &str, kind: NodeKind, text: Option<&str>) -> GraphRecord {
        let mut record = GraphRecord::node(
            id.to_owned(),
            kind,
            None,
            None,
            None,
            "fixture summary".to_owned(),
        );
        if let GraphRecord::Node {
            text: ref mut slot, ..
        } = record
        {
            *slot = text.map(str::to_owned);
        }
        record
    }

    #[test]
    fn entropy_thresholds_are_documented_constants() {
        assert_eq!(MIN_HIGH_ENTROPY_TOKEN_LEN, 20);
        assert!((MIN_HIGH_ENTROPY_BITS_PER_CHAR - 4.2).abs() < f64::EPSILON);
    }

    #[test]
    fn shannon_entropy_separates_random_from_prose() {
        // Uniform base64-ish token: high entropy.
        let random = "Fk3FAKE9c2E5b1D8f4A6c0E3b7D9a1F5c8E2b4D6a0F3e7Xy";
        assert!(
            shannon_entropy(random) >= MIN_HIGH_ENTROPY_BITS_PER_CHAR,
            "random token entropy must clear the threshold"
        );
        // Uniform hex (SHA-256): below the threshold by design.
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert!(
            shannon_entropy(hex) < MIN_HIGH_ENTROPY_BITS_PER_CHAR,
            "uniform hex must stay below the threshold"
        );
        // English prose: well below the detection threshold (its ~4.16
        // bits/char clears 4.0 but never reaches the 4.2 flag line).
        assert!(shannon_entropy("the quick brown fox jumps") < MIN_HIGH_ENTROPY_BITS_PER_CHAR);
        assert!(shannon_entropy("").abs() < f64::EPSILON);
    }

    #[test]
    fn pattern_detection_classifies_and_hashes_without_raw_value() {
        let record = node_with_text(
            "t:1",
            NodeKind::Observation,
            Some("key sk-FAKEAUDITKEY0123456789abcdefXYZ here"),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        assert_eq!(report.findings.len(), 1);
        let finding = &report.findings[0];
        assert_eq!(finding.record_id, "t:1");
        assert_eq!(finding.field_path, "text");
        assert_eq!(finding.classification, "api_token");
        assert_eq!(finding.hash_prefix.len(), 12);
        assert_eq!(
            finding.hash_prefix,
            redaction::hash_prefix("sk-FAKEAUDITKEY0123456789abcdefXYZ"),
            "hash_prefix is the BLAKE3 prefix of the detected value"
        );
    }

    #[test]
    fn high_entropy_token_is_flagged_as_high_entropy() {
        let record = node_with_text(
            "t:2",
            NodeKind::Observation,
            Some("saw Fk3FAKE9c2E5b1D8f4A6c0E3b7D9a1F5c8E2b4D6a0F3e7Xy in headers"),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].classification, "high_entropy");
        assert_eq!(report.findings[0].field_path, "text");
    }

    #[test]
    fn v1_stamped_records_are_never_flagged() {
        let mut record = node_with_text(
            "t:3",
            NodeKind::Observation,
            Some("key sk-FAKEV1STAMPED0123456789abcdefXY here"),
        );
        if let GraphRecord::Node {
            redaction_policy_version,
            ..
        } = &mut record
        {
            *redaction_policy_version = Some("v1".to_owned());
        }
        let report = redaction_audit(std::slice::from_ref(&record));
        assert!(
            report.findings.is_empty(),
            "a v1-stamped record is already-handled"
        );
    }

    #[test]
    fn marker_only_values_are_never_flagged() {
        let record = node_with_text(
            "t:4",
            NodeKind::Observation,
            Some("prefix <REDACTED:api_token:abcdef123456> suffix"),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        assert!(
            report.findings.is_empty(),
            "a redaction marker is already-handled"
        );
    }

    #[test]
    fn raw_secret_beside_a_marker_is_still_flagged() {
        // Mirrors the write gate: a field is only handled when fully redacted.
        let record = node_with_text(
            "t:5",
            NodeKind::Observation,
            Some("<REDACTED:api_token:abcdef123456> and sk-FAKEAUDITKEY0123456789abcdefXYZ"),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        assert_eq!(report.findings.len(), 1);
        assert_eq!(report.findings[0].classification, "api_token");
    }

    #[test]
    fn allowlist_suppresses_exact_and_prefix_matches() {
        for value in [
            "AKIAIOSFODNN7EXAMPLE",
            "sk-test-FAKEAUDITFIXTURE0123456789abcdef",
            "dGhpcy1pcy1hLXRlc3QtZml4dHVyZS10b2tlbi0wMTIz",
        ] {
            let record = node_with_text("t:6", NodeKind::Observation, Some(value));
            let report = redaction_audit(std::slice::from_ref(&record));
            assert!(
                report.findings.is_empty(),
                "allowlisted value {value:?} must not be flagged"
            );
        }
    }

    #[test]
    fn multiple_secrets_in_one_field_yield_multiple_findings() {
        let record = node_with_text(
            "t:7",
            NodeKind::Observation,
            Some(
                "a sk-FAKEAUDITKEY0123456789abcdefXYZ and b whsec_FAKEAUDIT0123456789abcdefXYZ123",
            ),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        let classes: Vec<&str> = report.findings.iter().map(|f| f.classification).collect();
        assert_eq!(classes, vec!["api_token", "webhook_secret"]);
    }

    #[test]
    fn findings_are_canonically_ordered() {
        let b = node_with_text(
            "t:b",
            NodeKind::Observation,
            Some("sk-FAKEAUDITKEY0123456789abcdefXYZ"),
        );
        let a = node_with_text(
            "t:a",
            NodeKind::Observation,
            Some("sk-FAKEAUDITKEY0123456789abcdefXYZ"),
        );
        // Feed in reverse order; output must still be canonical.
        let report = redaction_audit([&b, &a]);
        let ids: Vec<&str> = report
            .findings
            .iter()
            .map(|f| f.record_id.as_str())
            .collect();
        assert_eq!(ids, vec!["t:a", "t:b"]);
    }

    #[test]
    fn code_graph_symbol_body_is_swept() {
        let record = GraphRecord::node(
            "t:sym".to_owned(),
            NodeKind::Symbol,
            Some("src/config.rs".to_owned()),
            None,
            Some("load_key".to_owned()),
            "-----BEGIN RSA PRIVATE KEY-----\nFAKE\n-----END RSA PRIVATE KEY-----".to_owned(),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        assert_eq!(report.findings.len(), 1);
        let finding = &report.findings[0];
        assert_eq!(finding.field_path, "summary");
        assert_eq!(finding.classification, "ssh_private_key");
        assert_eq!(finding.domain, "codegraph");
    }

    #[test]
    fn domain_label_prefers_stamped_domain() {
        let mut record = node_with_text("t:8", NodeKind::Observation, None);
        if let GraphRecord::Node { domain, .. } = &mut record {
            *domain = Some("agent_memory".to_owned());
        }
        assert_eq!(domain_label(&record), "agent_memory");
        let bare = node_with_text("t:9", NodeKind::Observation, None);
        assert_eq!(domain_label(&bare), "unknown");
    }

    #[test]
    fn identical_repeat_findings_deduplicate_within_a_field() {
        let token = "sk-FAKEAUDITKEY0123456789abcdefXYZ";
        let record = node_with_text(
            "t:10",
            NodeKind::Observation,
            Some(&format!("{token} then {token} again")),
        );
        let report = redaction_audit(std::slice::from_ref(&record));
        assert_eq!(
            report.findings.len(),
            1,
            "the handle is the citable unit; repeats deduplicate"
        );
    }
}
