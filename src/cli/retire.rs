use super::*;

use std::io::Write as _;

use crate::graph_index::Selector;
use crate::memory_retire::{
    ReinstateRequest, RetireError, RetireReason, RetireRequest, RetirementStateLabel,
    reinstate_from_records, retire_from_records,
};

/// Arguments for [`retire_cmd`], bundled so the command signature stays under
/// the argument-count lint while each CLI flag stays explicit at the call
/// site.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) struct RetireCmdArgs<'a> {
    pub handle: &'a str,
    pub graph: Option<&'a Path>,
    pub data_dir: Option<&'a Path>,
    pub reason: &'a str,
    pub superseded_by: Option<String>,
    pub evidence_handle: Option<String>,
    pub retired_by: String,
    pub transaction_time: Option<String>,
}

/// Arguments for [`reinstate_cmd`]; see [`RetireCmdArgs`].
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) struct ReinstateCmdArgs<'a> {
    pub handle: &'a str,
    pub graph: Option<&'a Path>,
    pub data_dir: Option<&'a Path>,
    pub reason: String,
    pub reinstated_by: String,
    pub transaction_time: Option<String>,
}

/// The two write backends the retire/reinstate commands support: an offline
/// `JSONL` graph file, or the embedded `AletheiaDB` store.
#[cfg(feature = "embedded-aletheiadb")]
enum Backend {
    Graph(PathBuf),
    Embedded(Box<EmbeddedAletheiaSink>, PathBuf),
}

/// Default embedded store when neither `--graph` nor `--data-dir` is given.
/// Explicitness is tracked by the `Option`, so the exclusivity check in
/// [`load_backend`] never has to guess whether `.egregore` was typed or
/// defaulted.
#[cfg(feature = "embedded-aletheiadb")]
const DEFAULT_EMBEDDED_DATA_DIR: &str = ".egregore";

/// Load the current record view and the matching writer for the retire /
/// reinstate commands. Follows the codebase's `(Option, Option)` loader
/// convention: exactly one of `--graph` / `--data-dir` wins, both is an
/// error, neither falls back to the default embedded store.
#[cfg(feature = "embedded-aletheiadb")]
fn load_backend(
    graph: Option<&Path>,
    data_dir: Option<&Path>,
) -> Result<(Vec<GraphRecord>, Backend)> {
    match (graph, data_dir) {
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (Some(path), None) => {
            let records = load_records_from_jsonl_selected(path, &Selector::Whole)
                .with_context(|| format!("failed to read graph {}", path.display()))?;
            Ok((records, Backend::Graph(path.to_owned())))
        }
        (None, data_dir) => {
            let dir = data_dir.unwrap_or_else(|| Path::new(DEFAULT_EMBEDDED_DATA_DIR));
            validate_existing_embedded_store(dir)?;
            let sink = EmbeddedAletheiaSink::open(dir)
                .with_context(|| format!("failed to open embedded store {}", dir.display()))?;
            let records = sink
                .read_all_records()
                .map_err(|e| anyhow::anyhow!("failed to read from embedded store: {e}"))?;
            Ok((records, Backend::Embedded(Box::new(sink), dir.to_owned())))
        }
    }
}

/// Persist generated receipt/edge records to the backend the command read
/// from.
#[cfg(feature = "embedded-aletheiadb")]
fn persist_generated(backend: Backend, generated: &[GraphRecord]) -> Result<()> {
    match backend {
        Backend::Embedded(mut sink, dir) => {
            let report = ingest_records(generated, sink.as_mut());
            if !report.is_success() {
                for failure in &report.failures {
                    eprintln!("{}: {}", failure.record_id, failure.message);
                }
                anyhow::bail!("failed to write retirement records to store");
            }
            sink.persist_indexes()
                .with_context(|| format!("failed to persist embedded store {}", dir.display()))?;
        }
        Backend::Graph(path) => {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("failed to append to graph {}", path.display()))?;
            for record in generated {
                writeln!(
                    file,
                    "{}",
                    serde_json::to_string(record).expect("receipt serializes")
                )?;
            }
        }
    }
    Ok(())
}

/// Parse the `--reason` flag into the typed reason vocabulary (issue #156).
/// An unknown reason prints the machine-readable refusal envelope and exits.
///
/// The issue text spells the fourth reason `operator_decision`; the canonical
/// receipt code is kebab-case (`operator-decision`), so both spellings are
/// accepted at the CLI boundary and canonicalized.
#[cfg(feature = "embedded-aletheiadb")]
fn parse_retire_reason(reason: &str) -> RetireReason {
    match reason {
        "superseded" => RetireReason::Superseded,
        "drifted" => RetireReason::Drifted,
        "contradicted" => RetireReason::Contradicted,
        "operator-decision" | "operator_decision" => RetireReason::OperatorDecision,
        other => {
            let error = RetireError::InvalidReason {
                reason: other.to_owned(),
            };
            eprintln!("{}", error.to_json());
            std::process::exit(error.exit_code());
        }
    }
}

/// Print a refusal envelope and exit with its exit code. Diverges so callers
/// can use it as a match arm or `unwrap_or_else` target.
#[cfg(feature = "embedded-aletheiadb")]
fn exit_with_retire_error(error: &RetireError) -> ! {
    eprintln!("{}", error.to_json());
    std::process::exit(error.exit_code());
}

/// Print a reinstatement refusal envelope and exit with its exit code.
/// Diverges so callers can use it as a match arm or `unwrap_or_else` target.
#[cfg(feature = "embedded-aletheiadb")]
fn exit_with_reinstate_error(error: &crate::memory_retire::ReinstateError) -> ! {
    eprintln!("{}", error.to_json());
    std::process::exit(error.exit_code());
}

/// Implements `eg retire` (issue #156): retire an agent-memory record from
/// recall without erasing its history.
///
/// Reads the current store view (embedded data dir or an offline `--graph`
/// JSONL file), resolves the retirement through the pure
/// [`crate::memory_retire`] logic, persists the generated receipt node (and
/// the normal `SUPERSEDES` edge for `superseded`) through the adapter or by
/// appending to the graph file, and prints a machine-readable JSON envelope.
/// Failures print a JSON envelope to stderr and exit 1 (refused or malformed)
/// or 2 (handle not found).
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn retire_cmd(args: RetireCmdArgs<'_>) -> Result<()> {
    let reason = parse_retire_reason(args.reason);
    let req = RetireRequest {
        handle: args.handle.to_owned(),
        reason,
        superseded_by: args.superseded_by,
        evidence_handle: args.evidence_handle,
        retired_by: args.retired_by,
        transaction_time: args.transaction_time,
    };
    let (records, backend) = load_backend(args.graph, args.data_dir)?;

    let outcome =
        retire_from_records(&records, &req).unwrap_or_else(|error| exit_with_retire_error(&error));

    let (action, event) = match outcome {
        crate::memory_retire::RetireOutcome::Retired {
            event,
            records: generated,
        } => {
            persist_generated(backend, &generated)?;
            ("retired", event)
        }
        crate::memory_retire::RetireOutcome::AlreadyRetired { event, .. } => {
            ("already_retired", event)
        }
    };

    let envelope = serde_json::json!({
        "ok": true,
        "action": action,
        "receipt": event,
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

/// Implements `eg reinstate` (issue #156): return a retired agent-memory
/// record to active recall by appending a `ReinstatementReceipt`.
///
/// Same backend handling, envelope shape, and exit-code contract as
/// [`retire_cmd`]. Reinstating a record that is not retired is a refusal
/// (exit 1), not a silent no-op.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn reinstate_cmd(args: ReinstateCmdArgs<'_>) -> Result<()> {
    let req = ReinstateRequest {
        handle: args.handle.to_owned(),
        reason: args.reason,
        reinstated_by: args.reinstated_by,
        transaction_time: args.transaction_time,
    };
    let (records, backend) = load_backend(args.graph, args.data_dir)?;

    let outcome = reinstate_from_records(&records, &req)
        .unwrap_or_else(|error| exit_with_reinstate_error(&error));

    let (action, event) = match outcome {
        crate::memory_retire::ReinstateOutcome::Reinstated {
            event,
            records: generated,
        } => {
            persist_generated(backend, &generated)?;
            ("reinstated", event)
        }
        crate::memory_retire::ReinstateOutcome::AlreadyActive { event, .. } => {
            ("already_active", event)
        }
    };

    let envelope = serde_json::json!({
        "ok": true,
        "action": action,
        "receipt": event,
    });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

/// `eg query retirement` response envelope (issue #156).
#[derive(serde::Serialize)]
struct RetirementQueryResponse {
    ok: bool,
    handle: String,
    retirement_state: RetirementStateLabel,
    receipts: Vec<serde_json::Value>,
    target: GraphRecord,
}

impl PrintText for RetirementQueryResponse {
    fn as_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!("{}: {}\n", self.handle, self.retirement_state.state);
        for receipt in &self.receipts {
            let action = receipt
                .get("action")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let at = receipt
                .get("retired_at")
                .or_else(|| receipt.get("reinstated_at"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            let reason = receipt
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let _ = writeln!(out, "  {action} at {at} ({reason})");
        }
        out
    }
}

/// Implements `eg query retirement` (issue #156): report one record's
/// retirement state and its ordered receipt trail, with the target's original
/// provenance, without mutating anything.
///
/// `--as-of` pins the transaction-time instant: receipts after the pin are
/// invisible, so the reported state matches what recall would have shown
/// then.
pub(crate) fn query_retirement_cmd(
    handle: &str,
    graph: Option<&Path>,
    data_dir: Option<&Path>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let as_of = as_of.map(|raw| {
        chrono::DateTime::parse_from_rfc3339(raw).unwrap_or_else(|_| {
            let envelope = serde_json::json!({
                "ok": false,
                "error": {
                    "code": "retirement_query_invalid_as_of",
                    "message": format!("--as-of must be RFC 3339, got {raw:?}"),
                },
            });
            eprintln!("{envelope}");
            std::process::exit(1);
        })
    });
    let records = load_records_selected(graph, data_dir, &Selector::Whole)?;
    let target = records.iter().find(|record| record.id() == handle);
    let Some(target) = target else {
        let envelope = serde_json::json!({
            "ok": false,
            "error": {
                "code": "retirement_query_unknown_record",
                "message": format!("no record with id {handle:?}"),
                "handle": handle,
            },
        });
        eprintln!("{envelope}");
        std::process::exit(2);
    };

    let states = crate::memory_retire::retirement_states(&records, as_of);
    let label = crate::memory_retire::retirement_label(&states, handle);
    let trail = crate::memory_retire::receipt_trail(&records, handle, as_of);
    let receipts: Vec<serde_json::Value> = trail
        .into_iter()
        .map(|entry| {
            let mut value = entry.event;
            if let serde_json::Value::Object(ref mut map) = value {
                map.insert(
                    "action".to_owned(),
                    serde_json::Value::String(entry.action.to_owned()),
                );
            }
            value
        })
        .collect();

    let envelope = RetirementQueryResponse {
        ok: true,
        handle: handle.to_owned(),
        retirement_state: label,
        receipts,
        target: target.clone(),
    };
    print_result(&envelope, format)?;
    Ok(())
}
