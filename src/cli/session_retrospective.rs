//! `eg query session`: retrace one agent session's footprint, claims, and
//! verification (issue #259).

use super::*;
use crate::query::{
    SessionResolveError, SessionRetrospective, SessionVerificationStatus, VerificationOutcome,
    resolve_session_ids, retrace_session,
};

/// The standing disclaimer, emitted in every response and in `--help`.
/// No footprint found is not evidence the run did nothing: a run that never
/// wrote a cited edge, a dropped transcript import, or a partially ingested
/// store all look the same as an idle session.
pub(crate) const SESSION_DISCLAIMER: &str = "No footprint found is not evidence the run did nothing: a run that never wrote a cited edge, a dropped transcript import, or a partially ingested store all look the same as an idle session.";

/// Projected view of a node record for the response envelope.
#[derive(Debug, Clone, Serialize)]
struct NodeView<'a> {
    record_id: &'a str,
    kind: &'a str,
    trust: &'a str,
    citations: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo_relative_path: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    span: Option<SourceSpan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'a str>,
}

fn node_view<'a>(record: &'a GraphRecord, trust: &'a str, citations: Vec<String>) -> NodeView<'a> {
    let GraphRecord::Node {
        id,
        kind,
        name,
        title,
        summary,
        text,
        repo_relative_path,
        span,
        status,
        ..
    } = record
    else {
        return NodeView {
            record_id: record.id(),
            kind: "unknown",
            trust,
            citations,
            name: None,
            title: None,
            summary: None,
            text: None,
            repo_relative_path: None,
            span: None,
            status: None,
        };
    };
    NodeView {
        record_id: id.as_str(),
        kind: kind.as_str(),
        trust,
        citations,
        name: name.as_deref(),
        title: title.as_deref(),
        summary: Some(summary.as_str()),
        text: text.as_deref(),
        repo_relative_path: repo_relative_path.as_deref(),
        span: *span,
        status: status.as_deref(),
    }
}

/// The full JSON response envelope for `eg query session`.
#[derive(Debug, Clone, Serialize)]
struct SessionResponse<'a> {
    ok: bool,
    session_id: &'a str,
    touched: Vec<NodeView<'a>>,
    observations: Vec<NodeView<'a>>,
    failures: Vec<NodeView<'a>>,
    artifacts: Vec<NodeView<'a>>,
    verification_evidence: Vec<VerificationView<'a>>,
    verification_status: SessionVerificationStatus,
    disclaimer: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct VerificationView<'a> {
    #[serde(flatten)]
    node: NodeView<'a>,
    outcome: &'a str,
}

impl<'a> SessionResponse<'a> {
    fn from_retro(retro: &'a SessionRetrospective<'a>) -> Self {
        let touched = retro
            .touched
            .iter()
            .map(|row| node_view(row.record, row.trust.as_str(), row.cited_by.clone()))
            .collect();
        let observations = retro
            .observations
            .iter()
            .map(|row| node_view(row.record, row.trust.as_str(), row.citations.clone()))
            .collect();
        let failures = retro
            .failures
            .iter()
            .map(|row| node_view(row.record, row.trust.as_str(), row.citations.clone()))
            .collect();
        let artifacts = retro
            .artifacts
            .iter()
            .map(|row| node_view(row.record, row.trust.as_str(), row.citations.clone()))
            .collect();
        let verification_evidence = retro
            .verification_evidence
            .iter()
            .map(|row| VerificationView {
                node: node_view(row.record, row.trust.as_str(), row.citations.clone()),
                outcome: match row.outcome {
                    VerificationOutcome::Passing => "passing",
                    VerificationOutcome::Failing => "failing",
                    VerificationOutcome::Inconclusive => "inconclusive",
                },
            })
            .collect();
        Self {
            ok: true,
            session_id: retro.session_id.as_str(),
            touched,
            observations,
            failures,
            artifacts,
            verification_evidence,
            verification_status: retro.verification_status,
            disclaimer: SESSION_DISCLAIMER,
        }
    }

    fn as_text(&self) -> String {
        use std::fmt::Write as _;

        let mut out = String::new();
        let _ = writeln!(out, "session {}", self.session_id);
        let _ = writeln!(out, "verification_status {}", self.verification_status);
        for (section, rows) in [
            ("touched", &self.touched),
            ("observations", &self.observations),
            ("failures", &self.failures),
            ("artifacts", &self.artifacts),
        ] {
            let _ = writeln!(out, "\n{section}:");
            for row in rows {
                let _ = writeln!(
                    out,
                    "  {} {} trust={} citations={}",
                    row.record_id,
                    row.kind,
                    row.trust,
                    row.citations.join(",")
                );
            }
            if rows.is_empty() {
                out.push_str("  (none)\n");
            }
        }
        out.push_str("\nverification_evidence:\n");
        for row in &self.verification_evidence {
            let _ = writeln!(
                out,
                "  {} {} outcome={} citations={}",
                row.node.record_id,
                row.node.kind,
                row.outcome,
                row.node.citations.join(",")
            );
        }
        if self.verification_evidence.is_empty() {
            out.push_str("  (none)\n");
        }
        let _ = writeln!(out, "\n{}", self.disclaimer);
        out
    }
}

pub(crate) fn query_session_cmd(
    records: &[GraphRecord],
    id_or_handle: &str,
    format: OutputFormat,
) -> Result<()> {
    let resolved_ids = match resolve_session_ids(records, id_or_handle) {
        Ok(ids) => ids,
        Err(SessionResolveError::Ambiguous) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "ambiguous",
                    "session_id": id_or_handle,
                    "message": "handle matches more than one live session; use a record ID"
                }
            });
            eprintln!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(1);
        }
        Err(SessionResolveError::NoMatch) => {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "no_match",
                    "session_id": id_or_handle
                }
            });
            println!("{}", serde_json::to_string(&envelope)?);
            std::process::exit(2);
        }
    };

    if resolved_ids.is_empty() {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "no_match",
                "session_id": id_or_handle
            }
        });
        println!("{}", serde_json::to_string(&envelope)?);
        std::process::exit(2);
    }

    let retro = retrace_session(records, &resolved_ids);
    let response = SessionResponse::from_retro(&retro);
    match format {
        OutputFormat::Json => println!("{}", serde_json::to_string(&response)?),
        OutputFormat::Text => print!("{}", response.as_text()),
    }
    Ok(())
}
