//! Embedded-store write-lease contention contract (issue #200).
//!
//! ADR-0003 commits the embedded adapter to a local write lock so a swarm of
//! embedded writers against one data dir can never silently lose or corrupt a
//! record. These tests pin the contract: a concurrent embedded write either
//! durably persists with intact provenance or is refused with the structured
//! `store_contended` error, and the post-race store is byte-identical to a
//! single-writer baseline.
#![allow(missing_docs)]
#![cfg(feature = "embedded-aletheiadb")]

use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

use aletheia_egregore::{
    EdgeLabel, EvidenceLink, GraphRecord, NodeKind, SourceSpan,
    adapters::{
        AdapterError, EmbeddedAletheiaSink, STORE_CONTENDED_CODE, ingest_records,
        records_from_jsonl,
    },
    daemon::StoreLease,
    evidence::{EvidenceProvenance, ObservationRequest, build_observation_records},
    stable_id,
};
use assert_cmd::Command;
use predicates::prelude::*;

/// Number of concurrent embedded writers in the contention fixture (issue #200
/// success metric requires N >= 8).
const WRITERS: usize = 8;
/// Number of concurrent races compared against the single-writer baseline.
const RUNS: usize = 5;
/// Failsafe ceiling for a starved writer; generous because the loop is
/// progress-guaranteed (some writer always holds the lease and completes).
const WRITER_DEADLINE: Duration = Duration::from_secs(120);

fn seed_symbol_id() -> String {
    stable_id(&["node", "symbol", "src/lib.rs", "contended"])
}

/// Deterministic code-graph records every observation cites.
fn seed_records() -> Vec<GraphRecord> {
    let file_id = stable_id(&["node", "file", "src/lib.rs"]);
    let symbol_id = seed_symbol_id();
    vec![
        GraphRecord::node(
            file_id.clone(),
            NodeKind::File,
            Some("src/lib.rs".to_owned()),
            None,
            Some("src/lib.rs".to_owned()),
            "Rust source file src/lib.rs".to_owned(),
        ),
        GraphRecord::symbol(
            symbol_id.clone(),
            "function",
            "src/lib.rs".to_owned(),
            SourceSpan {
                start_byte: 0,
                end_byte: 20,
                start_line: 1,
                end_line: 1,
            },
            "contended".to_owned(),
            "contended target symbol".to_owned(),
        ),
        GraphRecord::edge(
            EdgeLabel::Defines,
            file_id,
            symbol_id,
            Some("1.0".to_owned()),
            "defines contended symbol".to_owned(),
        ),
    ]
}

/// One provenance-complete observation write batch for `writer`.
///
/// Record IDs are content-addressed, but the attached `Producer` envelope
/// stamps a wall-clock `producer_started_at`, so a batch is built exactly once
/// and shared by the baseline, every race, and the post-race assertions.
fn observation_batch(writer: usize) -> (String, Vec<GraphRecord>) {
    let req = ObservationRequest {
        provenance: EvidenceProvenance {
            agent_id: format!("agent-{writer}"),
            agent_kind: "other".to_owned(),
            session_id: format!("session-{writer}"),
            observed_at: "2026-07-01T00:00:00Z".to_owned(),
            source_handle: Some(format!("transcripts/agent-{writer}.md")),
        },
        text: format!("agent {writer} confirmed the contended symbol handles empty input"),
        confidence: 0.9,
        evidence_links: vec![EvidenceLink {
            target_record_id: Some(seed_symbol_id()),
            target_domain: "codegraph".to_owned(),
            relation: "OBSERVES".to_owned(),
            confidence: "0.9".to_owned(),
            as_of_commit: None,
            target_repo_relative_path: None,
            target_span: None,
            target_git_commit: None,
        }],
    };
    let outcome = build_observation_records(&req).expect("observation batch should build");
    (outcome.record_id, outcome.records)
}

fn seed_store(data_dir: &Path) {
    let mut sink = EmbeddedAletheiaSink::open(data_dir).expect("seed store should open");
    let report = ingest_records(&seed_records(), &mut sink);
    assert!(report.is_success(), "seed ingest must succeed: {report:?}");
    sink.persist_indexes().expect("seed store should persist");
}

/// Writes all batches sequentially through one sink: the single-writer baseline.
fn write_baseline(data_dir: &Path, batches: &[(String, Vec<GraphRecord>)]) {
    seed_store(data_dir);
    let mut sink = EmbeddedAletheiaSink::open(data_dir).expect("baseline store should reopen");
    for (_, records) in batches {
        let report = ingest_records(records, &mut sink);
        assert!(
            report.is_success(),
            "baseline ingest must succeed: {report:?}"
        );
    }
    sink.persist_indexes()
        .expect("baseline store should persist");
}

/// Canonical, order-independent byte serialization of a store's records.
fn canonical_store_bytes(data_dir: &Path) -> String {
    let sink = EmbeddedAletheiaSink::open(data_dir).expect("store should reopen for read-back");
    let records = sink
        .read_all_records()
        .expect("post-race store must stay readable");
    let mut lines = records
        .iter()
        .map(|record| serde_json::to_string(record).expect("record should serialize"))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines.join("\n")
}

/// Referential-integrity assertion: every edge endpoint and evidence-link
/// target in the store resolves to a record present in the store.
fn assert_referential_integrity(records: &[GraphRecord]) {
    let ids = records
        .iter()
        .map(|record| (record.id().to_owned(), record))
        .collect::<BTreeMap<_, _>>();
    for record in records {
        match record {
            GraphRecord::Edge { source, target, .. } => {
                assert!(
                    ids.contains_key(source),
                    "edge {} has dangling source {source}",
                    record.id()
                );
                assert!(
                    ids.contains_key(target),
                    "edge {} has dangling target {target}",
                    record.id()
                );
            }
            GraphRecord::Node { evidence_links, .. } => {
                for link in evidence_links.iter().flatten() {
                    if let Some(target_id) = &link.target_record_id {
                        assert!(
                            ids.contains_key(target_id),
                            "record {} cites missing evidence target {target_id}",
                            record.id()
                        );
                    }
                }
            }
            GraphRecord::Tombstone { .. } => {}
        }
    }
}

/// Races `WRITERS` concurrent embedded writers against one data dir.
///
/// Synchronization is a start barrier plus the write lease itself — no sleeps.
/// Every attempt must either persist durably or be refused with the structured
/// contention error; any other failure is a contract violation.
fn race_concurrent_writers(data_dir: &Path, batches: &[(String, Vec<GraphRecord>)]) -> usize {
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut handles = Vec::new();
    for (writer, (_, batch)) in batches.iter().enumerate() {
        let barrier = Arc::clone(&barrier);
        let data_dir = data_dir.to_path_buf();
        let records = batch.clone();
        handles.push(thread::spawn(move || -> usize {
            let deadline = Instant::now() + WRITER_DEADLINE;
            let mut contended = 0_usize;
            barrier.wait();
            loop {
                match EmbeddedAletheiaSink::open(&data_dir) {
                    Ok(mut sink) => {
                        let report = ingest_records(&records, &mut sink);
                        assert!(
                            report.is_success(),
                            "writer {writer} ingest must succeed once the lease is held: {report:?}"
                        );
                        sink.persist_indexes()
                            .expect("writer store should persist durably");
                        return contended;
                    }
                    Err(AdapterError::Contended { message, .. }) => {
                        assert!(
                            message.contains("retry"),
                            "contention error must name the retry remedy: {message}"
                        );
                        assert!(
                            message.contains("--adapter daemon"),
                            "contention error must name the daemon remedy: {message}"
                        );
                        contended += 1;
                        assert!(
                            Instant::now() < deadline,
                            "writer {writer} starved beyond the failsafe deadline"
                        );
                        thread::yield_now();
                    }
                    Err(other) => panic!(
                        "a concurrent embedded open may fail only with the structured \
                         contention error, got: {other}"
                    ),
                }
            }
        }));
    }
    handles
        .into_iter()
        .map(|handle| handle.join().expect("writer thread must not panic"))
        .sum()
}

/// AC1 + AC4 + success metric: N concurrent provenance-complete writes lose
/// nothing, corrupt nothing, keep the store queryable and referentially
/// intact, and reconstruct byte-identically to a single-writer baseline
/// across `RUNS` races.
#[test]
fn concurrent_embedded_writers_never_lose_or_corrupt_records() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let batches = (0..WRITERS).map(observation_batch).collect::<Vec<_>>();
    let baseline_dir = temp.path().join("baseline-store");
    write_baseline(&baseline_dir, &batches);
    let baseline_bytes = canonical_store_bytes(&baseline_dir);

    for run in 0..RUNS {
        let race_dir = temp.path().join(format!("race-store-{run}"));
        seed_store(&race_dir);
        race_concurrent_writers(&race_dir, &batches);

        let sink = EmbeddedAletheiaSink::open(&race_dir).expect("post-race store should reopen");
        let records = sink
            .read_all_records()
            .expect("post-race store must stay queryable");
        for (writer, (record_id, written)) in batches.iter().enumerate() {
            let stored = records
                .iter()
                .find(|record| record.id() == record_id)
                .unwrap_or_else(|| {
                    panic!("run {run}: writer {writer} observation {record_id} was lost")
                });
            let original = written
                .iter()
                .find(|record| record.id() == record_id)
                .expect("batch must contain its own observation");
            assert_eq!(
                stored, original,
                "run {run}: writer {writer} observation must survive with intact provenance"
            );
        }
        assert_referential_integrity(&records);
        drop(sink);

        assert_eq!(
            canonical_store_bytes(&race_dir),
            baseline_bytes,
            "run {run}: post-race store must be byte-identical to the single-writer baseline"
        );
    }
}

/// AC2: a second embedded writer is refused while a live embedded peer holds
/// the store, with the typed contention error, and recovers after release.
#[test]
fn second_embedded_writer_is_refused_then_recovers_after_release() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("peer-held-store");
    let first = EmbeddedAletheiaSink::open(&data_dir).expect("first embedded open should succeed");

    let error = EmbeddedAletheiaSink::open(&data_dir)
        .err()
        .expect("second concurrent embedded open must be refused");
    let AdapterError::Contended {
        data_dir: contended_dir,
        message,
    } = &error
    else {
        panic!("live-peer contention must be typed AdapterError::Contended, got: {error:?}");
    };
    assert_eq!(contended_dir, &data_dir.display().to_string());
    assert!(
        message.contains("--adapter daemon") && message.contains("retry"),
        "contention error must name both remedies (daemon, retry): {message}"
    );
    assert!(
        error.to_string().starts_with(STORE_CONTENDED_CODE),
        "contention display must carry the stable {STORE_CONTENDED_CODE} code: {error}"
    );

    drop(first);
    EmbeddedAletheiaSink::open(&data_dir)
        .expect("embedded open must succeed after the writer releases the lease");
}

/// AC1/AC2 at the CLI surface: `eg ingest --adapter embedded` against a held
/// store emits the structured `{"ok": false, "error": ...}` contention
/// envelope and performs no write.
#[test]
fn embedded_ingest_cli_reports_structured_contention_envelope() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("cli-held-store");
    let graph_path = temp.path().join("graph.jsonl");
    let jsonl = seed_records()
        .iter()
        .map(|record| serde_json::to_string(record).expect("record should serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&graph_path, jsonl + "\n").expect("graph JSONL should write");
    let _lease = StoreLease::acquire(&data_dir).expect("test should hold the store lease");

    Command::cargo_bin("egregore")
        .expect("binary should run")
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .assert()
        .failure()
        .stdout(predicate::str::contains(r#""ok":false"#))
        .stdout(predicate::str::contains(r#""code":"store_contended""#))
        .stdout(predicate::str::contains("--adapter daemon"))
        .stderr(predicate::str::contains("store_contended"));

    // Refusal must be write-free: the held data dir gained no store files.
    let wrote_store = fs::read_dir(&data_dir).is_ok_and(|mut entries| entries.next().is_some());
    assert!(
        !wrote_store,
        "a contended embedded ingest must not perform a partial write"
    );
}

/// AC5: the write lease never blocks reads — the documented read model takes a
/// snapshot copy of the store, which opens fine while the live writer holds
/// the original's lease.
#[test]
fn writer_lease_does_not_block_snapshot_reads() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let data_dir = temp.path().join("read-under-write-store");
    seed_store(&data_dir);

    let live_writer =
        EmbeddedAletheiaSink::open(&data_dir).expect("live writer should hold the store");

    let snapshot = temp.path().join("read-snapshot");
    copy_dir_recursive(&data_dir, &snapshot).expect("snapshot copy should succeed");
    let reader = EmbeddedAletheiaSink::open(&snapshot)
        .expect("snapshot read must not be blocked by the live writer's lease");
    let records = reader
        .read_all_records()
        .expect("snapshot store should read");
    assert!(
        records.iter().any(|record| record.id() == seed_symbol_id()),
        "snapshot read must return the seeded records"
    );
    drop(live_writer);
}

/// Round-trip guard: a graph JSONL written by this fixture parses back.
#[test]
fn contention_fixture_graph_round_trips() {
    let jsonl = seed_records()
        .iter()
        .map(|record| serde_json::to_string(record).expect("record should serialize"))
        .collect::<Vec<_>>()
        .join("\n");
    let parsed = records_from_jsonl(&jsonl).expect("fixture JSONL should parse");
    assert_eq!(parsed.len(), seed_records().len());
}

/// Mirrors the CLI's read-only snapshot copy (regular files and dirs only).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}
