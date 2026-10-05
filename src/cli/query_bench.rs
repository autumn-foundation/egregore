use super::*;

use crate::bench_capture::{BenchmarkSummaryView, benchmark_summary_of};

/// A `BenchmarkRun` candidate for the target, with its parsed summary.
struct BenchCandidate<'a> {
    record: &'a GraphRecord,
    summary: Option<BenchmarkSummaryView>,
    executed_at: Option<&'a str>,
    commit: Option<&'a str>,
}

/// Indexes symbol names / file paths and the run→symbol / run→file edges so a
/// target can be a benchmark id, a symbol name, or a file path.
struct BenchQueryIndex<'a> {
    symbol_id_by_name: std::collections::BTreeMap<&'a str, &'a str>,
    symbol_file_by_id: std::collections::BTreeMap<&'a str, &'a str>,
    symbol_name_by_id: std::collections::BTreeMap<&'a str, &'a str>,
    file_path_by_id: std::collections::BTreeMap<&'a str, &'a str>,
    mentions_symbol: Vec<(&'a str, &'a str)>,
    touched_file: Vec<(&'a str, &'a str)>,
}

impl<'a> BenchQueryIndex<'a> {
    fn build(records: &'a [GraphRecord]) -> Self {
        let mut index = Self {
            symbol_id_by_name: std::collections::BTreeMap::new(),
            symbol_file_by_id: std::collections::BTreeMap::new(),
            symbol_name_by_id: std::collections::BTreeMap::new(),
            file_path_by_id: std::collections::BTreeMap::new(),
            mentions_symbol: Vec::new(),
            touched_file: Vec::new(),
        };
        for record in records {
            match record {
                GraphRecord::Node {
                    id,
                    kind: NodeKind::Symbol,
                    name: Some(name),
                    repo_relative_path,
                    ..
                } => {
                    // First writer wins (deterministic).
                    index
                        .symbol_id_by_name
                        .entry(name.as_str())
                        .or_insert(id.as_str());
                    index.symbol_name_by_id.insert(id.as_str(), name.as_str());
                    if let Some(path) = repo_relative_path.as_deref() {
                        index.symbol_file_by_id.insert(id.as_str(), path);
                    }
                }
                GraphRecord::Node {
                    id,
                    kind: NodeKind::File,
                    repo_relative_path: Some(path),
                    ..
                } => {
                    index.file_path_by_id.insert(id.as_str(), path.as_str());
                }
                GraphRecord::Edge {
                    label: EdgeLabel::MentionsSymbol,
                    source,
                    target,
                    ..
                } => index
                    .mentions_symbol
                    .push((source.as_str(), target.as_str())),
                GraphRecord::Edge {
                    label: EdgeLabel::TouchedFile,
                    source,
                    target,
                    ..
                } => index.touched_file.push((source.as_str(), target.as_str())),
                _ => {}
            }
        }
        index
    }
}

/// Reads `(executed_at, temporal.git_commit)` off a `BenchmarkRun` node.
fn run_time_fields(record: &GraphRecord) -> (Option<&str>, Option<&str>) {
    match record {
        GraphRecord::Node {
            executed_at,
            temporal,
            ..
        } => (
            executed_at.as_deref(),
            temporal.as_ref().map(|t| t.git_commit.as_str()),
        ),
        _ => (None, None),
    }
}

/// Whether the run's benchmark id matches the target directly.
fn id_matches(summary: Option<&BenchmarkSummaryView>, record: &GraphRecord, target: &str) -> bool {
    if let Some(view) = summary
        && view.benchmark_id == target
    {
        return true;
    }
    // Fallback for records ingested without a normalized summary: the node
    // name is the benchmark id (the capture lane stamps it).
    matches!(record, GraphRecord::Node { name: Some(n), .. } if n == target)
}

/// Emits the standard no-match envelope and exits 2.
fn exit_no_match(target: &str) -> ! {
    let envelope = serde_json::json!({
        "ok": false,
        "error": { "code": "no_match", "benchmark": target }
    });
    // Serialization of a `json!` literal with a string target is infallible in
    // practice; the static fallback keeps the no-match contract total.
    let text = serde_json::to_string(&envelope)
        .unwrap_or_else(|_| r#"{"ok":false,"error":{"code":"no_match"}}"#.to_owned());
    println!("{text}");
    std::process::exit(2);
}

/// Handles `eg query bench <target>` — the latest `BenchmarkRun` for a
/// benchmark id, symbol name, or file path, with a citable handle.
///
/// Temporal selectors pin the answer: `--at <commit-prefix>` keeps runs whose
/// captured commit starts with the prefix; `--as-of <RFC3339>` keeps runs at
/// or before the instant. Without either, the latest run wins.
#[allow(clippy::too_many_lines)]
pub(crate) fn query_bench_cmd(
    records: &[GraphRecord],
    target: &str,
    at: Option<&str>,
    as_of: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    let as_of_instant = as_of.map(|instant| {
        chrono::DateTime::parse_from_rfc3339(instant).unwrap_or_else(|_| {
            eprintln!(r#"{{"code":"invalid_field", "field":"as_of"}}"#);
            std::process::exit(1);
        })
    });

    let index = BenchQueryIndex::build(records);

    // ── Candidate runs ───────────────────────────────────────────────────────
    let mut candidates: Vec<BenchCandidate> = Vec::new();
    for record in records {
        let GraphRecord::Node {
            kind: NodeKind::BenchmarkRun,
            ..
        } = record
        else {
            continue;
        };
        let summary = benchmark_summary_of(record);
        let (executed_at, commit) = run_time_fields(record);
        let run_id = record.id();

        let matched = id_matches(summary.as_ref(), record, target)
            || index.mentions_symbol.iter().any(|(source, symbol_id)| {
                *source == run_id
                    && index
                        .symbol_name_by_id
                        .get(symbol_id)
                        .is_some_and(|n| *n == target)
            })
            || index.touched_file.iter().any(|(source, file_id)| {
                *source == run_id
                    && index
                        .file_path_by_id
                        .get(file_id)
                        .is_some_and(|p| *p == target)
            });
        if !matched {
            continue;
        }

        // ── Temporal selectors ───────────────────────────────────────────────
        if let Some(prefix) = at
            && !commit.is_some_and(|c| c.starts_with(prefix))
        {
            continue;
        }
        if let Some(cutoff) = as_of_instant {
            let run_instant =
                executed_at.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok());
            if run_instant.is_none_or(|instant| instant > cutoff) {
                continue;
            }
        }

        candidates.push(BenchCandidate {
            record,
            summary,
            executed_at,
            commit,
        });
    }

    if candidates.is_empty() {
        exit_no_match(target);
    }

    // Latest run wins: greatest executed_at, then greatest record id —
    // deterministic when two runs share an instant.
    candidates.sort_by(|a, b| {
        a.executed_at
            .cmp(&b.executed_at)
            .then_with(|| a.record.id().cmp(b.record.id()))
    });
    let latest = candidates.last().unwrap_or_else(|| exit_no_match(target));
    let run_count = candidates.len();

    // ── Answer ───────────────────────────────────────────────────────────────
    let GraphRecord::Node {
        id: record_id,
        summary: human,
        source_artifact_path,
        source_artifact_hash,
        ..
    } = latest.record
    else {
        return Err(anyhow::anyhow!("query bench: matched record is not a node"));
    };

    let symbol = index
        .mentions_symbol
        .iter()
        .find(|(source, _)| *source == record_id.as_str())
        .and_then(|(_, symbol_id)| {
            let name = index.symbol_name_by_id.get(symbol_id)?;
            Some(serde_json::json!({
                "id": symbol_id,
                "name": name,
                "file": index.symbol_file_by_id.get(symbol_id),
            }))
        });

    let (benchmark_id, mean_ns, unit, verdict, delta_pct) = latest.summary.as_ref().map_or_else(
        || {
            (
                String::new(),
                None,
                "ns".to_owned(),
                "unknown".to_owned(),
                None,
            )
        },
        |view| {
            (
                view.benchmark_id.clone(),
                Some(view.mean_ns),
                view.unit.clone(),
                view.verdict.clone(),
                view.delta_pct,
            )
        },
    );

    match format {
        OutputFormat::Text => {
            let estimate = mean_ns.map_or_else(|| "?".to_owned(), |m| format!("{m} {unit}"));
            println!(
                "benchmark {benchmark_id}: mean {estimate}, {verdict} ({})",
                record_id.as_str()
            );
        }
        OutputFormat::Json => {
            // The `symbol` key is present only when the run is anchored to a
            // symbol; unanchored runs omit it rather than emitting null.
            let mut answer = serde_json::json!({
                "ok": true,
                "target": target,
                "benchmark_id": benchmark_id,
                "record_id": record_id,
                "commit": latest.commit,
                "artifact_path": source_artifact_path,
                "artifact_hash": source_artifact_hash,
                "executed_at": latest.executed_at,
                "estimate_ns": mean_ns,
                "unit": unit,
                "measurement": "mean",
                "verdict": verdict,
                "delta_pct": delta_pct,
                "summary": human,
                "run_count": run_count,
            });
            if let (Some(map), Some(symbol)) = (answer.as_object_mut(), symbol) {
                map.insert("symbol".to_owned(), symbol);
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&answer)
                    .context("failed to serialize bench answer")?
            );
        }
    }
    Ok(())
}
