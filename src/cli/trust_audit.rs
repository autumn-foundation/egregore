use super::*;
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// trust-audit — producer trust-class audit (issue #236)
// ---------------------------------------------------------------------------

/// Stable machine-readable reason for a clean audit: the store was scanned
/// and no record's producer class contradicts its node kind's trust class —
/// a citable positive fact, not silence.
pub(crate) const TRUST_AUDIT_CLEAN_REASON: &str = "no_trust_violations";

/// Human-readable message for a clean audit.
pub(crate) const TRUST_AUDIT_CLEAN_MESSAGE: &str = "No producer trust-class violations: every record's producer class is \
     consistent with its node kind's trust class";

/// Soundness boundary carried on every response: the audit verifies producer
/// *attribution*, not producer *honesty* — a correctly-classed producer can
/// still have written wrong content, and a clean audit is not proof the
/// store is complete or correct.
pub(crate) const TRUST_AUDIT_DISCLAIMER: &str = "Attribution audit: each \
     record's producer class is checked against the trust class its node \
     kind implies (deterministic code-fact kinds only from deterministic \
     producers; agent-authored kinds only from agent producers). A violation \
     is a fact about the store, not a verdict about the code it describes; \
     a clean audit is not proof the store is complete or correct.";

/// Top-level trust-audit response envelope: one compact JSON document on a
/// single line, like the sibling query lanes — byte-identical across runs on
/// an unchanged store.
#[derive(Serialize)]
pub(crate) struct TrustAuditResponse<'a> {
    ok: bool,
    lane: &'static str,
    disclaimer: &'static str,
    violations: &'a [query::TrustViolation],
    counts: TrustAuditCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    empty_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<&'static str>,
}

/// Deterministic audit tallies in the trust-audit response.
#[derive(Serialize)]
pub(crate) struct TrustAuditCounts {
    records_scanned: usize,
    nodes_scanned: usize,
    edges_scanned: usize,
    tombstones_scanned: usize,
    legacy_records_skipped: usize,
    violations: usize,
}

/// Run `eg query trust-audit`: scan every persisted node, edge, and
/// tombstone and verify each record's `producer_kind` is consistent with the
/// trust class its node kind implies.
///
/// Exit `0` whether or not violations are reported (a clean store is an
/// affirmative `ok:true` verdict with counted totals, not silence); usage
/// errors (unreadable input, both/neither of `--graph`/`--data-dir`) exit
/// `1` via the shared loader. Strictly read-only.
pub(crate) fn query_trust_audit_cmd(records: &[GraphRecord], format: OutputFormat) -> Result<()> {
    let report = query::trust_audit(records);
    let clean = report.violations.is_empty();

    match format {
        OutputFormat::Json => {
            let response = TrustAuditResponse {
                ok: true,
                lane: "trust_audit",
                disclaimer: TRUST_AUDIT_DISCLAIMER,
                violations: &report.violations,
                counts: TrustAuditCounts {
                    records_scanned: report.records_scanned,
                    nodes_scanned: report.nodes_scanned,
                    edges_scanned: report.edges_scanned,
                    tombstones_scanned: report.tombstones_scanned,
                    legacy_records_skipped: report.legacy_records_skipped,
                    violations: report.violations.len(),
                },
                empty_reason: clean.then_some(TRUST_AUDIT_CLEAN_REASON),
                message: clean.then_some(TRUST_AUDIT_CLEAN_MESSAGE),
            };
            println!(
                "{}",
                serde_json::to_string(&response).context("failed to serialize trust audit")?
            );
        }
        OutputFormat::Text => {
            if clean {
                println!("# {TRUST_AUDIT_CLEAN_REASON}: {TRUST_AUDIT_CLEAN_MESSAGE}");
            } else {
                println!(
                    "# trust-audit: {} violation(s) — citable records, never synthesized prose",
                    report.violations.len()
                );
                for violation in &report.violations {
                    // The text rendering carries only the citable handle
                    // (`record_id`, `kind`, `producer_kind`, the broken rule)
                    // plus the handle/span where present.
                    let mut line = format!(
                        "- {} [{}] kind={} producer_kind={} rule={}",
                        violation.record_id,
                        violation.record_type,
                        violation.kind.as_deref().unwrap_or("unknown"),
                        violation.producer_kind,
                        violation.rule
                    );
                    if let Some(path) = violation.repo_relative_path.as_deref() {
                        line.push_str(" path=");
                        line.push_str(path);
                    }
                    if let Some(span) = violation.span {
                        // `write!` on a `String` is infallible; discard the `fmt::Result`.
                        let _ = write!(
                            line,
                            " span={}:{}-{}:{}",
                            span.start_line, span.start_byte, span.end_line, span.end_byte
                        );
                    }
                    println!("{line}");
                }
            }
            println!(
                "counts: records_scanned={} nodes_scanned={} edges_scanned={} \
                 tombstones_scanned={} legacy_records_skipped={} violations={}",
                report.records_scanned,
                report.nodes_scanned,
                report.edges_scanned,
                report.tombstones_scanned,
                report.legacy_records_skipped,
                report.violations.len()
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn violation(
        record_id: &str,
        record_type: &'static str,
        kind: Option<&str>,
        producer_kind: &'static str,
        rule: &'static str,
    ) -> query::TrustViolation {
        query::TrustViolation {
            record_id: record_id.to_owned(),
            record_type,
            kind: kind.map(str::to_owned),
            producer_kind,
            rule,
            repo_relative_path: None,
            span: None,
            source_id: None,
            target_id: None,
            deleted_id: None,
        }
    }

    fn render_json(violations: &[query::TrustViolation]) -> String {
        let report = query::TrustAuditReport {
            violations: violations.to_vec(),
            records_scanned: 4,
            nodes_scanned: 3,
            edges_scanned: 1,
            tombstones_scanned: 0,
            legacy_records_skipped: 0,
        };
        let response = TrustAuditResponse {
            ok: true,
            lane: "trust_audit",
            disclaimer: TRUST_AUDIT_DISCLAIMER,
            violations: &report.violations,
            counts: TrustAuditCounts {
                records_scanned: report.records_scanned,
                nodes_scanned: report.nodes_scanned,
                edges_scanned: report.edges_scanned,
                tombstones_scanned: report.tombstones_scanned,
                legacy_records_skipped: report.legacy_records_skipped,
                violations: report.violations.len(),
            },
            empty_reason: report
                .violations
                .is_empty()
                .then_some(TRUST_AUDIT_CLEAN_REASON),
            message: report
                .violations
                .is_empty()
                .then_some(TRUST_AUDIT_CLEAN_MESSAGE),
        };
        serde_json::to_string(&response).expect("response should serialize")
    }

    #[test]
    fn json_envelope_carries_violations_as_citable_records() {
        let violations = [violation(
            "rec:1",
            "node",
            Some("Symbol"),
            "observation_writer",
            query::RULE_CODE_FACT_WRITTEN_BY_AGENT_PRODUCER,
        )];
        let json = render_json(&violations);
        for field in [
            "record_id",
            "record_type",
            "kind",
            "producer_kind",
            "rule",
            "records_scanned",
            "nodes_scanned",
            "edges_scanned",
            "tombstones_scanned",
            "legacy_records_skipped",
            "violations",
            "disclaimer",
        ] {
            assert!(json.contains(field), "envelope should carry {field}");
        }
        assert!(json.contains("\"lane\":\"trust_audit\""));
        assert!(json.contains("\"ok\":true"));
        assert!(
            !json.contains("empty_reason"),
            "violations envelope carries no empty reason"
        );
        assert_eq!(json.lines().count(), 1, "one compact document per line");
    }

    #[test]
    fn json_envelope_clean_carries_stable_empty_reason() {
        let json = render_json(&[]);
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("envelope is valid JSON");
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["violations"].as_array().expect("array").len(), 0);
        assert_eq!(parsed["empty_reason"], TRUST_AUDIT_CLEAN_REASON);
        assert_eq!(parsed["message"], TRUST_AUDIT_CLEAN_MESSAGE);
        assert_eq!(parsed["counts"]["violations"], 0);
        assert_eq!(parsed["counts"]["records_scanned"], 4);
    }

    #[test]
    fn clean_constants_match_the_documented_contract() {
        assert_eq!(TRUST_AUDIT_CLEAN_REASON, "no_trust_violations");
    }
}
