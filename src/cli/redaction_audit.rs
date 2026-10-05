use super::*;

// ---------------------------------------------------------------------------
// redaction-audit — resting-store secret sweep (issue #244)
// ---------------------------------------------------------------------------

/// Stable machine-readable reason for a clean sweep: the scope exists and no
/// secret-shaped value was found in any swept field — a citable positive
/// fact, not a no-match.
pub(crate) const REDACTION_AUDIT_CLEAN_REASON: &str = "no_unredacted_secrets_in_scope";

/// Human-readable message for a clean sweep.
pub(crate) const REDACTION_AUDIT_CLEAN_MESSAGE: &str =
    "No secret-shaped values found at rest in the swept fields";

/// Soundness boundary carried on every response: the sweep is heuristic —
/// findings are never confirmed secrets, and a clean sweep is not proof the
/// store holds no secrets.
pub(crate) const REDACTION_AUDIT_DISCLAIMER: &str = "Advisory sweep: findings are pattern- and \
     entropy-heuristic matches over the documented swept fields — never confirmed \
     secrets, and never raw values. A clean sweep is not proof the store holds no \
     secrets; it only reports that no secret-shaped value was found at rest.";

/// Exit code when the audit reports ≥1 finding (issue #244: exit is non-zero
/// on findings, `0` on a clean store).
pub(crate) const REDACTION_AUDIT_EXIT_FINDINGS: i32 = 3;

/// Top-level redaction-audit response envelope: one compact JSON document on a
/// single line, like the sibling query lanes — byte-identical across runs on
/// an unchanged store.
#[derive(Serialize)]
pub(crate) struct RedactionAuditResponse<'a> {
    ok: bool,
    lane: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_scope: Option<&'a str>,
    disclaimer: &'static str,
    findings: &'a [query::AuditFinding],
    counts: RedactionAuditCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}

/// Deterministic sweep tallies in the redaction-audit response.
#[derive(Serialize)]
pub(crate) struct RedactionAuditCounts {
    records_scanned: usize,
    fields_scanned: usize,
    findings: usize,
}

/// Run `eg query redaction-audit`: sweep the store slice for secret-shaped
/// values at rest and report findings (hashes only — never raw values).
///
/// Exit `0` on a clean store; exit [`REDACTION_AUDIT_EXIT_FINDINGS`] when ≥1
/// finding is reported. Usage errors (unknown `--repo`, unreadable input,
/// both/neither of `--graph`/`--data-dir`) exit `1` via the shared loader.
pub(crate) fn query_redaction_audit_cmd(
    records: &[GraphRecord],
    index: &query::RepositoryIndex,
    repo_scope: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    // `--repo` scoping narrows the swept slice to records the repository
    // index attributes to the selected repository — the same contract as the
    // sibling inventory lanes. Unattributed records leave the slice when a
    // scope is given.
    let scoped: Vec<&GraphRecord> = records
        .iter()
        .filter(|record| repo_scope.is_none_or(|repo| index.owner_of(record.id()) == Some(repo)))
        .collect();
    let report = query::redaction_audit(scoped);
    let clean = report.findings.is_empty();

    match format {
        OutputFormat::Json => {
            let response = RedactionAuditResponse {
                ok: true,
                lane: "redaction_audit",
                repo_scope,
                disclaimer: REDACTION_AUDIT_DISCLAIMER,
                findings: &report.findings,
                counts: RedactionAuditCounts {
                    records_scanned: report.records_scanned,
                    fields_scanned: report.fields_scanned,
                    findings: report.findings.len(),
                },
                empty_reason: clean.then_some(REDACTION_AUDIT_CLEAN_REASON),
                message: clean.then_some(REDACTION_AUDIT_CLEAN_MESSAGE),
            };
            println!(
                "{}",
                serde_json::to_string(&response).context("failed to serialize redaction audit")?
            );
        }
        OutputFormat::Text => {
            if clean {
                println!("# {REDACTION_AUDIT_CLEAN_REASON}: {REDACTION_AUDIT_CLEAN_MESSAGE}");
            } else {
                println!(
                    "# redaction-audit: {} finding(s) — hashes only, never raw values",
                    report.findings.len()
                );
                for finding in &report.findings {
                    // The text rendering carries only the citable handle plus
                    // classification and hash prefix — a raw secret can never
                    // appear here because findings never carry one.
                    println!(
                        "- {} [{}] {}: {} hash_prefix={}",
                        finding.record_id,
                        finding.domain,
                        finding.field_path,
                        finding.classification,
                        finding.hash_prefix
                    );
                }
            }
            println!(
                "counts: records_scanned={} fields_scanned={} findings: {}",
                report.records_scanned,
                report.fields_scanned,
                report.findings.len()
            );
        }
    }

    if !clean {
        std::process::exit(REDACTION_AUDIT_EXIT_FINDINGS);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(
        record_id: &str,
        domain: &str,
        field_path: &str,
        classification: &'static str,
        hash_prefix: &str,
    ) -> query::AuditFinding {
        query::AuditFinding {
            record_id: record_id.to_owned(),
            domain: domain.to_owned(),
            field_path: field_path.to_owned(),
            classification,
            hash_prefix: hash_prefix.to_owned(),
        }
    }

    fn render_json(findings: &[query::AuditFinding]) -> String {
        let response = RedactionAuditResponse {
            ok: true,
            lane: "redaction_audit",
            repo_scope: None,
            disclaimer: REDACTION_AUDIT_DISCLAIMER,
            findings,
            counts: RedactionAuditCounts {
                records_scanned: 1,
                fields_scanned: 3,
                findings: findings.len(),
            },
            empty_reason: findings.is_empty().then_some(REDACTION_AUDIT_CLEAN_REASON),
            message: findings.is_empty().then_some(REDACTION_AUDIT_CLEAN_MESSAGE),
        };
        serde_json::to_string(&response).expect("response should serialize")
    }

    #[test]
    fn json_envelope_carries_findings_without_raw_values() {
        let findings = [finding(
            "rec:1",
            "agent_memory",
            "text",
            "api_token",
            "abcdef123456",
        )];
        let json = render_json(&findings);
        for field in [
            "record_id",
            "domain",
            "field_path",
            "classification",
            "hash_prefix",
            "records_scanned",
            "fields_scanned",
            "findings",
            "disclaimer",
        ] {
            assert!(json.contains(field), "envelope should carry {field}");
        }
        assert!(json.contains("\"lane\":\"redaction_audit\""));
        assert!(
            !json.contains("empty_reason"),
            "findings envelope carries no empty reason"
        );
        assert_eq!(json.lines().count(), 1, "one compact document per line");
    }

    #[test]
    fn json_envelope_clean_carries_stable_empty_reason() {
        let json = render_json(&[]);
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("envelope is valid JSON");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["findings"].as_array().expect("array").len(), 0);
        assert_eq!(parsed["empty_reason"], REDACTION_AUDIT_CLEAN_REASON);
        assert_eq!(parsed["message"], REDACTION_AUDIT_CLEAN_MESSAGE);
        assert_eq!(parsed["counts"]["findings"], 0);
    }

    #[test]
    fn exit_code_constants_match_the_documented_contract() {
        assert_eq!(
            REDACTION_AUDIT_EXIT_FINDINGS, 3,
            "findings exit is the documented non-zero code"
        );
    }
}
