use super::*;

/// Exports every persisted graph record from an embedded store as canonical
/// newline-delimited JSON (issue #155) — the inverse of `eg ingest`, distinct
/// from the issue #68 evidence bundle.
///
/// Reads the store directly (no daemon, no network, no embeddings) and writes
/// the full record set — nodes, edges, valid-time tombstones, diagnostics,
/// superseded versions, and unknown-version records — across every domain, in
/// the exact record shapes `eg scan` / `eg ingest` emit.
pub(crate) fn export(data_dir: &Path, out: &Path) -> Result<()> {
    export_embedded_store(data_dir, out)
}

/// Exports an embedded `--data-dir` store to canonical JSONL (issue #155).
///
/// Read surface. Export reads the same physical inventory
/// `eg inspect --data-dir` uses — `EmbeddedAletheiaSink::inspect_all_records`,
/// through the throwaway read-only store copy `readonly_audit_store` makes — so
/// the record set is identical to inspect's by construction: no temporal
/// deduplication, tombstones and superseded versions included, unknown
/// `(domain, kind, schema_version)` tuples preserved.
///
/// Forget suppression. The one deliberate departure from the raw physical
/// inventory: a record hidden by a transaction-time `eg forget` retraction
/// (issue #231) is never re-emitted — re-exporting its body would resurface
/// exactly the bytes forget hid, a redaction leak. A forget retraction is
/// identified by its `Retraction` event node (`source_handle` names the
/// retracted record) and its deterministic retraction tombstone
/// (`retraction_tombstone_id`); the retracted ORIGINAL is dropped while the
/// audit trail — the `Retraction` event and its `Tombstone` — is preserved.
/// Valid-time tombstones from history replay carry neither signal and are kept
/// (a legitimate historical fact that must round-trip). Every physical version
/// of a retracted stable ID is dropped (fail-closed on privacy), so a
/// forget-then-re-observe log signature is over-suppressed rather than leaked;
/// this is the single enumerated deviation from "every physical record".
///
/// Determinism. Lines are sorted and joined exactly as `Graph::to_jsonl` does
/// (sort, `\n` join, single trailing newline), and the output carries no
/// header, manifest, run id, or timestamp, so repeated exports of an unchanged
/// store are byte-identical.
///
/// The read is strictly read-only (the throwaway copy is never re-persisted to
/// the original), the store is fully read before the output file is written (a
/// mid-read failure leaves no partial file), and a missing / empty / non-store
/// `--data-dir` fails with a diagnostic naming the path before any write.
#[cfg(feature = "embedded-aletheiadb")]
pub(crate) fn export_embedded_store(data_dir: &Path, out: &Path) -> Result<()> {
    // Validate BEFORE opening the engine or touching the output file: a missing,
    // empty, or unreadable data dir fails naming the path and writes nothing.
    validate_existing_embedded_store(data_dir)?;

    let (store_root, _readonly_guard) = readonly_audit_store(data_dir)?;
    let sink = EmbeddedAletheiaSink::open_unleased(&store_root)
        .with_context(|| format!("failed to open embedded store {}", data_dir.display()))?;
    let report = sink.inspect_all_records().map_err(|error| {
        anyhow::anyhow!(
            "failed to read embedded store {} for export: {error}",
            data_dir.display()
        )
    })?;

    // A store directory can hold engine index/runtime files while containing
    // zero Egregore records (empty ingest, or a non-Egregore AletheiaDB dir).
    // That is a wrong-store diagnostic naming the path, never a valid empty
    // export (mirrors `eg inspect --data-dir`, issue #125). Guard before writing.
    if report.records.is_empty() && report.unknown_schema_versions.is_empty() {
        anyhow::bail!(
            "error: embedded store at {} contains no Egregore records - \
             run `eg ingest --adapter embedded --data-dir <path>` first",
            data_dir.display()
        );
    }

    let forget_retracted = forget_retracted_ids(&report.records);

    let mut lines: Vec<String> = Vec::new();
    for record in &report.records {
        if forget_retracted.contains(record.id()) {
            continue;
        }
        lines.push(
            serde_json::to_string(record)
                .with_context(|| format!("failed to serialize record {}", record.id()))?,
        );
    }

    // Unknown-version records re-emit their reconstructed canonical line
    // verbatim. A record that could not be reconstructed from stored properties
    // (a required property is absent — only reachable by artificial raw
    // injection, never by `eg ingest`) is surfaced as an enumerated skip
    // diagnostic on stderr, never silently dropped.
    let mut unreconstructable: Vec<String> = Vec::new();
    for unknown in &report.unknown_schema_versions {
        match &unknown.raw_line {
            Some(line) => lines.push(line.clone()),
            None => unreconstructable.push(format!(
                "{}:{}:{}",
                unknown.version.domain, unknown.version.kind, unknown.version.version
            )),
        }
    }

    // Canonical ordering mirrors `Graph::to_jsonl`: sort the final serialized
    // lines, join with `\n`, single trailing newline. Byte-stable regardless of
    // physical store iteration order.
    lines.sort_unstable();
    let content = format!("{}\n", lines.join("\n"));

    // Full read completed; only now write, so a mid-read error never leaves a
    // partial output file.
    fs::write(out, &content)
        .with_context(|| format!("failed to write export to {}", out.display()))?;

    if !unreconstructable.is_empty() {
        unreconstructable.sort_unstable();
        for tuple in &unreconstructable {
            eprintln!(
                "warning: skipped unknown-version record that could not be \
                 reconstructed from stored properties ({tuple})"
            );
        }
    }

    println!("exported {} records to {}", lines.len(), out.display());
    Ok(())
}

/// Collects the stable IDs of records suppressed by an `eg forget` retraction
/// (issue #231), so export never re-emits a transaction-time-retracted body.
///
/// Two independent signals, unioned so a partially-written retraction (event
/// without tombstone, or vice versa) still fails closed:
///
/// * a `Retraction` event node whose `source_handle` names the retracted
///   record, and
/// * a retraction tombstone, recognized by its deterministic
///   [`crate::forget::retraction_tombstone_id`] identity over its `deleted_id`.
///
/// Valid-time (history-replay) tombstones carry neither signal — their IDs are
/// not the retraction-tombstone hash and no `Retraction` event names them — so
/// the removed historical nodes they mark are preserved.
#[cfg(feature = "embedded-aletheiadb")]
fn forget_retracted_ids(records: &[GraphRecord]) -> std::collections::BTreeSet<String> {
    let mut retracted = std::collections::BTreeSet::new();
    for record in records {
        match record {
            GraphRecord::Node {
                kind: NodeKind::Retraction,
                source_handle: Some(handle),
                ..
            } => {
                retracted.insert(handle.clone());
            }
            GraphRecord::Tombstone { id, deleted_id, .. }
                if *id == crate::forget::retraction_tombstone_id(deleted_id).0 =>
            {
                retracted.insert(deleted_id.clone());
            }
            _ => {}
        }
    }
    retracted
}

/// Exports a definitions-only SCIP code-intelligence index (issue #233).
///
/// Reads a graph JSONL (`--graph`) or an embedded store (`--data-dir`),
/// strictly read-only and offline, maps every span-bearing `Symbol`/`Module`
/// node to a SCIP `SymbolInformation` + definition `Occurrence`, and writes the
/// encoded protobuf to `out`. Positionless edges, `Diagnostic` stubs, span-less
/// nodes, and anonymous `impl` blocks are dropped and reported (AC#7). The
/// output is byte-identical across runs. See `docs/cli/export-scip.md`.
pub(crate) fn export_scip(graph: Option<&Path>, data_dir: Option<&Path>, out: &Path) -> Result<()> {
    let records = match (graph, data_dir) {
        (Some(path), None) => load_records_from_jsonl(path)?,
        (None, Some(dir)) => load_records_from_data_dir_readonly(dir)?,
        (Some(_), Some(_)) => {
            anyhow::bail!("provide only one of --graph or --data-dir, not both")
        }
        (None, None) => anyhow::bail!("provide --graph <path> or --data-dir <path>"),
    };

    let project_root = crate::scip::package_name(&records);
    let export = crate::scip::build_index(&records, &project_root, env!("CARGO_PKG_VERSION"));
    let bytes = crate::scip::encode_index(&export.index)?;

    fs::write(out, &bytes)
        .with_context(|| format!("failed to write SCIP index to {}", out.display()))?;

    let skipped = export.skipped;
    println!(
        "exported {} definitions across {} documents to {}; \
         skipped {} nodes ({} no-span, {} diagnostic-stubs, {} impl-blocks)",
        export.definition_count,
        export.document_count,
        out.display(),
        skipped.no_span + skipped.diagnostic + skipped.impl_block,
        skipped.no_span,
        skipped.diagnostic,
        skipped.impl_block,
    );
    Ok(())
}

/// Feature-off stub: embedded-store export needs the embedded adapter.
#[cfg(not(feature = "embedded-aletheiadb"))]
pub(crate) fn export_embedded_store(data_dir: &Path, _out: &Path) -> Result<()> {
    anyhow::bail!(
        "exporting {} requires the 'embedded-aletheiadb' feature",
        data_dir.display()
    )
}
