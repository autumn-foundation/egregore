//! Evaluation tests for issue #491: `AletheiaDB` 0.2.0's tamper-evident
//! provenance hash chain against Egregore's write/retraction shape.
//!
//! The decision (docs/adr/0006-tamper-evidence-provenance-chain.md) is that
//! `verify_chain` can back the #338 evidence-pack manifest's "this evidence
//! store has not been altered" claim. These tests pin the two load-bearing
//! compatibility facts that decision rests on:
//!
//! 1. Egregore-shaped writes — `create_node` / `create_edge` facts plus
//!    `eg forget`-shaped logical retraction (tombstone node + retraction
//!    event node; writes only, nothing is ever deleted) and an
//!    `eg forget-repo`-shaped bulk logical eviction (tombstones + one
//!    eviction event) — seal and verify cleanly, including across a
//!    close/reopen boundary where the unsealed tail is rebuilt from
//!    replayed history. A chain that flagged our own retractions as
//!    tampering would be worse than no chain; this is the regression
//!    guard for that.
//! 2. The manifest primitive round-trips: `export_chain_head` before close,
//!    `verify_chain_against` after reopen, proving the store append-only
//!    extends the anchored head (rollback/fork detection) without an
//!    O(history) pass.
//!
//! The store is opened through `aletheiadb`'s own public
//! `config::durable_config_for_data_dir` — the same call
//! `EmbeddedAletheiaSink::open_inner` uses — with only the chain config
//! added, so this exercises the exact write path the adapter uses
//! (`AletheiaDB::create_node` / `create_edge` route through the
//! chain-captured commit path in `db/transaction.rs`).

#![allow(missing_docs)]

#[cfg(feature = "embedded-aletheiadb")]
mod chain_compatibility {
    use std::path::Path;

    use aletheiadb::provenance_chain::{ChainConfig, ChainHead};

    /// Open the store the way the embedded adapter does, but with the 0.2.0
    /// provenance hash chain enabled — the configuration ADR 0006 evaluates.
    fn open_store(data_dir: &Path, fresh: bool) -> Result<aletheiadb::AletheiaDB, String> {
        let mut config = aletheiadb::config::durable_config_for_data_dir(data_dir);
        if fresh {
            // Mirror `EmbeddedAletheiaSink::open_inner`: nothing to load yet.
            config.persistence.load_on_startup = false;
        }
        config.chain = ChainConfig::enabled();
        aletheiadb::AletheiaDB::with_unified_config(config).map_err(|error| error.to_string())
    }

    fn props(pairs: &[(&str, &str)]) -> aletheiadb::PropertyMap {
        let mut builder = aletheiadb::PropertyMapBuilder::new();
        for (key, value) in pairs {
            builder = builder.insert(key, *value);
        }
        builder.build()
    }

    fn create_node(
        db: &aletheiadb::AletheiaDB,
        label: &str,
        pairs: &[(&str, &str)],
    ) -> Result<aletheiadb::NodeId, String> {
        db.create_node(label, props(pairs))
            .map_err(|error| error.to_string())
    }

    /// Egregore's write shape: code-fact nodes, an evidence edge, then
    /// logical retraction exactly as `eg forget` (#231) and `eg forget-repo`
    /// (#248) perform it — tombstone nodes plus retraction/eviction event
    /// nodes, all plain `create_node` writes; no version is ever deleted or
    /// mutated in place.
    ///
    /// Returns the number of committed write transactions, so the test can
    /// assert the chain saw every one of them.
    fn write_egregore_shaped_history(db: &aletheiadb::AletheiaDB) -> Result<u64, String> {
        let mut txns = 0u64;

        // Code facts, as the extractor writes them.
        let sym_a = create_node(
            db,
            "Symbol",
            &[
                ("codegraph_id", "node:symbol:repo:src/a.rs:foo"),
                ("symbol_kind", "function"),
            ],
        )?;
        txns += 1;
        let sym_b = create_node(
            db,
            "Symbol",
            &[
                ("codegraph_id", "node:symbol:repo:src/b.rs:bar"),
                ("symbol_kind", "function"),
            ],
        )?;
        txns += 1;
        db.create_edge(
            sym_a,
            sym_b,
            "CALLS",
            props(&[("codegraph_id", "edge:calls:1")]),
        )
        .map_err(|error| error.to_string())?;
        txns += 1;
        create_node(
            db,
            "AgentObservation",
            &[("codegraph_id", "node:obs:1"), ("note", "saw foo call bar")],
        )?;
        txns += 1;

        // `eg forget` (#231): logical retraction of sym_a — a Tombstone node
        // plus a Retraction event node. The retracted record's bytes are
        // untouched; suppression happens on read surfaces.
        create_node(
            db,
            "Tombstone",
            &[
                ("codegraph_id", "node:tombstone:repo:src/a.rs:foo"),
                ("deleted_id", "node:symbol:repo:src/a.rs:foo"),
            ],
        )?;
        txns += 1;
        create_node(
            db,
            "Retraction",
            &[
                ("codegraph_id", "node:retraction:1"),
                ("retracted_id", "node:symbol:repo:src/a.rs:foo"),
                ("retracted_by", "operator"),
            ],
        )?;
        txns += 1;

        // `eg forget-repo` (#248): bulk logical eviction — one tombstone per
        // evicted record plus a single auditable eviction event. Still only
        // writes; the evicted bytes stay for bi-temporal history.
        create_node(
            db,
            "Tombstone",
            &[
                ("codegraph_id", "node:tombstone:repo:src/b.rs:bar"),
                ("deleted_id", "node:symbol:repo:src/b.rs:bar"),
            ],
        )?;
        txns += 1;
        create_node(
            db,
            "Retraction",
            &[
                ("codegraph_id", "node:retraction:repo-eviction"),
                ("retracted_id", "repo:repo"),
                ("retracted_by", "operator"),
            ],
        )?;
        txns += 1;

        Ok(txns)
    }

    fn write_and_anchor(data_dir: &Path) -> Result<(u64, ChainHead), String> {
        let db = open_store(data_dir, true)?;
        let txns = write_egregore_shaped_history(&db)?;
        // Export the head the way a #338 pack manifest would at assemble
        // time; dropping the db flushes the sealer and checkpoints the log.
        let anchor = db.export_chain_head().map_err(|error| error.to_string())?;
        drop(db);
        Ok((txns, anchor))
    }

    #[test]
    fn retraction_shaped_history_verifies_clean_across_reopen() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let (txns, _anchor) = write_and_anchor(dir.path())?;

        // Reopen: the chain rebuilds any unsealed tail from replayed history
        // before the sealer starts, so verification is deterministic.
        let db = open_store(dir.path(), false)?;
        let verification = db.verify_chain().map_err(|error| error.to_string())?;
        assert!(
            verification.passed,
            "verify_chain must pass over Egregore-shaped writes + logical \
             retractions; reason: {:?}, earliest broken seq: {:?}",
            verification.reason, verification.earliest_broken_seq
        );
        assert_eq!(
            verification.earliest_broken_seq, None,
            "no transaction may read as tampered"
        );
        assert_eq!(
            verification.transactions_checked, txns,
            "the chain must have sealed every committed transaction"
        );
        Ok(())
    }

    #[test]
    fn exported_anchor_proves_append_only_extension() -> Result<(), String> {
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let (txns, anchor) = write_and_anchor(dir.path())?;

        let db = open_store(dir.path(), false)?;
        // Cheap freshness check for `verify_pack`: no version re-fetch, just
        // re-fold the sealed leaves and prove the current head extends the
        // anchor exported at pack-assembly time.
        let against = db
            .verify_chain_against(&anchor)
            .map_err(|error| error.to_string())?;
        assert!(
            against.passed,
            "verify_chain_against must prove append-only extension of the \
             exported anchor; reason: {:?}",
            against.reason
        );
        assert_eq!(against.head_seq, txns);

        // And a genuinely unrelated anchor must NOT verify: fork detection.
        let other_dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let other_db = open_store(other_dir.path(), true)?;
        let foreign_anchor = other_db
            .export_chain_head()
            .map_err(|error| error.to_string())?;
        drop(other_db);
        let forked = db
            .verify_chain_against(&foreign_anchor)
            .map_err(|error| error.to_string())?;
        assert!(
            !forked.passed,
            "an anchor from an unrelated chain must not verify"
        );
        Ok(())
    }

    #[test]
    fn chain_is_opt_in_and_absent_when_disabled() -> Result<(), String> {
        // ADR 0006 condition 1: enabling is deliberate. With the default
        // config the chain APIs must refuse rather than silently no-op.
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let config = aletheiadb::config::durable_config_for_data_dir(dir.path());
        let db = aletheiadb::AletheiaDB::with_unified_config(config)
            .map_err(|error| error.to_string())?;
        create_node(&db, "Symbol", &[("codegraph_id", "node:symbol:1")])?;
        assert!(
            db.verify_chain().is_err(),
            "verify_chain must fail closed when the chain was never enabled"
        );
        assert!(
            db.export_chain_head().is_err(),
            "export_chain_head must fail closed when the chain was never enabled"
        );
        Ok(())
    }
}
