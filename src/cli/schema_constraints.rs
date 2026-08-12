//! `eg audit schema-constraints` — the issue #486 commit-time backstop surface.
//!
//! Three actions over one embedded store:
//!
//! * **report** (default) — strictly read-only. Inventories the labels the
//!   embedded adapter can write, reconciles that against what the store holds,
//!   and runs the upstream `.dry_run()` conformance scan of the chosen profile.
//!   Reads through a throwaway copy of the store, exactly like `eg inspect
//!   --data-dir` and every other read-only audit, so it takes no write lease and
//!   cannot re-persist a byte of the original.
//! * **declare** (`--declare`) — the opt-in Phase 2 action. Opens the REAL store
//!   with the exclusive write lease and declares the profile on every writable
//!   label.
//! * **drop** (`--drop`) — retracts every declared constraint, so the Phase 2
//!   decision is reversible.
//!
//! The report is a single deterministic JSON line, byte-identical across runs on
//! an unchanged store, carrying only labels, property keys, type tokens, counts,
//! upstream reason strings, and Egregore record-ID handles — never a record's
//! payload text and never an engine-internal entity id.

use std::path::Path;

use anyhow::Result;

use crate::schema_constraints::{ConstraintAction, ConstraintProfile, SchemaConstraintReport};
// Scanning/rendering types are reachable only from the embedded lane; importing
// them unconditionally would warn in a `--no-default-features` build, where the
// command is a usage-error stub.
#[cfg(feature = "embedded-aletheiadb")]
use crate::schema_constraints::{
    ConformanceStatus, EntityKindToken, LabelConformance, unknown_edge_types, unknown_node_labels,
    writable_edge_types, writable_node_labels,
};

use super::OutputFormat;

/// Prints a redaction-safe JSON error on stderr and exits with the usage code.
fn usage_exit(code: &str, message: &str) -> ! {
    eprintln!(
        "{}",
        serde_json::json!({ "ok": false, "code": code, "message": message })
    );
    std::process::exit(2);
}

/// Entry point for `eg audit schema-constraints`.
///
/// # Errors
///
/// Returns an error only for failures the caller should surface as-is; every
/// usage and load failure exits directly with code 2 so the diagnostic shape
/// matches the rest of the `eg audit` family.
pub(crate) fn audit_schema_constraints_cmd(
    data_dir: &Path,
    profile: &str,
    declare: bool,
    drop: bool,
    include_foreign: bool,
    format: OutputFormat,
) -> Result<()> {
    if declare && drop {
        usage_exit(
            "unsupported_combination",
            "--declare and --drop are mutually exclusive",
        );
    }
    let Some(profile) = ConstraintProfile::parse(profile) else {
        usage_exit(
            "unknown_profile",
            &format!(
                "unknown profile {profile:?}; known profiles: {}",
                ConstraintProfile::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    };

    let action = if declare {
        ConstraintAction::Declare
    } else if drop {
        ConstraintAction::Drop
    } else {
        ConstraintAction::Report
    };

    if include_foreign && !drop {
        usage_exit(
            "unsupported_combination",
            "--include-foreign only applies to --drop",
        );
    }

    run(data_dir, profile, action, include_foreign, format)
}

#[cfg(feature = "embedded-aletheiadb")]
fn run(
    data_dir: &Path,
    profile: ConstraintProfile,
    action: ConstraintAction,
    include_foreign: bool,
    format: OutputFormat,
) -> Result<()> {
    let mut report = SchemaConstraintReport {
        action,
        data_dir: data_dir.display().to_string(),
        profile,
        rows: Vec::new(),
        unknown_node_labels: Vec::new(),
        unknown_edge_types: Vec::new(),
        declared_constraints: Vec::new(),
        declared_labels: 0,
        declaration_refusal: None,
        dropped_labels: 0,
        dropped_constraints: Vec::new(),
        foreign_constraints_retained: Vec::new(),
        conformance_evaluated: false,
    };

    match action {
        ConstraintAction::Report => {
            // Strictly read-only: the scan runs against a throwaway copy, so the
            // original store is never opened for writing and never re-persisted.
            let (store_root, _guard) = super::readonly_audit_store(data_dir)
                .unwrap_or_else(|error| usage_exit("store_unreadable", &error.to_string()));
            let sink = open_store(&store_root, data_dir, false);
            collect_conformance(&sink, profile, &mut report);
            report.declared_constraints = sink.declared_schema_constraints();
        }
        ConstraintAction::Declare => {
            validate_store_path(data_dir);
            // The write lease is required: declaring persists a sidecar into the
            // real store, so it must contend with any other live writer.
            let sink = open_store(data_dir, data_dir, true);
            // Report conformance first, so a refusal is explained by the same
            // rows the read-only report would have shown.
            collect_conformance(&sink, profile, &mut report);
            if !report.ok() {
                report.declared_constraints = sink.declared_schema_constraints();
                finish(&mut report, format, 1);
            }
            let nodes = writable_node_labels();
            let edges = writable_edge_types();
            let node_outcome =
                sink.declare_schema_constraints(profile, EntityKindToken::Node, &nodes);
            report.declared_labels = node_outcome.declared;
            report.declaration_refusal = node_outcome.refusal;
            if report.declaration_refusal.is_none() {
                let edge_outcome =
                    sink.declare_schema_constraints(profile, EntityKindToken::Edge, &edges);
                report.declared_labels += edge_outcome.declared;
                report.declaration_refusal = edge_outcome.refusal;
            }
            // Declaration is not atomic across labels, so a refusal can leave the
            // store half-constrained. Always read the store's real posture back
            // and PRINT the full report: an operator who cannot see which labels
            // landed cannot clean up, and a bare stderr line would hide it.
            report.declared_constraints = sink.declared_schema_constraints();
        }
        ConstraintAction::Drop => {
            validate_store_path(data_dir);
            let sink = open_store(data_dir, data_dir, true);
            let outcome = sink
                .drop_schema_constraints(include_foreign)
                .unwrap_or_else(|error| usage_exit("drop_failed", &error.to_string()));
            report.dropped_labels = outcome.dropped.len();
            // The before-image: upstream atomically rewrites the sidecar on every
            // drop, so without this a mistaken `--drop` is unrecoverable by
            // inspection - you cannot re-declare what you can no longer read.
            report.dropped_constraints = outcome.dropped;
            report.foreign_constraints_retained = outcome.foreign_retained;
            report.declared_constraints = sink.declared_schema_constraints();
        }
    }

    let code = i32::from(!report.ok());
    finish(&mut report, format, code);
}

/// Canonicalizes, prints, and exits with `code`.
///
/// Every terminal path goes through here so the full report is ALWAYS printed
/// before exit - including a refused or partial `--declare`, where the report is
/// the only record of which labels actually landed.
#[cfg(feature = "embedded-aletheiadb")]
fn finish(report: &mut SchemaConstraintReport, format: OutputFormat, code: i32) -> ! {
    report.canonicalize();
    emit(report, format);
    std::process::exit(code);
}

/// Opens an embedded store, reporting failures against the ORIGINAL path.
///
/// The report action reads a throwaway copy, so `store_root` and the path the
/// operator typed differ; the diagnostic must name the latter. `leased` selects
/// the exclusive write lease: the read-only report must NOT take it (it would
/// contend with a live writer for no reason), while `--declare`/`--drop` must
/// (they persist the upstream sidecar into the real store).
#[cfg(feature = "embedded-aletheiadb")]
fn open_store(
    store_root: &Path,
    reported_path: &Path,
    leased: bool,
) -> crate::adapters::EmbeddedAletheiaSink {
    let opened = if leased {
        crate::adapters::EmbeddedAletheiaSink::open(store_root)
    } else {
        crate::adapters::EmbeddedAletheiaSink::open_unleased(store_root)
    };
    opened.unwrap_or_else(|error| {
        // A lease held by another live writer is its own documented condition
        // (issue #200) with its own remedy - route through the daemon, or retry.
        // Collapsing it into `store_unreadable` would bury that remedy in free
        // text under a misleading machine-readable code.
        let code = match error {
            crate::adapters::AdapterError::Contended { .. } => {
                crate::adapters::STORE_CONTENDED_CODE
            }
            _ => "store_unreadable",
        };
        usage_exit(
            code,
            &format!(
                "failed to open embedded store {}: {error}",
                reported_path.display()
            ),
        )
    })
}

/// Rejects a missing or non-store `--data-dir` before any write lease is taken.
#[cfg(feature = "embedded-aletheiadb")]
fn validate_store_path(data_dir: &Path) {
    if let Err(error) = super::validate_existing_embedded_store(data_dir) {
        usage_exit("store_unreadable", &error.to_string());
    }
}

/// Fills `report.rows` and the unknown-label sets from one open store.
///
/// Only labels the store actually HOLDS are scanned. Upstream's scan is
/// per-label and an edge-type scan walks every edge, so probing all 110
/// inventoried labels would cost a full pass each; a label with no entity yields
/// a zero-checked report that is synthesised here instead, which is identical in
/// content and free.
#[cfg(feature = "embedded-aletheiadb")]
fn collect_conformance(
    sink: &crate::adapters::EmbeddedAletheiaSink,
    profile: ConstraintProfile,
    report: &mut SchemaConstraintReport,
) {
    // A store that cannot be READ is a load error (exit 2), never a conformance
    // gate failure (exit 1): a CI job keying on exit 1 must never be told
    // "schema violations found" for what is actually an I/O failure.
    let (observed_nodes, observed_edges) = sink.observed_labels().unwrap_or_else(|error| {
        usage_exit(
            "store_unreadable",
            &format!("failed to read store schema: {error}"),
        )
    });

    report.unknown_node_labels = unknown_node_labels(&observed_nodes);
    report.unknown_edge_types = unknown_edge_types(&observed_edges);

    let present_nodes: std::collections::BTreeSet<&str> =
        observed_nodes.iter().map(String::as_str).collect();
    let present_edges: std::collections::BTreeSet<&str> =
        observed_edges.iter().map(String::as_str).collect();

    let scan_nodes: Vec<&str> = writable_node_labels()
        .into_iter()
        .filter(|label| present_nodes.contains(label))
        .collect();
    let scan_edges: Vec<&str> = writable_edge_types()
        .into_iter()
        .filter(|label| present_edges.contains(label))
        .collect();

    report.rows = sink
        .schema_constraint_dry_run(profile, EntityKindToken::Node, &scan_nodes)
        .unwrap_or_else(|error| {
            usage_exit(
                "conformance_scan_failed",
                &format!("node conformance scan failed: {error}"),
            )
        });
    report.rows.extend(
        sink.schema_constraint_dry_run(profile, EntityKindToken::Edge, &scan_edges)
            .unwrap_or_else(|error| {
                usage_exit(
                    "conformance_scan_failed",
                    &format!("edge conformance scan failed: {error}"),
                )
            }),
    );

    // Every inventoried label the store holds nothing of is reported
    // `not_present`, so the report enumerates the FULL writable surface and an
    // absent label is never silently indistinguishable from a conforming one.
    for label in writable_node_labels() {
        if !present_nodes.contains(label) {
            report
                .rows
                .push(LabelConformance::not_present(EntityKindToken::Node, label));
        }
    }
    for label in writable_edge_types() {
        if !present_edges.contains(label) {
            report
                .rows
                .push(LabelConformance::not_present(EntityKindToken::Edge, label));
        }
    }

    // A scanned label whose entities all conform, plus one that does not, must
    // be distinguishable at a glance; the status token already carries that, so
    // this is only a defensive check that the adapter never invented a status.
    debug_assert!(
        report
            .rows
            .iter()
            .all(|row| row.status != ConformanceStatus::NotPresent || row.checked == 0)
    );

    report.conformance_evaluated = true;
}

#[cfg(not(feature = "embedded-aletheiadb"))]
fn run(
    _data_dir: &Path,
    _profile: ConstraintProfile,
    _action: ConstraintAction,
    _include_foreign: bool,
    _format: OutputFormat,
) -> Result<()> {
    usage_exit(
        "embedded_adapter_unavailable",
        "eg audit schema-constraints requires the `embedded-aletheiadb` feature",
    );
}

#[cfg(feature = "embedded-aletheiadb")]
fn emit(report: &SchemaConstraintReport, format: OutputFormat) {
    match format {
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string(&report.to_json())
                    .unwrap_or_else(|error| usage_exit("render_failed", &error.to_string()))
            );
        }
        OutputFormat::Text => print!("{}", report.to_text()),
    }
}
