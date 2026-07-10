use super::*;

/// Machine-readable report emitted by `eg query evidence-freshness` (issue #85).
#[derive(serde::Serialize)]
pub(crate) struct EvidenceFreshnessReport {
    ok: bool,
    stale_only: bool,
    /// Verdict tally across `current` / `drifted` / `unresolved` / `untemporal`.
    counts: std::collections::BTreeMap<&'static str, usize>,
    /// Stable diagnostic so an empty stale-only result is never silent (AC7).
    diagnostic: &'static str,
    /// Per-evidence-link freshness verdicts, deterministically ordered.
    verdicts: Vec<crate::evidence_freshness::FreshnessVerdictEntry>,
}

/// Handles `eg query evidence-freshness --graph <path> | --data-dir <dir> [--stale-only]`.
///
/// Strictly read-only: computes verdicts from records already in the store and
/// never creates, modifies, or deletes anything. Output carries only record IDs,
/// hashes, handles, spans, confidence, and redaction markers — never raw
/// observation text or other protected payloads (AC9).
pub(crate) fn query_freshness_cmd(records: &[GraphRecord], stale_only: bool) -> Result<()> {
    let all = crate::evidence_freshness::evidence_link_freshness(records);
    let counts = crate::evidence_freshness::verdict_counts(&all);

    let verdicts = if stale_only {
        crate::evidence_freshness::stale_only(all)
    } else {
        all
    };

    // In stale-only mode an empty result is reported with a stable diagnostic,
    // never silently as success-with-nothing (AC7).
    let diagnostic = if stale_only {
        if verdicts.is_empty() {
            crate::evidence_freshness::NO_STALE_DIAGNOSTIC
        } else {
            crate::evidence_freshness::STALE_PRESENT_DIAGNOSTIC
        }
    } else {
        "freshness_verdicts"
    };

    let report = EvidenceFreshnessReport {
        ok: true,
        stale_only,
        counts,
        diagnostic,
        verdicts,
    };

    let output =
        serde_json::to_string_pretty(&report).context("failed to serialize freshness report")?;
    println!("{output}");
    Ok(())
}
