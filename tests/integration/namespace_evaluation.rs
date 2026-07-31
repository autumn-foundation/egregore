//! Evidence locks for ADR-0006 — the `AletheiaDB` 0.2.0 namespace evaluation
//! (issue #485).
//!
//! Issue #485 is decision-first: it asks for a written evaluation answering four
//! open questions with citations into the 0.2.0 source, then a design slice or a
//! documented "not adopting, because …". A prose answer rots silently, so every
//! load-bearing premise of that evaluation is locked here as an executable test:
//!
//! * **Upstream characterization** (`upstream` module) — what 0.2.0's namespace
//!   surface actually does, exercised against the linked crate rather than the
//!   changelog. If an upstream release changes one of these behaviors the ADR's
//!   premise breaks loudly here instead of quietly in production.
//! * **Egregore premises** (`egregore` module) — the properties of this repo's
//!   `--repo` attribution model that the decision turns on.
//! * **Cross-repository witness** (`cross_repo_evidence_witness`) — that an
//!   evidence chain legitimately spans two repositories, and what a corpus
//!   missing one end reports.
//! * **Documentation locks** (`adr_doc`) — the ADR exists, records a decision,
//!   answers all four questions, and is linked from the index and agent guide.
//!
//! These tests lock **premises**, not the inferences the ADR draws from them.
//! They stay valid whether the decision is later revisited or reversed; what
//! would change is the ADR text they anchor.

#![allow(missing_docs)]

/// Characterization of the `AletheiaDB` 0.2.0 namespace surface itself.
///
/// Gated on `embedded-aletheiadb` because that is the feature under which the
/// `aletheiadb` crate is linked at all.
#[cfg(feature = "embedded-aletheiadb")]
mod upstream {
    use aletheiadb::{
        AletheiaDB, Namespace, NamespaceScope, PropertyMapBuilder, PropertyValue, StorageError,
        WriteOps, time,
    };

    /// A durable store rooted in a temp dir.
    ///
    /// Deliberately NOT a mirror of `EmbeddedAletheiaSink::open_inner`, which
    /// additionally disables `load_on_startup` for a fresh dir and pins
    /// `max_interned_strings`. The interner cap is process-global and read once
    /// per process, so a test binary shared with thousands of other tests should
    /// not race to set it.
    fn durable_db(data_dir: &std::path::Path) -> AletheiaDB {
        let config = aletheiadb::config::durable_config_for_data_dir(data_dir);
        AletheiaDB::with_unified_config(config).expect("durable store should open")
    }

    fn widget(name: &str) -> aletheiadb::PropertyMap {
        PropertyMapBuilder::new().insert("name", name).build()
    }

    /// Q1 (the "yes" half): a namespace scope DOES compose with bi-temporal
    /// point-in-time reads — `find_nodes_by_property_at_scoped` reconstructs at
    /// `(valid_time, transaction_time)` and then filters by the entity's
    /// immutable namespace.
    #[test]
    fn namespace_scope_composes_with_bitemporal_point_in_time_reads() {
        let db = AletheiaDB::new().expect("ephemeral store");
        let scoped = db
            .create_node_in_namespace("Widget", widget("scoped"), "repo/alpha")
            .expect("namespaced create");
        let unscoped = db
            .create_node("Widget", widget("unscoped"))
            .expect("default create");
        let now = time::now();

        let alpha = NamespaceScope::single(Namespace::new("repo/alpha").unwrap());
        let found = db
            .find_nodes_by_property_at_scoped(
                "Widget",
                "name",
                &PropertyValue::from("scoped"),
                now,
                now,
                &alpha,
            )
            .expect("scoped point-in-time find");
        let ids: Vec<_> = found.nodes.iter().map(|n| n.id).collect();
        assert_eq!(
            ids,
            vec![scoped],
            "a scoped point-in-time read returns the in-namespace node"
        );

        // The default-namespace sibling is invisible to that same scope even
        // though it matches the label and exists at the coordinate.
        let other = db
            .find_nodes_by_property_at_scoped(
                "Widget",
                "name",
                &PropertyValue::from("unscoped"),
                now,
                now,
                &alpha,
            )
            .expect("scoped point-in-time find");
        assert!(
            other.nodes.is_empty(),
            "a default-namespace node is filtered out of a repo/alpha-scoped read"
        );

        // Positive control: the in-scope node IS readable at the same coordinate,
        // so the negative below is about the scope and not about the coordinate.
        assert!(
            db.get_node_at_time_scoped(scoped, now, now, &alpha).is_ok(),
            "the in-scope node is readable at the bi-temporal coordinate"
        );
        // And the out-of-scope node is reported as MISSING, not as forbidden —
        // the exact premise Q2 rests on.
        let err = db
            .get_node_at_time_scoped(unscoped, now, now, &alpha)
            .expect_err("an out-of-scope node is not readable at a bi-temporal coordinate");
        assert!(
            matches!(
                err,
                aletheiadb::Error::Storage(StorageError::NodeNotFound(_))
            ),
            "out-of-scope reads report NOT_FOUND, indistinguishable from absent: {err:?}"
        );
    }

    /// Q1/Q3: the scoped AS-OF traversal honors the namespace boundary per hop —
    /// an edge into another namespace is never crossed even when both endpoints
    /// exist at the coordinate.
    ///
    /// Egregore calls no engine traversal today, so this locks a premise the ADR
    /// asserts rather than a behavior Egregore depends on.
    #[test]
    fn scoped_as_of_traversal_does_not_cross_the_namespace_boundary() {
        let db = AletheiaDB::new().expect("ephemeral store");
        let alpha_node = db
            .create_node_in_namespace("Widget", widget("alpha"), "repo/alpha")
            .expect("namespaced create");
        let alpha_peer = db
            .create_node_in_namespace("Widget", widget("alpha-peer"), "repo/alpha")
            .expect("namespaced create");
        let beta_node = db
            .create_node_in_namespace("Widget", widget("beta"), "repo/beta")
            .expect("namespaced create");
        db.create_edge_in_namespace(
            alpha_node,
            alpha_peer,
            "LINKS",
            PropertyMapBuilder::new().build(),
            "repo/alpha",
        )
        .expect("in-namespace edge");
        db.create_edge_in_namespace(
            alpha_node,
            beta_node,
            "LINKS",
            PropertyMapBuilder::new().build(),
            "repo/alpha",
        )
        .expect("cross-namespace edge");
        let now = time::now();

        let alpha = NamespaceScope::single(Namespace::new("repo/alpha").unwrap());
        let reached = db
            .traverse_scoped_as_of(alpha_node, Some("LINKS"), 3, now, now, &alpha)
            .expect("scoped as-of traversal");
        assert_eq!(
            reached,
            vec![alpha_peer],
            "the in-namespace hop is followed and the cross-namespace hop is not"
        );

        // Positive control: with an unfiltered scope BOTH hops are reachable, so
        // the assertion above is about the boundary and not about the fixture.
        let both = db
            .traverse_scoped_as_of(
                alpha_node,
                Some("LINKS"),
                3,
                now,
                now,
                &NamespaceScope::all(),
            )
            .expect("unscoped as-of traversal");
        assert!(
            both.contains(&alpha_peer) && both.contains(&beta_node),
            "without a scope the traversal reaches both peers: {both:?}"
        );
    }

    /// Q1 (the caveat, and its limits): the CHEAP namespace enumeration path
    /// (`list_nodes_scoped` via the membership index) is current-state, so an
    /// entity deleted through the ENGINE is no longer listable by namespace even
    /// though its history — namespace included — is still reconstructable at a
    /// past valid time.
    ///
    /// Egregore never creates this condition (see
    /// `egregore::egregore_issues_no_engine_level_deletes`); this test
    /// characterizes what the caveat actually is, so the ADR's claim that it does
    /// not bite is anchored to a real behavior.
    ///
    /// Determinism: the VALID-time axis is pinned to fixed 2020 constants (the
    /// node is created backdated and read at a fixed instant inside its validity),
    /// so that axis never races the wallclock. The TRANSACTION-time axis still
    /// needs an instant strictly before the delete — an engine delete closes the
    /// interval at its own timestamp, and `time::now()` is wallclock-derived — so
    /// one 2ms sleep separates them. Anchoring valid time in the past instead of
    /// the transaction time does NOT work: after the delete commits, a read at
    /// transaction time "now" reports the node gone regardless of the valid-time
    /// coordinate, which is why both axes are pinned before the deletion here.
    #[test]
    fn namespace_membership_enumeration_is_current_state_only() {
        use std::{thread::sleep, time::Duration};

        use aletheiadb::WriteOps as _;
        use aletheiadb::api::transaction::WriteRequestOptions;

        let db = AletheiaDB::new().expect("ephemeral store");
        let ns = Namespace::new("repo/alpha").unwrap();
        // A raw `with_namespace` write does not auto-register (unlike
        // `create_node_in_namespace`), so register first or the scope will not
        // validate.
        db.create_namespace("repo/alpha", None)
            .expect("register namespace");

        let backdated = time::from_secs(1_600_000_000); // 2020-09-13T12:26:40Z
        let node = db
            .write(|tx| {
                tx.create_node_with_options(
                    "Widget",
                    widget("doomed"),
                    WriteRequestOptions::new()
                        .with_namespace(ns.clone())
                        .with_valid_from(backdated),
                )
            })
            .expect("backdated namespaced create");

        let alpha = NamespaceScope::single(ns);
        // Positive control: it IS enumerable by namespace before the delete.
        assert_eq!(
            db.list_nodes_scoped(Some("Widget"), &alpha)
                .expect("scoped list"),
            vec![node],
            "a live entity is listable by namespace"
        );

        let before_delete = time::now();
        sleep(Duration::from_millis(2));
        db.write(|tx| tx.delete_node(node)).expect("delete");

        assert!(
            db.list_nodes_scoped(Some("Widget"), &alpha)
                .expect("scoped list")
                .is_empty(),
            "namespace enumeration is current-state: a deleted entity is not listed"
        );
        // `Some(0)`, not `map_or(0, …)`: the namespace is still REGISTERED, so
        // "absent from the counts" must not be able to masquerade as "zero".
        assert_eq!(
            db.namespace_counts()
                .iter()
                .find(|c| c.name == "repo/alpha")
                .map(|c| c.node_count),
            Some(0),
            "namespace_counts() is a current-state membership read"
        );

        // The entity's history is untouched, and it still carries its namespace.
        // Valid time is a fixed instant inside the backdated validity window;
        // transaction time is pinned before the delete committed.
        let observed = time::from_secs(1_600_000_100);
        let historical = db
            .get_node_at_time_scoped(node, observed, before_delete, &alpha)
            .expect("the deleted node is still reconstructable before its deletion");
        assert_eq!(
            historical.namespace().as_str(),
            "repo/alpha",
            "namespace is immutable across an entity's history"
        );
    }

    /// Q2: a repo-agnostic record placed in `default` is INVISIBLE to a
    /// repository-scoped read — an out-of-scope entity is indistinguishable from
    /// a missing one. Seeing it requires explicitly unioning `default` into the
    /// scope, which also re-admits every other repository's `default` residue.
    #[test]
    fn scoped_read_hides_default_namespace_records_unless_unioned() {
        let db = AletheiaDB::new().expect("ephemeral store");
        let repo_agnostic = db
            .create_node("EmbeddingModel", widget("vector-index-identity"))
            .expect("default create");

        let alpha = NamespaceScope::single(Namespace::new("repo/alpha").unwrap());
        // The namespace must exist for the scope to validate at all.
        db.create_namespace("repo/alpha", None)
            .expect("register namespace");

        let err = db
            .get_node_scoped(repo_agnostic, &alpha)
            .expect_err("a default-namespace node is invisible to a repo-scoped read");
        assert!(
            matches!(
                err,
                aletheiadb::Error::Storage(StorageError::NodeNotFound(_))
            ),
            "out-of-scope reads report NOT_FOUND, indistinguishable from absent: {err:?}"
        );

        let unioned = NamespaceScope::list(vec![
            Namespace::new("repo/alpha").unwrap(),
            Namespace::new(Namespace::DEFAULT).unwrap(),
        ])
        .expect("non-empty scope list");
        assert!(
            db.get_node_scoped(repo_agnostic, &unioned).is_ok(),
            "unioning `default` into the scope re-admits the repo-agnostic record"
        );
    }

    /// Q4: there is no supported re-stamp. A namespace is fixed at creation, the
    /// engine's own re-stamp helper only ever PRESERVES the existing value, and a
    /// user write that tries to forge the ride-along key is rejected.
    #[test]
    fn existing_entities_cannot_be_restamped_into_a_namespace() {
        let db = AletheiaDB::new().expect("ephemeral store");
        let legacy = db
            .create_node("Widget", widget("legacy"))
            .expect("default create");
        assert_eq!(
            db.get_node(legacy).unwrap().namespace().as_str(),
            Namespace::DEFAULT
        );

        // An ordinary update does not move it.
        db.write(|tx| tx.update_node(legacy, widget("legacy-updated")))
            .expect("update");
        assert_eq!(
            db.get_node(legacy).unwrap().namespace().as_str(),
            Namespace::DEFAULT,
            "an update never changes an entity's namespace"
        );

        // Forging the reserved ride-along key is rejected at the write seam.
        let forged = PropertyMapBuilder::new()
            .insert(aletheiadb::NAMESPACE_KEY, "repo/alpha")
            .build();
        let err = db
            .write(|tx| tx.update_node(legacy, forged))
            .expect_err("a user write may not carry the reserved namespace key");
        assert!(
            matches!(err, aletheiadb::Error::Namespace(_)),
            "reserved-key writes fail with a namespace error: {err:?}"
        );
        assert_eq!(
            db.get_node(legacy).unwrap().namespace().as_str(),
            Namespace::DEFAULT,
            "the rejected write left the entity in its original namespace"
        );
    }

    /// The issue's own warning, locked: `delete_namespace` removes the
    /// REGISTRATION, not the entities. Namespaces are not a physical-eviction
    /// route and do not close issue #472.
    #[test]
    fn delete_namespace_leaves_its_entities_intact() {
        let db = AletheiaDB::new().expect("ephemeral store");
        let node = db
            .create_node_in_namespace("Widget", widget("survivor"), "repo/alpha")
            .expect("namespaced create");

        db.delete_namespace("repo/alpha")
            .expect("registration delete");
        assert!(
            db.get_namespace("repo/alpha").is_err(),
            "the registration is gone"
        );

        let survivor = db.get_node(node).expect("the entity survives");
        assert_eq!(
            survivor.namespace().as_str(),
            "repo/alpha",
            "the entity keeps a namespace whose registration no longer exists"
        );
        assert!(
            db.namespace_counts()
                .iter()
                .any(|c| c.name == "repo/alpha" && c.node_count == 1),
            "counts still report the populated-but-unregistered namespace"
        );
    }

    /// Recursively locate a file by name under `root`.
    fn find_file(root: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
        let entries = std::fs::read_dir(root).ok()?;
        let mut dirs = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|f| f == name) {
                return Some(path);
            }
        }
        dirs.iter().find_map(|dir| find_file(dir, name))
    }

    /// Egregore-specific: under THIS crate's dependency configuration the
    /// namespace registry never persists at all.
    ///
    /// `Cargo.toml` links `aletheiadb` with `default-features = false` and does
    /// not enable `serde`, and both halves of the registry sidecar — the load in
    /// `NamespaceRegistry::open` and the write in `save_locked` — are
    /// `#[cfg(feature = "serde")]`. None of the features Egregore DOES enable
    /// (`semantic-search`, `semantic-temporal`, `semantic-diagnostics`,
    /// `embeddings`, `nova`) implies `serde`, so this holds in every CI config.
    /// A future dependency pulling `aletheiadb` with a `serde`-implying feature
    /// would flip it through Cargo feature unification with no local change —
    /// which is exactly what this test would catch.
    ///
    /// Consequence for any adoption: a namespace holding entities is self-healing
    /// (`reconcile_namespace_registry` rebuilds it from membership at load), but
    /// an explicitly-created EMPTY namespace is per-process.
    #[test]
    fn namespace_registry_does_not_persist_under_egregores_feature_set() {
        let temp = tempfile::tempdir().expect("temp dir");
        let data_dir = temp.path().join("store");
        let node = {
            let db = durable_db(&data_dir);
            let node = db
                .create_node_in_namespace("Widget", widget("survivor"), "repo/alpha")
                .expect("namespaced create");
            db.create_namespace("repo/empty", None)
                .expect("register an empty namespace");
            db.persist_indexes().expect("persist");
            node
        };

        // Positive control for the walk itself: `find_file` recurses into the
        // persistence directory, so the `None` below is evidence of absence and
        // not evidence that the walk never got there.
        assert!(
            find_file(&data_dir, "manifest.idx").is_some(),
            "the recursive walk reaches the persistence directory"
        );
        assert_eq!(
            find_file(&data_dir, "namespaces.json"),
            None,
            "with `serde` off the registry sidecar is never written"
        );

        let db = durable_db(&data_dir);

        // The populated namespace is reconciled back from entity membership.
        let alpha = NamespaceScope::single(Namespace::new("repo/alpha").unwrap());
        let listed = db
            .list_nodes_scoped(Some("Widget"), &alpha)
            .expect("a populated namespace is reconciled from entity membership");
        assert_eq!(listed, vec![node], "the scoped read still resolves");
        let survivor = db.get_node(node).expect("the entity survives");
        assert_eq!(survivor.namespace().as_str(), "repo/alpha");

        // The gap: an empty registered namespace has nothing to reconcile from.
        let empty = NamespaceScope::single(Namespace::new("repo/empty").unwrap());
        assert!(
            db.list_nodes_scoped(Some("Widget"), &empty).is_err(),
            "an EMPTY registered namespace does not survive a reopen"
        );
    }
}

/// Premises of Egregore's own `--repo` attribution model that the decision
/// turns on.
mod egregore {
    use aletheia_egregore::{
        EdgeLabel, EmbeddingModel, GraphRecord, IdentitySource, MetricKind, NodeKind,
        RepositoryIdentityPayload, RepositoryIndex, SelectionBasis, SemanticDriftMetadata,
        SourceSpan,
        embeddings::{embedding_index_identity_id, embedding_index_identity_record},
        repo_evict::{EvictionRequest, plan_eviction},
        stable_id,
    };

    const fn span(start: usize, end: usize) -> SourceSpan {
        SourceSpan {
            start_byte: 0,
            end_byte: 0,
            start_line: start,
            end_line: end,
        }
    }

    fn repository(display: &str, remote: &str) -> (String, GraphRecord) {
        let id = stable_id(&["repository", "remote", remote]);
        let record = GraphRecord::node(
            id.clone(),
            NodeKind::Repository,
            None,
            None,
            Some(display.to_owned()),
            format!("Repository {display}"),
        )
        .with_repository_identity(RepositoryIdentityPayload {
            identity_source: IdentitySource::Remote,
            remote_url: Some(remote.to_owned()),
            root_commit_sha: None,
            canonical_path: None,
            basename: display.rsplit('/').next().unwrap_or(display).to_owned(),
        });
        (id, record)
    }

    fn edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            format!("{} edge", label.as_str()),
        )
    }

    /// One repository's code spine: Repository -CONTAINS-> File -DEFINES-> Symbol.
    fn repo_spine(display: &str, remote: &str) -> (String, String, Vec<GraphRecord>) {
        let (repo_id, repo_record) = repository(display, remote);
        let file_id = stable_id(&["node", "file", &repo_id, "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "function", &repo_id, "widget"]);
        let records = vec![
            repo_record,
            GraphRecord::node(
                file_id.clone(),
                NodeKind::File,
                Some("src/lib.rs".to_owned()),
                None,
                Some("src/lib.rs".to_owned()),
                format!("source file in {display}"),
            ),
            edge(EdgeLabel::Contains, &repo_id, &file_id),
            GraphRecord::symbol(
                symbol_id.clone(),
                "function",
                "src/lib.rs".to_owned(),
                span(10, 20),
                "widget".to_owned(),
                format!("function widget in {display}"),
            ),
            edge(EdgeLabel::Defines, &file_id, &symbol_id),
        ];
        (repo_id, symbol_id, records)
    }

    fn observation(id: &str) -> GraphRecord {
        GraphRecord::node(
            id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "observation".to_owned(),
        )
        .with_domain("agent_memory", 1)
    }

    fn model() -> EmbeddingModel {
        EmbeddingModel {
            provider: "test".to_owned(),
            name: "test-model".to_owned(),
            version: "1".to_owned(),
            dim: 4,
            content_hash: "unknown".to_owned(),
        }
    }

    /// Q2: the vector-index identity node (issue #104) is deliberately
    /// repo-agnostic — it describes the STORE's single vector index, not any
    /// repository — so it has no natural namespace.
    ///
    /// Built from the REAL producer (`embedding_index_identity_record`), and
    /// paired with a `SemanticDrift` control that IS attributed. The control is
    /// what makes this non-vacuous: it proves semantic-domain records CAN be
    /// attributed by `RepositoryIndex`, so the identity node's lack of an owner
    /// is a property of the identity node and not of the fixture.
    #[test]
    fn repo_agnostic_embedding_index_identity_has_no_repository_owner() {
        let (repo_id, symbol_id, mut records) =
            repo_spine("acme/widget", "https://example.com/acme/widget");

        let identity = embedding_index_identity_record(&model());
        let identity_id = identity.id().to_owned();
        assert_eq!(
            identity_id,
            embedding_index_identity_id(),
            "the producer mints the fixed repo-agnostic identity id"
        );
        records.push(identity);

        // Control: a semantic-domain record that IS repo-attributable, via the
        // DRIFTS_FROM path RepositoryIndex uses for drift nodes.
        let drift_id = stable_id(&["node", "semantic-drift", &repo_id, "widget"]);
        records.push(
            GraphRecord::node(
                drift_id.clone(),
                NodeKind::SemanticDrift,
                Some("src/lib.rs".to_owned()),
                None,
                Some("widget".to_owned()),
                "semantic drift for widget".to_owned(),
            )
            .with_semantic_drift(SemanticDriftMetadata {
                embedding_model: model(),
                target_record_id: symbol_id.clone(),
                prior_record_id: symbol_id.clone(),
                before_git_commit: "a".repeat(40),
                after_git_commit: "b".repeat(40),
                before_valid_time: "2026-01-01T00:00:00Z".to_owned(),
                after_valid_time: "2026-01-02T00:00:00Z".to_owned(),
                metric_kind: MetricKind::CosineDistance,
                score: 0.5,
                selection_threshold: 0.2,
                selection_basis: SelectionBasis::ThresholdOnly,
            }),
        );
        records.push(edge(EdgeLabel::DriftsFrom, &drift_id, &symbol_id));

        let index = RepositoryIndex::build(&records);
        assert_eq!(
            index.owner_of(&symbol_id),
            Some(repo_id.as_str()),
            "the code spine is attributed"
        );
        assert_eq!(
            index.owner_of(&drift_id),
            Some(repo_id.as_str()),
            "control: a semantic-domain record IS attributable"
        );
        assert_eq!(
            index.owner_of(&identity_id),
            None,
            "the store-wide vector-index identity belongs to no repository"
        );
    }

    /// Locks the premise the ADR's Q1 answer rests on: Egregore performs no
    /// ENGINE-level delete. Deletion is entirely logical (tombstone records that
    /// are themselves live nodes), and each history snapshot is a distinct live
    /// node — so upstream's "namespace enumeration is current-state" caveat
    /// (`upstream::namespace_membership_enumeration_is_current_state_only`) never
    /// applies to an Egregore store.
    ///
    /// A source-level invariant, deliberately: the claim is about what Egregore
    /// never calls, which no single behavioral fixture can establish. Comment
    /// lines are stripped before matching so prose about deletion does not trip
    /// it. If a future physical-eviction mechanism (the #472 direction) starts
    /// issuing engine deletes, this fails and ADR-0006's cost analysis needs
    /// redoing.
    #[test]
    fn egregore_issues_no_engine_level_deletes() {
        use std::{fs, path::Path};

        const ENGINE_DELETE_CALLS: &[&str] = &[
            ".delete_node(",
            ".delete_edge(",
            ".delete_node_with_",
            ".delete_edge_with_",
            ".delete_node_cascade",
        ];

        fn scan(dir: &Path, hits: &mut Vec<String>) {
            for entry in fs::read_dir(dir).expect("readable source dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, hits);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    let text = fs::read_to_string(&path).expect("readable source file");
                    for (line_no, line) in text.lines().enumerate() {
                        let code = line.split("//").next().unwrap_or("");
                        for needle in ENGINE_DELETE_CALLS {
                            if code.contains(needle) {
                                hits.push(format!("{}:{}", path.display(), line_no + 1));
                            }
                        }
                    }
                }
            }
        }

        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        // Positive control: the scanner does read real code from this tree.
        assert!(
            fs::read_to_string(src.join("lib.rs"))
                .expect("lib.rs is readable")
                .contains("pub mod adapters"),
            "the scanned tree is Egregore's own source"
        );

        let mut hits = Vec::new();
        scan(&src, &mut hits);
        assert!(
            hits.is_empty(),
            "Egregore must not issue engine-level deletes; found: {hits:?}"
        );
    }

    /// `RepositoryIndex` derives ownership from graph CLOSURE, so a record's
    /// owner can be established by a LATER write: the same record is
    /// unattributed until its containment edge lands, and attributed afterwards.
    ///
    /// Note what this does NOT show: that a writer could not know the repository.
    /// For codegraph records it demonstrably does — `repository_id` is an input
    /// to the stable ID. The ADR's Decision §1 turns on the CROSS-DOMAIN records
    /// (agent memory, artifacts, verification), which acquire a repository only
    /// through evidence-edge closure.
    #[test]
    fn repository_index_requires_the_containment_edge_to_attribute() {
        let (repo_id, _, spine) = repo_spine("acme/widget", "https://example.com/acme/widget");

        // First ingest: an agent observation arrives with no evidence edge yet.
        let obs_id = "agent_memory:v1:late-bound-observation";
        let mut first_pass = spine;
        first_pass.push(observation(obs_id));
        assert_eq!(
            RepositoryIndex::build(&first_pass).owner_of(obs_id),
            None,
            "before any evidence edge exists the record has no derivable owner"
        );

        // Second ingest: the record is unchanged, but a new edge attributes it.
        let file_id = stable_id(&["node", "file", &repo_id, "src/lib.rs"]);
        let mut second_pass = first_pass;
        second_pass.push(edge(EdgeLabel::Contains, &file_id, obs_id));
        assert_eq!(
            RepositoryIndex::build(&second_pass).owner_of(obs_id),
            Some(repo_id.as_str()),
            "a later edge write establishes attribution for an already-written record"
        );
    }

    /// A namespace holds exactly one value per entity, but Egregore already has a
    /// first-class class of records attributable to MORE than one repository.
    /// `forget-repo` reports them as `shared_cross_repo` and refuses to evict
    /// them; a namespace cannot express that state at all.
    #[test]
    fn a_record_reached_from_two_repositories_is_shared_not_owned() {
        let (_, alpha_symbol, mut records) =
            repo_spine("acme/alpha", "https://example.com/acme/alpha");
        let (_, beta_symbol, beta) = repo_spine("acme/beta", "https://example.com/acme/beta");
        records.extend(beta);

        // One agent observation grounded in BOTH repositories' symbols.
        let obs_id = "agent_memory:v1:shared-observation".to_owned();
        records.push(observation(&obs_id));
        records.push(edge(EdgeLabel::Observes, &obs_id, &alpha_symbol));
        records.push(edge(EdgeLabel::Observes, &obs_id, &beta_symbol));

        let index = RepositoryIndex::build(&records);
        assert_eq!(
            index.owner_of(&obs_id),
            None,
            "the shared observation has no single derivable owner"
        );

        let plan = plan_eviction(
            &records,
            &records,
            &EvictionRequest {
                selector: "acme/alpha".to_owned(),
                reason: "evaluation fixture".to_owned(),
                evicted_by: "tester".to_owned(),
                transaction_time: Some("2026-01-01T00:00:00Z".to_owned()),
            },
        )
        .expect("plan");
        assert!(
            plan.shared_cross_repo.iter().any(|r| r.record_id == obs_id),
            "a record reached from two repositories is classified shared, never owned"
        );
        assert!(
            !plan.unattributable.iter().any(|r| r.record_id == obs_id),
            "shared is a distinct class from unattributable"
        );
    }
}

/// Q3: a cross-domain evidence witness chain may legitimately span two
/// repositories, and a corpus missing one end reports that end as ABSENT.
mod cross_repo_evidence_witness {
    use std::{fs, path::Path};

    use aletheia_egregore::{
        EdgeLabel, GraphRecord, IdentitySource, NodeKind, RepositoryIdentityPayload, SourceSpan,
        stable_id,
    };
    use assert_cmd::Command as CargoCommand;

    fn write_graph(records: &[GraphRecord], path: &Path) {
        let mut out = String::new();
        for record in records {
            out.push_str(&serde_json::to_string(record).unwrap());
            out.push('\n');
        }
        fs::write(path, out).unwrap();
    }

    fn edge(label: EdgeLabel, source: &str, target: &str) -> GraphRecord {
        GraphRecord::edge(
            label,
            source.to_owned(),
            target.to_owned(),
            Some("1.0".to_owned()),
            format!("{} edge", label.as_str()),
        )
    }

    fn repo_spine(display: &str, remote: &str) -> (String, Vec<GraphRecord>) {
        let repo_id = stable_id(&["repository", "remote", remote]);
        let file_id = stable_id(&["node", "file", &repo_id, "src/lib.rs"]);
        let symbol_id = stable_id(&["node", "symbol", "function", &repo_id, "widget"]);
        let records = vec![
            GraphRecord::node(
                repo_id.clone(),
                NodeKind::Repository,
                None,
                None,
                Some(display.to_owned()),
                format!("Repository {display}"),
            )
            .with_repository_identity(RepositoryIdentityPayload {
                identity_source: IdentitySource::Remote,
                remote_url: Some(remote.to_owned()),
                root_commit_sha: None,
                canonical_path: None,
                basename: display.rsplit('/').next().unwrap_or(display).to_owned(),
            }),
            GraphRecord::node(
                file_id.clone(),
                NodeKind::File,
                Some("src/lib.rs".to_owned()),
                None,
                Some("src/lib.rs".to_owned()),
                format!("source file in {display}"),
            ),
            edge(EdgeLabel::Contains, &repo_id, &file_id),
            GraphRecord::symbol(
                symbol_id.clone(),
                "function",
                "src/lib.rs".to_owned(),
                SourceSpan {
                    start_byte: 0,
                    end_byte: 0,
                    start_line: 10,
                    end_line: 20,
                },
                "widget".to_owned(),
                format!("function widget in {display}"),
            ),
            edge(EdgeLabel::Defines, &file_id, &symbol_id),
        ];
        (symbol_id, records)
    }

    /// Two facts, both load-bearing for ADR-0006 Q3:
    ///
    /// 1. An agent-memory witness chain legitimately connects symbols in TWO
    ///    repositories, in exactly two hops. `eg query evidence-path` is
    ///    repo-agnostic by design and returns it.
    /// 2. Over a corpus pruned to one repository's attributed records, the same
    ///    query reports the missing end as `endpoint_not_found` — naming WHICH
    ///    side is missing — rather than the softer `no_path`.
    ///
    /// Honest limit: this SIMULATES the corpus a namespace-scoped bulk read would
    /// produce by pruning a JSONL by hand. No `NamespaceScope` is involved, so it
    /// is evidence about the lane's behavior on a pruned corpus, not proof about
    /// namespaces. The ADR states the conditional explicitly.
    #[test]
    fn witness_chain_spans_two_repositories_and_a_pruned_corpus_names_the_missing_end() {
        let temp = tempfile::tempdir().unwrap();
        let (alpha_symbol, alpha_records) =
            repo_spine("acme/alpha", "https://example.com/acme/alpha");
        let (beta_symbol, beta_records) = repo_spine("acme/beta", "https://example.com/acme/beta");
        let obs_id = "agent_memory:v1:cross-repo-observation";
        let observation = GraphRecord::node(
            obs_id.to_owned(),
            NodeKind::Observation,
            None,
            None,
            None,
            "observation".to_owned(),
        )
        .with_domain("agent_memory", 1);

        // Full corpus: alpha's symbol <- OBSERVES - observation - OBSERVES -> beta's symbol.
        let mut full = alpha_records.clone();
        full.extend(beta_records);
        full.push(observation.clone());
        full.push(edge(EdgeLabel::Observes, obs_id, &alpha_symbol));
        full.push(edge(EdgeLabel::Observes, obs_id, &beta_symbol));
        let full_graph = temp.path().join("full.jsonl");
        write_graph(&full, &full_graph);

        let assert = CargoCommand::cargo_bin("egregore")
            .unwrap()
            .args(["query", "evidence-path", &alpha_symbol, &beta_symbol])
            .arg("--graph")
            .arg(&full_graph)
            .assert()
            .success();
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let summary: serde_json::Value =
            serde_json::from_str(stdout.lines().next().unwrap()).expect("summary line is JSON");
        assert_eq!(summary["ok"], true);
        assert_eq!(
            summary["hop_count"], 2,
            "the witness chain crosses the repository boundary in two hops"
        );

        // Pruned to alpha's attributed records: beta's symbol is simply absent.
        let mut pruned = alpha_records;
        pruned.push(observation);
        pruned.push(edge(EdgeLabel::Observes, obs_id, &alpha_symbol));
        let pruned_graph = temp.path().join("pruned.jsonl");
        write_graph(&pruned, &pruned_graph);

        let assert = CargoCommand::cargo_bin("egregore")
            .unwrap()
            .args(["query", "evidence-path", &alpha_symbol, &beta_symbol])
            .arg("--graph")
            .arg(&pruned_graph)
            .assert()
            .code(2);
        let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        let envelope: serde_json::Value =
            serde_json::from_str(stdout.lines().next().unwrap()).expect("error line is JSON");
        assert_eq!(envelope["ok"], false);
        assert_eq!(envelope["error"]["error_type"], "endpoint_not_found");
        // Which END is missing must be identified, not merely that one is.
        assert_eq!(envelope["error"]["side"], "target");
        assert_eq!(envelope["error"]["handle"], beta_symbol);
    }
}

/// Documentation locks for the ADR the issue asks for.
mod adr_doc {
    use std::{fs, path::Path};

    const ADR: &str = "docs/adr/0006-aletheiadb-namespaces-evaluation.md";

    fn read_repo_text(path: &str) -> String {
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(path))
            .unwrap_or_else(|error| panic!("{path} should be readable: {error}"))
    }

    fn assert_contains_all(path: &str, needles: &[&str]) {
        let text = read_repo_text(path);
        for needle in needles {
            assert!(text.contains(needle), "{path} must document `{needle}`");
        }
    }

    #[test]
    fn adr_records_a_decision_and_answers_all_four_questions() {
        assert_contains_all(
            ADR,
            &[
                // A decision is actually recorded, not deferred into prose.
                "## Decision",
                "Not adopting",
                // Each of the issue's four open questions has its own section.
                "### Q1",
                "### Q2",
                "### Q3",
                "### Q4",
                // The non-negotiable constraints from the issue are carried.
                "`--graph`",
                "`--data-dir`",
                "byte-identical",
                "Determinism",
                // The trap the issue calls out explicitly.
                "#472",
                "not a route to physical eviction",
                // The decision is revisitable on stated triggers, not forever.
                "## Revisit triggers",
                "## Alternatives considered",
            ],
        );
    }

    /// Names the identifiers and source files the ADR's reasoning depends on.
    ///
    /// This does NOT verify that any cited `file:line` resolves in the vendored
    /// crate — the ADR says so in its own header. It catches wholesale removal of
    /// a load-bearing citation, not line drift.
    #[test]
    fn adr_mentions_the_020_source_symbols_it_relies_on() {
        assert_contains_all(
            ADR,
            &[
                "src/db/namespace.rs",
                "src/db/namespace_query.rs",
                "src/core/namespace.rs",
                "aletheiadb-0.2.0",
                "list_nodes_scoped",
                "find_nodes_by_property_scoped",
                "get_node_at_time_scoped",
                "find_similar_scoped",
                "max_schema_as_of_entities",
                "delete_namespace",
                "delete_tenant",
                "restamp_namespace",
                "NamespaceScope",
            ],
        );
    }

    /// The evaluation must be explicit that no measurement backs it, and must
    /// name the executable evidence.
    #[test]
    fn adr_names_its_executable_evidence_and_its_honest_limits() {
        assert_contains_all(
            ADR,
            &[
                "tests/integration/namespace_evaluation.rs",
                "No benchmark was run",
                "Honest limit",
            ],
        );
    }

    #[test]
    fn adr_is_linked_from_the_index_and_the_agent_guide() {
        assert_contains_all(
            "docs/adr/README.md",
            &["0006-aletheiadb-namespaces-evaluation.md"],
        );
        // The agent guide's "0.2.0 subsystems are INERT" line must point at the
        // decision, so the next agent to reach for namespaces finds the answer
        // instead of re-deriving it. (`AGENTS.md` is a deliberately short mirror
        // and carries no 0.2.0 detail, so it is not required to link.)
        assert_contains_all(
            "CLAUDE.md",
            &["docs/adr/0006-aletheiadb-namespaces-evaluation.md"],
        );
    }
}
