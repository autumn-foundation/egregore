#![allow(missing_docs)]

//! Recall-time collapse of near-duplicate agent observations (issue #163).
//!
//! RED phase: these tests specify the contract before the implementation
//! exists. They cover:
//!
//! - AC1: one documented `eg` recall workflow (`eg query semantic-memory
//!   --collapse`) returns a collapsed answer, local-only.
//! - AC2: collapse is read-only — store fingerprint identical before/after;
//!   recall without `--collapse` returns every original record unchanged.
//! - AC3: eligibility = same primary cited target (resolved OBSERVES /
//!   `MENTIONS_SYMBOL` target, honoring `as_of_commit`) AND similarity >= pinned
//!   threshold; different targets never merge; target-less records never merge.
//! - AC4: embedding-backed stores reuse the stored vectors (cosine >= pinned
//!   threshold); stores without embeddings degrade to normalized-text
//!   equality; the mode actually used is named in the answer envelope.
//! - AC5: deterministic representative total order — highest confidence, then
//!   earliest `observed_at`, then lexicographically smallest record ID; the
//!   representative is a real stored record with its provenance intact.
//! - AC6: each representative carries `cluster_size`, the complete ordered
//!   member ID list (representative included), and min/max `observed_at`.
//! - AC7: trust spread per cluster (#114 vocabulary); never merges an
//!   agent-authored observation with a deterministic code fact.
//! - AC8: threshold and mode are caller-visible inputs echoed in the envelope;
//!   raising the threshold monotonically refines the partition.
//! - AC9: no raw transcript text / command output / patch hunks / issue bodies
//!   / env values / tokens in the output.
//! - AC10: byte-identical across 5 consecutive runs.
//! - Success metric: 25-record fixture (5 claims x 5 restatements, one shared
//!   target) -> <= 5 representatives, >= 60% JSON payload drop, 25/25 member
//!   IDs retrievable, highest-confidence representative in 100% of clusters.

#![cfg(all(feature = "embedded-aletheiadb", feature = "embeddings"))]

use std::collections::BTreeMap;
use std::path::Path;

use aletheia_egregore::adapters::{EmbeddedAletheiaSink, GraphSink};
use aletheia_egregore::embeddings::{EmbeddingVectorKey, EmbeddingVectorMap};
use aletheia_egregore::ir::AGENT_MEMORY_SCHEMA_VERSION;
use aletheia_egregore::query::{
    CollapseCandidate, CollapseConfig, CollapseEnvelope, CollapseMode, CollapsedMemoryRow,
    DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD, PrimaryCitedTarget, TrustClass, collapse_envelope,
    collapse_memory_recall, normalize_memory_text, render_collapsed_rows,
};
use aletheia_egregore::{
    EvidenceLink, Graph, GraphRecord, NodeKind, SourceSpan, TemporalMetadata, stable_id,
};
use assert_cmd::Command;

const DIM: usize = 5;

/// Five distinct claims; each is restated five times by different sessions.
/// All 25 restatements cite the same code target (the daemon.rs File node).
const CLAIMS: [[&str; 5]; 5] = [
    [
        "the daemon holds a lease file under .egregore to prove it is alive. if the file goes stale for more than a minute, a fresh daemon instance may start and the two will fight over the same port. stale leases have bitten us twice in staging already. the lease path is configurable but nobody has ever changed the default",
        "a lease file under .egregore is held by the daemon as a liveness signal. when that file stops being refreshed, a second daemon can come up and collide on the port. the on-call runbook now says to check the lease before restarting. a missing lease directory is created on startup with a warning",
        "the daemon keeps its lease file in the .egregore directory and refreshes it on a timer. a stale lease lets another instance start, which leads to two daemons binding the same port. we added a startup guard that refuses to bind when a live lease exists. the lease file format is just a pid and a timestamp, nothing fancier",
        "under .egregore the daemon maintains a lease file that marks it as the live instance. if the lease is not refreshed, a duplicate daemon may launch and the pair will contend for the port. the fix was a five-line check at startup; the outage was two hours. we considered etcd for this and correctly decided it was overkill",
        "the lease file lives under .egregore and is held by the daemon while it runs. let it go stale and a second daemon starts up, and then both fight over the listening port. monitoring now alerts when the lease age exceeds forty-five seconds. the windows port will need a different lease location",
    ],
    [
        "the parser panics on empty input; add a length check before indexing. every crash report we have seen traces back to an unchecked index into a zero-length buffer. the regression test feeds an empty file through every parser entry point. the parser is hand-written, so the guard went in without grammar changes",
        "empty input makes the parser panic - check the length before indexing. the backtraces all end at the same unchecked slice access on a zero-length buffer. we now fuzz the parser with zero-length inputs in CI. the fuzzer found two more empty-input panics in adjacent functions",
        "add a length check before indexing: the parser panics on empty input. all of the crash reports point at one unchecked index into an empty buffer. the fix landed as a one-line guard at the top of the parse function. input validation now happens in one place at the boundary",
        "the parser will panic if the input is empty; guard with a length check. each crash we investigated came from indexing a zero-length buffer without checking first. three separate crash reports turned out to be this same unchecked index. the old code trusted callers to never pass empty slices",
        "indexing into empty input panics the parser, so check length first. the recurring crash signature is an unchecked slice index on an empty buffer. the entry-point wrapper now rejects empty input before parsing begins. this class of bug is why we added the empty-input lint",
    ],
    [
        "prefer thiserror over anyhow in library crates for structured errors. anyhow erases the error type at the boundary, which makes programmatic handling by callers impossible. binaries may keep using anyhow; the rule only covers public library APIs. the migration guide documents the anyhow-to-thiserror mapping we used",
        "in library crates, choose thiserror instead of anyhow for structured errors. anyhow's type erasure at the public boundary prevents callers from matching on error variants. the lint for this lives in the workspace CI check script. error enums now derive Debug, Clone, and PartialEq for testability",
        "thiserror is preferred over anyhow in libraries so errors stay structured. once anyhow erases the type at the boundary, downstream code cannot handle specific failures. we migrated three crates last week and the diff was mostly mechanical. we kept anyhow for the binary crates where ergonomics win",
        "for structured errors in library crates prefer thiserror to anyhow. anyhow is fine for binaries, but libraries need the matchable variants thiserror generates. callers can now match on variants instead of stringifying the error. the public API docs now show matchable error variants",
        "library crates should use thiserror rather than anyhow for structured errors. type-erased anyhow errors at the API boundary leave callers with nothing to match on. the error-kind enum gained two new variants during the migration. new contributors get pointed at the error-handling ADR first",
    ],
    [
        "the cache keys on mtime; stale entries survive a rebuild. touching a source file without changing it is enough to poison the cache with entries the rebuild never clears. the long-term fix is content hashing, but that work is not scheduled yet. content hashing would also fix the rename-detection false positives",
        "stale cache entries survive rebuilds because the cache keys on mtime. a no-op touch updates the key, and the rebuild leaves the stale entry in place. CI now touches a canary file to detect the stale-entry problem early. the cache directory layout predates the current build system",
        "the cache is keyed by mtime, so stale entries persist across rebuilds. even touching a file without editing it creates a cache entry the rebuild does not invalidate. we lost a day debugging a green build that was actually a stale cache hit. we measured cache hits at ninety percent before the fix",
        "rebuilds do not clear stale entries: the cache keys on mtime. the key changes on touch alone, and the rebuild logic never evicts the orphaned entries. the cache-eviction ticket has been open for three sprints. the workaround is documented in the contributor guide",
        "an mtime-keyed cache means stale entries survive a rebuild. we saw entries from untouched-but-touched files linger long after the rebuild finished. a rebuild with --force-clean is the current workaround. the ticket now has a proposed design from the last sprint review",
    ],
    [
        "retry with exponential backoff and jitter on 429 responses. hammering the endpoint at a fixed interval just extends the rate-limit window and burns the request budget. the client wrapper now exposes the retry policy as a configurable struct. the policy struct has builder methods for tests",
        "on 429 responses, retry using exponential backoff plus jitter. fixed-interval retries synchronize with the limiter and keep the client throttled longer. we set max retries to five after the third incident. we log a warning with the computed backoff on every retry",
        "429s should be retried with exponential backoff and jitter. without jitter the retries line up with the rate-limit window and the throttle never lifts. the jitter uses a full-jitter strategy per the architecture notes. the default policy matches what the api docs recommend",
        "use exponential backoff with jitter when retrying 429 responses. a fixed sleep keeps hitting the limiter in phase, so the backoff never actually backs off. server headers now advertise the retry-after hint we honor. circuit breaking is the next step after retries stabilize",
        "back off exponentially with jitter before retrying a 429. retrying on a fixed cadence re-triggers the limiter and wastes the request budget. the load test confirmed the thundering herd is gone. the retry budget is shared across all outbound calls",
    ],
];

fn egregore() -> Command {
    Command::cargo_bin("egregore").expect("binary should run")
}

const fn span(start_line: usize, end_line: usize) -> SourceSpan {
    SourceSpan {
        start_byte: 0,
        end_byte: 100,
        start_line,
        end_line,
        start_column: None,
        end_column: None,
    }
}

fn link(target: &str, domain: &str, relation: &str, as_of_commit: Option<&str>) -> EvidenceLink {
    EvidenceLink {
        target_record_id: Some(target.to_owned()),
        target_domain: domain.to_owned(),
        relation: relation.to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: as_of_commit.map(str::to_owned),
        target_repo_relative_path: None,
        target_span: None,
        target_git_commit: None,
    }
}

/// A triple-only evidence link: no `target_record_id`, only the
/// (path, span, commit) triple — the shape CLI ingest stores, since unlike the
/// daemon it does not canonicalize triples at write time.
fn triple_link(path: &str, span: SourceSpan, commit: &str) -> EvidenceLink {
    EvidenceLink {
        target_record_id: None,
        target_domain: "codegraph".to_owned(),
        relation: "OBSERVES".to_owned(),
        confidence: "1.0".to_owned(),
        as_of_commit: None,
        target_repo_relative_path: Some(path.to_owned()),
        target_span: Some(span),
        target_git_commit: Some(commit.to_owned()),
    }
}

fn file_node(path: &str) -> GraphRecord {
    GraphRecord::syntax_node(
        stable_id(&["node", "File", path]),
        NodeKind::File,
        path.to_owned(),
        span(1, 100),
        path.to_owned(),
        "rust",
        format!("Source file {path}"),
    )
}

/// One restatement of claim `claim` (restatement index `rest`).
///
/// - claim `2`'s first two restatements carry `VALIDATED_BY` links (verified);
///   everything else is unverified -> the claim-2 cluster must report a
///   mixed trust spread.
/// - restatement 0 of every claim has the highest confidence and the earliest
///   `observed_at`, so it must win the representative total order.
fn memory_node(claim: usize, rest: usize, target_id: &str, verified: bool) -> GraphRecord {
    let id = format!("agent_memory:v1:collapse-c{claim}-r{rest}");
    let mut node = GraphRecord::node(
        id.clone(),
        NodeKind::Observation,
        None,
        None,
        None,
        format!("memory {id}"),
    );
    let confidence = if rest == 0 {
        "0.95".to_owned()
    } else {
        format!("0.{}", 5 + rest)
    };
    // observed_at spreads one day per claim; restatement 0 is earliest.
    let observed_at = format!("2026-06-{:02}T1{}:00:00Z", 1 + claim, rest);
    let mut links = vec![link(target_id, "codegraph", "OBSERVES", None)];
    if verified {
        links.push(link(
            "verification:v1:collapse-ver",
            "verification",
            "VALIDATED_BY",
            None,
        ));
    }
    if let GraphRecord::Node {
        text: ref mut t,
        schema_version: ref mut sv,
        agent_id: ref mut aid,
        agent_kind: ref mut ak,
        session_id: ref mut sid,
        observed_at: ref mut oa,
        ingested_at: ref mut ia,
        confidence: ref mut conf,
        source_handle: ref mut sh,
        evidence_links: ref mut el,
        domain: ref mut dom,
        ..
    } = node
    {
        *t = Some(CLAIMS[claim][rest].to_owned());
        *sv = AGENT_MEMORY_SCHEMA_VERSION;
        *aid = Some(format!("agent_{}", (claim + rest) % 2 + 1));
        *ak = Some("claude-code".to_owned());
        *sid = Some(format!("sess_c{claim}_r{rest}"));
        *oa = Some(observed_at);
        *ia = Some("2026-06-01T00:00:01Z".to_owned());
        *conf = Some(confidence);
        *sh = Some(format!("trajectories/collapse-c{claim}-r{rest}.traj"));
        *el = Some(links);
        *dom = Some("agent_memory".to_owned());
    }
    node
}

fn verification_node() -> GraphRecord {
    let mut node = GraphRecord::node(
        "verification:v1:collapse-ver".to_owned(),
        NodeKind::Verification,
        None,
        None,
        None,
        "Verification pass".to_owned(),
    );
    if let GraphRecord::Node {
        schema_version: ref mut sv,
        status: ref mut st,
        verification_kind: ref mut vk,
        ..
    } = node
    {
        *sv = aletheia_egregore::ir::VERIFICATION_SCHEMA_VERSION;
        *st = Some("pass".to_owned());
        *vk = Some("command_run".to_owned());
    }
    node
}

/// Deterministic 5-D unit-ish vectors: claim `c` lives on axis `c` with a
/// small deterministic jitter per restatement. Within-claim cosine ~0.9975 to
/// ~0.9988, across-claim cosine <= ~0.1, so the pinned 0.85 threshold
/// separates the five claims exactly while a 0.9995 threshold separates every
/// restatement.
fn claim_vector(claim: usize, rest: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; DIM];
    v[claim] = 1.0;
    v[(claim + rest + 1) % DIM] += 0.05;
    v
}

struct Seed {
    _temp: tempfile::TempDir,
    data_dir: std::path::PathBuf,
}

/// Seeds the 25-record fixture with injected vectors (no model needed).
fn seed_store() -> Seed {
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");

    let target = file_node("src/daemon.rs");
    let target_id = target.id().to_owned();
    let ver = verification_node();

    let mut vectors = EmbeddingVectorMap::new();
    vectors.insert(
        EmbeddingVectorKey::from_record(&target).expect("file embeddable"),
        vec![0.9, 0.1, 0.0, 0.0, 0.0],
    );

    let mut records = vec![target, ver];
    for claim in 0..5 {
        for rest in 0..5 {
            let verified = claim == 2 && rest < 2;
            let node = memory_node(claim, rest, &target_id, verified);
            vectors.insert(
                EmbeddingVectorKey::from_record(&node).expect("memory embeddable"),
                claim_vector(claim, rest),
            );
            records.push(node);
        }
    }

    let mut sink =
        EmbeddedAletheiaSink::open_with_embeddings(&data_dir, vectors, DIM).expect("store opens");
    for record in &records {
        sink.write_record(record).expect("record writes");
    }
    sink.persist_indexes().expect("indexes persist");
    drop(sink);

    Seed {
        _temp: temp,
        data_dir,
    }
}

/// Builds [`CollapseCandidate`]s for every observation-class record in the
/// store, mirroring the CLI's candidate construction (same gates, same
/// provenance, same trust derivation), with vectors read back from the
/// store's own vector index.
fn candidates_from_store(seed: &Seed) -> Vec<CollapseCandidate> {
    let sink = EmbeddedAletheiaSink::open(&seed.data_dir).expect("store opens");
    let records = sink.read_all_records().expect("records read");
    let mut out = Vec::new();
    for record in &records {
        let GraphRecord::Node {
            text,
            summary,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            confidence,
            source_handle,
            evidence_links,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(
            record.node_kind_name(),
            Some("Observation" | "Decision" | "Failure")
        ) {
            continue;
        }
        let has_provenance = source_handle.is_some() || session_id.is_some();
        if !has_provenance {
            continue;
        }
        let body = text.as_deref().unwrap_or(summary.as_str()).to_owned();
        let primary_target = evidence_links.as_ref().and_then(|links| {
            links
                .iter()
                .filter(|l| {
                    matches!(l.relation.as_str(), "OBSERVES" | "MENTIONS_SYMBOL")
                        && l.target_record_id.is_some()
                })
                .map(|l| {
                    (
                        l.relation.clone(),
                        l.target_record_id.clone().unwrap_or_default(),
                        l.as_of_commit.clone(),
                    )
                })
                .min_by(|a, b| {
                    a.0.cmp(&b.0)
                        .then_with(|| a.1.cmp(&b.1))
                        .then_with(|| a.2.cmp(&b.2))
                })
                .map(
                    |(relation, target_record_id, as_of_commit)| PrimaryCitedTarget {
                        target_record_id,
                        relation,
                        as_of_commit,
                    },
                )
        });
        let verified = evidence_links.as_ref().is_some_and(|links| {
            links.iter().any(|l| {
                matches!(
                    l.relation.as_str(),
                    "VALIDATED_BY" | "HAS_EVIDENCE" | "PRODUCED_EVIDENCE"
                )
            })
        });
        out.push(CollapseCandidate {
            record_id: record.id().to_owned(),
            kind: record.node_kind_name().unwrap_or("Observation").to_owned(),
            trust_class: if verified {
                TrustClass::AgentVerified
            } else {
                TrustClass::AgentUnverified
            },
            agent_authored: true,
            primary_target,
            body_text: body,
            confidence_raw: confidence.clone(),
            confidence: confidence.as_deref().and_then(|s| s.parse::<f32>().ok()),
            observed_at: observed_at.clone(),
            vector: sink.stored_embedding_vector(record),
            retrieval_score: None,
            source_handle: source_handle.clone(),
            agent_id: agent_id.clone(),
            agent_kind: agent_kind.clone(),
            session_id: session_id.clone(),
            evidence_links: evidence_links.clone(),
        });
    }
    out.sort_by(|a, b| a.record_id.cmp(&b.record_id));
    out
}

/// Canonical store fingerprint: record IDs, tombstone count, vector-index
/// state, and the on-disk file inventory (paths + sizes). Any created,
/// modified, superseded, or deleted record/index/tombstone/receipt changes
/// this string.
fn store_fingerprint(data_dir: &Path) -> String {
    fn walk(dir: &Path, out: &mut Vec<(String, String)>, root: &Path) {
        let entries = std::fs::read_dir(dir).expect("read dir");
        let mut names: Vec<_> = entries.map(|e| e.expect("dir entry").path()).collect();
        names.sort();
        for path in names {
            if path.is_dir() {
                walk(&path, out, root);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("prefix")
                    .to_string_lossy()
                    .into_owned();
                // The engine's private `indexes/` subtree is EXCLUDED from the
                // byte-identity check: a bare EmbeddedAletheiaSink open/drop
                // with no query at all rewrites files there (probed:
                // `indexes/indexes/manifest.idx` changes on every open), so
                // byte-identity of those files is an engine-housekeeping
                // concern, not a collapse concern. Semantic index state is
                // still asserted strictly via `embedding_index_state()` below,
                // and the WAL (the record of actual mutations) IS hashed.
                if rel.starts_with("indexes/") || rel.starts_with("indexes\\") {
                    continue;
                }
                // Hash file CONTENTS, not just sizes: a same-size rewrite
                // (record/tombstone/receipt mutation that preserves length)
                // must change the fingerprint.
                let bytes = std::fs::read(&path).expect("file reads");
                let digest = blake3::hash(&bytes).to_hex().to_string();
                out.push((rel, digest));
            }
        }
    }

    let sink = EmbeddedAletheiaSink::open(data_dir).expect("store opens");
    let records = sink.read_all_records().expect("records read");
    let mut ids: Vec<&str> = records.iter().map(GraphRecord::id).collect();
    ids.sort_unstable();
    let tombstones = records
        .iter()
        .filter(|r| matches!(r, GraphRecord::Tombstone { .. }))
        .count();
    let index_state = format!("{:?}", sink.embedding_index_state());
    drop(sink);
    let mut files = Vec::new();
    walk(data_dir, &mut files, data_dir);
    format!(
        "records={} ids={ids:?} tombstones={tombstones} index={index_state} files={files:?}",
        records.len()
    )
}

const fn default_config() -> CollapseConfig {
    CollapseConfig {
        similarity_threshold: DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD,
        mode: CollapseMode::EmbeddingCosine,
    }
}

// ---------------------------------------------------------------------------
// AC1/AC5/AC6: five claims -> five representatives, full member lists
// ---------------------------------------------------------------------------

#[test]
fn collapse_groups_five_claims_into_five_representatives() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);
    assert_eq!(
        candidates.len(),
        25,
        "all 25 fixture records are candidates"
    );

    let outcome = collapse_memory_recall(&candidates, &default_config());
    assert_eq!(outcome.mode, CollapseMode::EmbeddingCosine);
    assert!(
        (outcome.threshold - DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD).abs() < f32::EPSILON,
        "the pinned default threshold is used"
    );
    assert_eq!(outcome.clusters.len(), 5, "one representative per claim");

    let mut all_members: Vec<&str> = Vec::new();
    for cluster in &outcome.clusters {
        assert_eq!(cluster.members.len(), 5, "each claim has 5 restatements");
        assert_eq!(
            cluster.representative.record_id, cluster.members[0].record_id,
            "representative is first in the member list"
        );
        // AC6: complete ordered member list, representative included.
        let ids: Vec<&str> = cluster
            .members
            .iter()
            .map(|m| m.record_id.as_str())
            .collect();
        assert_eq!(ids[0], cluster.representative.record_id);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5, "no member ID dropped or duplicated");
        all_members.extend(ids);
    }
    all_members.sort_unstable();
    all_members.dedup();
    assert_eq!(
        all_members.len(),
        25,
        "100% of record IDs retrievable via member lists"
    );
}

#[test]
fn highest_confidence_member_is_representative_in_every_cluster() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());

    for cluster in &outcome.clusters {
        let best = cluster
            .members
            .iter()
            .map(|m| m.confidence.unwrap_or(f32::NEG_INFINITY))
            .fold(f32::NEG_INFINITY, f32::max);
        assert_eq!(
            cluster.representative.confidence,
            Some(best),
            "representative must be the highest-confidence member"
        );
        assert!(
            cluster.representative.record_id.ends_with("-r0"),
            "restatement 0 carries confidence 0.95 and the earliest observed_at: {}",
            cluster.representative.record_id
        );
    }
}

#[test]
fn cluster_carries_min_max_observed_at() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());

    for cluster in &outcome.clusters {
        let mut times: Vec<&str> = cluster
            .members
            .iter()
            .filter_map(|m| m.observed_at.as_deref())
            .collect();
        times.sort_unstable();
        assert_eq!(
            cluster.observed_at_min.as_deref(),
            Some(times[0]),
            "min observed_at across the cluster"
        );
        assert_eq!(
            cluster.observed_at_max.as_deref(),
            Some(times[times.len() - 1]),
            "max observed_at across the cluster"
        );
    }
}

// ---------------------------------------------------------------------------
// AC5: representative total order (confidence, observed_at, record ID)
// ---------------------------------------------------------------------------

fn unit_candidate(
    id: &str,
    confidence: Option<&str>,
    observed_at: Option<&str>,
) -> CollapseCandidate {
    CollapseCandidate {
        record_id: id.to_owned(),
        kind: "Observation".to_owned(),
        trust_class: TrustClass::AgentUnverified,
        agent_authored: true,
        primary_target: Some(PrimaryCitedTarget {
            target_record_id: "target-1".to_owned(),
            relation: "OBSERVES".to_owned(),
            as_of_commit: None,
        }),
        body_text: "identical normalized body".to_owned(),
        confidence_raw: confidence.map(str::to_owned),
        confidence: confidence.and_then(|s| s.parse::<f32>().ok()),
        observed_at: observed_at.map(str::to_owned),
        vector: Some(vec![1.0, 0.0, 0.0, 0.0, 0.0]),
        retrieval_score: None,
        source_handle: Some("traj".to_owned()),
        agent_id: None,
        agent_kind: None,
        session_id: None,
        evidence_links: None,
    }
}

#[test]
fn representative_tie_break_is_confidence_then_observed_at_then_record_id() {
    // Same confidence -> earliest observed_at wins.
    let candidates = vec![
        unit_candidate("id-b", Some("0.5"), Some("2026-06-02T00:00:00Z")),
        unit_candidate("id-a", Some("0.5"), Some("2026-06-01T00:00:00Z")),
    ];
    let outcome = collapse_memory_recall(&candidates, &default_config());
    assert_eq!(outcome.clusters.len(), 1);
    assert_eq!(outcome.clusters[0].representative.record_id, "id-a");

    // Same confidence and observed_at -> lexicographically smallest ID wins.
    let candidates = vec![
        unit_candidate("id-b", Some("0.5"), Some("2026-06-01T00:00:00Z")),
        unit_candidate("id-a", Some("0.5"), Some("2026-06-01T00:00:00Z")),
    ];
    let outcome = collapse_memory_recall(&candidates, &default_config());
    assert_eq!(outcome.clusters[0].representative.record_id, "id-a");

    // Missing confidence sorts below any present confidence.
    let candidates = vec![
        unit_candidate("id-a", None, Some("2026-06-01T00:00:00Z")),
        unit_candidate("id-b", Some("0.1"), Some("2026-06-02T00:00:00Z")),
    ];
    let outcome = collapse_memory_recall(&candidates, &default_config());
    assert_eq!(outcome.clusters[0].representative.record_id, "id-b");

    // Missing observed_at sorts after any present observed_at.
    let candidates = vec![
        unit_candidate("id-a", Some("0.5"), None),
        unit_candidate("id-b", Some("0.5"), Some("2026-06-02T00:00:00Z")),
    ];
    let outcome = collapse_memory_recall(&candidates, &default_config());
    assert_eq!(outcome.clusters[0].representative.record_id, "id-b");
}

// ---------------------------------------------------------------------------
// AC3: same primary cited target required; as_of_commit honored
// ---------------------------------------------------------------------------

#[test]
fn different_targets_never_merge_even_when_textually_similar() {
    let mut a = unit_candidate("id-a", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    a.body_text = "the daemon holds a lease file".to_owned();
    let mut b = unit_candidate("id-b", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    b.body_text = "the daemon holds a lease file".to_owned();
    b.primary_target = Some(PrimaryCitedTarget {
        target_record_id: "target-2".to_owned(),
        relation: "OBSERVES".to_owned(),
        as_of_commit: None,
    });
    let outcome = collapse_memory_recall(&[a, b], &default_config());
    assert_eq!(
        outcome.clusters.len(),
        2,
        "different cited targets must never merge, even with identical text and vectors"
    );
}

#[test]
fn same_target_at_different_commits_does_not_merge() {
    let a = unit_candidate("id-a", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    let mut b = unit_candidate("id-b", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    b.primary_target = Some(PrimaryCitedTarget {
        target_record_id: "target-1".to_owned(),
        relation: "OBSERVES".to_owned(),
        as_of_commit: Some("abc123".to_owned()),
    });
    let outcome = collapse_memory_recall(&[a, b], &default_config());
    assert_eq!(
        outcome.clusters.len(),
        2,
        "as_of_commit is part of the target identity"
    );
}

#[test]
fn same_target_through_different_relations_merges() {
    // Issue #163: eligibility is the same resolved primary target record ID
    // plus similarity — the citation relation is not part of the cluster
    // identity. An OBSERVES citation and a MENTIONS_SYMBOL citation of the
    // same target with identical text collapse together.
    let a = unit_candidate("id-a", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    let mut b = unit_candidate("id-b", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    b.primary_target = Some(PrimaryCitedTarget {
        target_record_id: "target-1".to_owned(),
        relation: "MENTIONS_SYMBOL".to_owned(),
        as_of_commit: None,
    });
    let outcome = collapse_memory_recall(&[a, b], &default_config());
    assert_eq!(
        outcome.clusters.len(),
        1,
        "same target record ID merges across OBSERVES / MENTIONS_SYMBOL"
    );
    assert_eq!(outcome.clusters[0].members.len(), 2);
}

#[test]
fn targetless_records_never_merge() {
    let mut a = unit_candidate("id-a", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    a.primary_target = None;
    let mut b = unit_candidate("id-b", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    b.primary_target = None;
    let outcome = collapse_memory_recall(&[a, b], &default_config());
    assert_eq!(
        outcome.clusters.len(),
        2,
        "a memory citing no code target has no eligible partner"
    );
}

// ---------------------------------------------------------------------------
// AC7: trust spread; never merge agent observation with deterministic fact
// ---------------------------------------------------------------------------

#[test]
fn mixed_trust_cluster_reports_trust_spread() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());

    let claim2 = outcome
        .clusters
        .iter()
        .find(|c| c.representative.record_id.contains("-c2-"))
        .expect("claim-2 cluster exists");
    let spread = &claim2.trust_spread;
    assert_eq!(
        spread.get(TrustClass::AgentVerified.as_str()),
        Some(&2),
        "two verified restatements"
    );
    assert_eq!(
        spread.get(TrustClass::AgentUnverified.as_str()),
        Some(&3),
        "three unverified restatements"
    );
    // The representative's own trust class must not silently stand in for the
    // group: the spread is reported per cluster, not per representative.
    assert_eq!(spread.values().sum::<usize>(), 5);
}

#[test]
fn never_merges_agent_observation_with_deterministic_code_fact() {
    let mut fact = unit_candidate("code-fact-1", Some("1.0"), Some("2026-06-01T00:00:00Z"));
    fact.agent_authored = false;
    fact.trust_class = TrustClass::SourceDerived;
    let obs = unit_candidate("obs-1", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    let outcome = collapse_memory_recall(&[fact, obs], &default_config());
    assert_eq!(
        outcome.clusters.len(),
        2,
        "an agent-authored observation must never collapse with a deterministic code fact"
    );
}

#[test]
fn two_non_agent_candidates_never_merge_even_when_identical() {
    // Guards the `agent_authored ==` vs `agent_authored &&` distinction: two
    // source-derived records must stay singletons, not merge via `false == false`.
    let mut a = unit_candidate("fact-a", Some("1.0"), Some("2026-06-01T00:00:00Z"));
    a.agent_authored = false;
    a.trust_class = TrustClass::SourceDerived;
    a.body_text = "the daemon holds a lease file".to_owned();
    let mut b = unit_candidate("fact-b", Some("1.0"), Some("2026-06-01T00:00:00Z"));
    b.agent_authored = false;
    b.trust_class = TrustClass::SourceDerived;
    b.body_text = "the daemon holds a lease file".to_owned();
    let config = CollapseConfig {
        similarity_threshold: DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD,
        mode: CollapseMode::NormalizedText,
    };
    let outcome = collapse_memory_recall(&[a, b], &config);
    assert_eq!(
        outcome.clusters.len(),
        2,
        "two non-agent-authored records must never collapse together"
    );
}

// ---------------------------------------------------------------------------
// AC4: normalized-text degradation mode
// ---------------------------------------------------------------------------

#[test]
fn normalized_text_mode_clusters_on_post_redaction_equality() {
    let config = CollapseConfig {
        similarity_threshold: DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD,
        mode: CollapseMode::NormalizedText,
    };
    let mut a = unit_candidate("id-a", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    a.body_text = "The Daemon   holds a LEASE file".to_owned();
    a.vector = None;
    let mut b = unit_candidate("id-b", Some("0.8"), Some("2026-06-02T00:00:00Z"));
    b.body_text = "the daemon holds a lease file".to_owned();
    b.vector = None;
    let mut c = unit_candidate("id-c", Some("0.7"), Some("2026-06-03T00:00:00Z"));
    c.body_text = "the daemon holds a lock file".to_owned();
    c.vector = None;
    let outcome = collapse_memory_recall(&[a, b, c], &config);
    assert_eq!(outcome.mode, CollapseMode::NormalizedText);
    assert_eq!(
        outcome.clusters.len(),
        2,
        "a/b share normalized text; c differs"
    );
    let ab = outcome
        .clusters
        .iter()
        .find(|cl| cl.members.len() == 2)
        .expect("a/b cluster");
    assert_eq!(ab.representative.record_id, "id-a");
}

#[test]
fn normalize_memory_text_collapses_case_and_whitespace() {
    assert_eq!(
        normalize_memory_text("  The   Daemon\nHolds\ta lease file. "),
        "the daemon holds a lease file."
    );
}

#[test]
fn embedding_mode_requires_vectors_on_both_sides() {
    // One side without a stored vector must not merge (fail closed, never a
    // silent semantic grouping the store cannot compute).
    let a = unit_candidate("id-a", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    let mut b = unit_candidate("id-b", Some("0.9"), Some("2026-06-01T00:00:00Z"));
    b.vector = None;
    let outcome = collapse_memory_recall(&[a, b], &default_config());
    assert_eq!(outcome.clusters.len(), 2);
}

// ---------------------------------------------------------------------------
// AC8: threshold monotonicity; envelope echoes inputs
// ---------------------------------------------------------------------------

#[test]
fn raising_threshold_monotonically_refines_the_partition() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);

    let coarse = collapse_memory_recall(
        &candidates,
        &CollapseConfig {
            similarity_threshold: 0.5,
            mode: CollapseMode::EmbeddingCosine,
        },
    );
    let fine = collapse_memory_recall(
        &candidates,
        &CollapseConfig {
            similarity_threshold: 0.9995,
            mode: CollapseMode::EmbeddingCosine,
        },
    );
    assert!(
        fine.clusters.len() >= coarse.clusters.len(),
        "raising the threshold must never coarsen the partition"
    );
    // Every fine cluster is a subset of some coarse cluster (refinement).
    let coarse_sets: Vec<std::collections::BTreeSet<&str>> = coarse
        .clusters
        .iter()
        .map(|c| c.members.iter().map(|m| m.record_id.as_str()).collect())
        .collect();
    for fine_cluster in &fine.clusters {
        let fine_ids: std::collections::BTreeSet<&str> = fine_cluster
            .members
            .iter()
            .map(|m| m.record_id.as_str())
            .collect();
        assert!(
            coarse_sets.iter().any(|s| fine_ids.is_subset(s)),
            "fine cluster {fine_ids:?} must refine a coarse cluster"
        );
    }
    assert_eq!(coarse.clusters.len(), 5);
    assert_eq!(
        fine.clusters.len(),
        25,
        "at 0.9995 no two restatements clear the bar"
    );
}

#[test]
fn envelope_names_mode_and_threshold() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());
    let envelope: CollapseEnvelope = collapse_envelope(
        "what did we learn?",
        outcome.mode,
        "auto",
        outcome.threshold,
        candidates.len(),
        outcome.clusters.len(),
    );
    let json = serde_json::to_value(&envelope).expect("serializes");
    assert_eq!(json["ok"], true);
    assert_eq!(json["collapse"]["mode"], "embedding-cosine");
    assert_eq!(json["collapse"]["mode_requested"], "auto");
    let threshold = json["collapse"]["similarity_threshold"]
        .as_f64()
        .expect("threshold is a number");
    assert!(
        (threshold - f64::from(DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD)).abs() < 1e-6,
        "threshold echoed verbatim, got {threshold}"
    );
    assert_eq!(json["collapse"]["source_records"], 25);
    assert_eq!(json["collapse"]["representatives"], 5);
    assert_eq!(json["collapse"]["collapsed_away"], 20);
}

// ---------------------------------------------------------------------------
// AC9: no raw transcript / command output / secrets in the output
// ---------------------------------------------------------------------------

/// The collapsed JSON schema allowlist for issue #163: rows and the
/// envelope must expose exactly the documented field set.
fn assert_collapsed_schema(rows: &[CollapsedMemoryRow], envelope: &CollapseEnvelope) {
    let row_value = serde_json::to_value(&rows[0]).expect("row serializes");
    let row_object = row_value.as_object().expect("row is an object");
    let mut row_keys: Vec<&str> = row_object.keys().map(String::as_str).collect();
    row_keys.sort_unstable();
    assert_eq!(
        row_keys,
        [
            "agent_id",
            "agent_kind",
            "cluster_observed_at_max",
            "cluster_observed_at_min",
            "cluster_size",
            "confidence",
            "evidence_links",
            "kind",
            "member_ids",
            "memory_text",
            "observed_at",
            "primary_cited_target",
            "record_id",
            "representative_trust_class",
            "retrieval_score",
            "session_id",
            "source_handle",
            "trust_class",
            "trust_spread",
        ],
        "collapsed rows must not gain undocumented fields"
    );

    let envelope_value = serde_json::to_value(envelope).expect("envelope serializes");
    let envelope_object = envelope_value.as_object().expect("envelope is an object");
    let mut envelope_keys: Vec<&str> = envelope_object.keys().map(String::as_str).collect();
    envelope_keys.sort_unstable();
    assert_eq!(envelope_keys, ["collapse", "ok", "query"]);
    let mut collapse_keys: Vec<&str> = envelope_object["collapse"]
        .as_object()
        .expect("collapse detail is an object")
        .keys()
        .map(String::as_str)
        .collect();
    collapse_keys.sort_unstable();
    assert_eq!(
        collapse_keys,
        [
            "collapsed_away",
            "enabled",
            "mode",
            "mode_requested",
            "representatives",
            "similarity_threshold",
            "source_records",
            "threshold_monotonicity",
        ]
    );
}

#[test]
fn collapsed_output_adds_no_new_exfiltration_surface() {
    // AC9: collapse must not widen what recall emits. The stored body
    // (post-redaction lesson text) is already recall's surface; the collapse
    // layer may add cluster metadata but must not add new free-text fields.
    // Plant sentinel markers that look like raw transcript text, command
    // output, and patch hunks in one candidate's body, then verify (a) the
    // emitted JSON schema is exactly the documented field set, and (b) the
    // sentinels appear ONLY inside `memory_text` values.
    let sentinel_body = "SENTINEL_RAW_TRANSCRIPT_MARKER then ran $ SENTINEL_COMMAND \
         producing @@ -1,2 +1,2 @@ SENTINEL_PATCH_HUNK with sk-SENTINEL-not-a-real-key"
        .to_owned();
    let mut candidates = vec![
        unit_candidate("sentinel-1", Some("0.9"), Some("2026-06-01T00:00:00Z")),
        unit_candidate("sentinel-2", Some("0.8"), Some("2026-06-02T00:00:00Z")),
    ];
    for candidate in &mut candidates {
        candidate.body_text.clone_from(&sentinel_body);
        // Populate every optional row field so the schema allowlist below
        // covers the full field set (serde skips `None`s).
        candidate.agent_id = Some("agent-1".to_owned());
        candidate.agent_kind = Some("test".to_owned());
        candidate.session_id = Some("sess-1".to_owned());
        candidate.retrieval_score = Some(0.9);
        candidate.evidence_links = Some(vec![link("target-1", "codegraph", "OBSERVES", None)]);
    }
    let outcome = collapse_memory_recall(&candidates, &default_config());
    assert_eq!(outcome.clusters.len(), 1);
    let rows = render_collapsed_rows(&outcome.clusters);
    let envelope = collapse_envelope(
        "q",
        outcome.mode,
        "auto",
        outcome.threshold,
        candidates.len(),
        outcome.clusters.len(),
    );

    assert_collapsed_schema(&rows, &envelope);

    // Sentinels are confined to memory_text: strip every memory_text value
    // from the payload, and nothing sentinel-like may remain.
    let mut scrubbed = serde_json::to_string(&envelope).expect("envelope");
    for row in &rows {
        let mut value = serde_json::to_value(row).expect("row");
        if let Some(object) = value.as_object_mut() {
            object.remove("memory_text");
        }
        scrubbed.push('\n');
        scrubbed.push_str(&serde_json::to_string(&value).expect("row"));
    }
    for sentinel in [
        "SENTINEL_RAW_TRANSCRIPT_MARKER",
        "SENTINEL_COMMAND",
        "SENTINEL_PATCH_HUNK",
        "SENTINEL-not-a-real-key",
    ] {
        assert!(
            !scrubbed.contains(sentinel),
            "sentinel {sentinel:?} leaked outside memory_text"
        );
    }
    // Sanity: the sentinels ARE in the stored body surface (recall already
    // emits memory_text), so the confinement check above is non-vacuous.
    assert!(
        rows[0]
            .memory_text
            .contains("SENTINEL_RAW_TRANSCRIPT_MARKER")
    );
}

// ---------------------------------------------------------------------------
// AC10 + success metric: payload drop >= 60%, 5-run byte-identical
// ---------------------------------------------------------------------------

/// Renders the un-collapsed answer the same rows would produce: one JSON
/// object per source record, mirroring the `MemoryRecallResult` row shape the
/// real `eg query semantic-memory` emits (same gates as
/// `candidates_from_store`). Every field the recall answer emits for these
/// records is present, so the ratio measures the product's actual payload
/// win. `retrieval_score` is absent only because the unit harness does no
/// ranking; the CLI end-to-end test covers the scored path (66.7% there).
fn uncollapsed_payload(records: &[GraphRecord]) -> String {
    let mut lines = Vec::new();
    for record in records {
        let GraphRecord::Node {
            text,
            summary,
            agent_id,
            agent_kind,
            session_id,
            observed_at,
            ingested_at,
            confidence,
            source_handle,
            evidence_links,
            ..
        } = record
        else {
            continue;
        };
        if !matches!(
            record.node_kind_name(),
            Some("Observation" | "Decision" | "Failure")
        ) {
            continue;
        }
        if source_handle.is_none() && session_id.is_none() {
            continue;
        }
        let verified = evidence_links.as_ref().is_some_and(|links| {
            links.iter().any(|l| {
                matches!(
                    l.relation.as_str(),
                    "VALIDATED_BY" | "HAS_EVIDENCE" | "PRODUCED_EVIDENCE"
                )
            })
        });
        // Resolved code handles, exactly as the real `MemoryRecallResult`
        // row emits them: the OBSERVES/MENTIONS_SYMBOL links resolve to the
        // cited file node, whose handle is `path::name`.
        let linked_code_handles: Vec<&str> = evidence_links
            .as_ref()
            .map(|links| {
                links
                    .iter()
                    .filter(|l| {
                        matches!(l.relation.as_str(), "OBSERVES" | "MENTIONS_SYMBOL")
                            && l.target_record_id.is_some()
                    })
                    .map(|_| "src/daemon.rs::src/daemon.rs")
                    .collect()
            })
            .unwrap_or_default();
        lines.push(
            serde_json::json!({
                "record_id": record.id(),
                "kind": record.node_kind_name().unwrap_or("Observation"),
                "trust_class": "agent_authored",
                "source_handle": source_handle,
                "agent_id": agent_id,
                "agent_kind": agent_kind,
                "session_id": session_id,
                "confidence": confidence,
                "observed_at": observed_at,
                "ingested_at": ingested_at,
                "review_state": if verified { "verified" } else { "unverified" },
                "redacted": false,
                "linked_code_handles": linked_code_handles,
                "memory_text": text.as_deref().unwrap_or(summary.as_str()),
            })
            .to_string(),
        );
    }
    lines.join("\n")
}

fn collapsed_payload(outcome: &aletheia_egregore::query::CollapseOutcome) -> String {
    let envelope = collapse_envelope(
        "what did we learn?",
        outcome.mode,
        "auto",
        outcome.threshold,
        25,
        outcome.clusters.len(),
    );
    let rows = render_collapsed_rows(&outcome.clusters);
    let mut out = serde_json::to_string(&envelope).expect("envelope");
    for row in &rows {
        out.push('\n');
        out.push_str(&serde_json::to_string(row).expect("row"));
    }
    out
}

#[test]
fn collapsed_payload_drops_at_least_60_percent() {
    let seed = seed_store();
    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());

    let sink = EmbeddedAletheiaSink::open(&seed.data_dir).expect("store opens");
    let records = sink.read_all_records().expect("records read");
    let before = uncollapsed_payload(&records);
    let after = collapsed_payload(&outcome);
    #[allow(clippy::cast_precision_loss)]
    let drop = 1.0 - (after.len() as f64 / before.len() as f64);
    assert!(
        drop >= 0.60,
        "payload must drop >= 60%: before={} after={} drop={:.1}%",
        before.len(),
        after.len(),
        drop * 100.0
    );
}

#[test]
fn collapse_is_byte_identical_across_five_runs() {
    let seed = seed_store();
    let mut payloads = Vec::new();
    for _ in 0..5 {
        let candidates = candidates_from_store(&seed);
        let outcome = collapse_memory_recall(&candidates, &default_config());
        payloads.push(collapsed_payload(&outcome));
    }
    for (i, p) in payloads.iter().enumerate().skip(1) {
        assert_eq!(p, &payloads[0], "run {i} must be byte-identical to run 0");
    }
}

// ---------------------------------------------------------------------------
// AC2: read-only — store fingerprint identical before/after; un-collapsed
// recall unchanged
// ---------------------------------------------------------------------------

#[test]
fn collapse_is_read_only_over_the_store() {
    let seed = seed_store();
    let before = store_fingerprint(&seed.data_dir);

    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());
    let _rows = render_collapsed_rows(&outcome.clusters);

    let after = store_fingerprint(&seed.data_dir);
    assert_eq!(before, after, "collapse must not touch the store");
}

#[test]
fn uncollapsed_recall_is_unchanged_by_collapse() {
    let seed = seed_store();
    let read_records = || {
        EmbeddedAletheiaSink::open(&seed.data_dir)
            .expect("store opens")
            .read_all_records()
            .expect("records read")
    };
    let before = uncollapsed_payload(&read_records());

    // Collapse runs over the same store...
    let candidates = candidates_from_store(&seed);
    let outcome = collapse_memory_recall(&candidates, &default_config());
    let _ = render_collapsed_rows(&outcome.clusters);

    // ...and the un-collapsed answer is byte-identical afterwards.
    let after = uncollapsed_payload(&read_records());
    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// AC1/AC4-degraded: `eg query semantic-memory --collapse` on a store WITHOUT
// embeddings degrades to normalized-text equality (no model needed, so this
// drives the real CLI binary end to end).
// ---------------------------------------------------------------------------

/// For the degraded fixture the five restatements of each claim share
/// identical text (post-normalization), so normalized-text equality clusters
/// them without any vectors.
fn seed_store_degraded_text() -> Seed {
    // Rewrite the 25 records with identical text per claim via the JSONL path
    // is unnecessary: memory_node already varies text per restatement, so
    // instead rebuild the store with claim-identical bodies here.
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let target = file_node("src/daemon.rs");
    let target_id = target.id().to_owned();
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("store opens");
    sink.write_record(&target).expect("target writes");
    for (claim, claim_texts) in CLAIMS.iter().enumerate() {
        for rest in 0..5 {
            let mut node = memory_node(claim, rest, &target_id, claim == 2 && rest < 2);
            if let GraphRecord::Node {
                text: ref mut t, ..
            } = node
            {
                // Same normalized body for all five restatements of a claim;
                // raw text varies in case/whitespace to prove normalization.
                let raw = if rest % 2 == 0 {
                    claim_texts[0].to_uppercase()
                } else {
                    format!("  {}  ", claim_texts[0])
                };
                *t = Some(raw);
            }
            sink.write_record(&node).expect("record writes");
        }
    }
    drop(sink);
    Seed {
        _temp: temp,
        data_dir,
    }
}

fn run_collapse_cli(data_dir: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = egregore();
    cmd.arg("query")
        .arg("semantic-memory")
        .arg("what did past sessions learn?")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--collapse")
        .arg("--format")
        .arg("json");
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("cli runs")
}

#[test]
fn cli_collapse_degrades_to_normalized_text_without_embeddings() {
    let seed = seed_store_degraded_text();
    let before = store_fingerprint(&seed.data_dir);

    let output = run_collapse_cli(
        &seed.data_dir,
        &["--collapse-mode", "normalized-text", "--limit", "25"],
    );
    assert!(
        output.status.success(),
        "degraded collapse must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let mut lines = stdout.lines();
    let envelope: serde_json::Value =
        serde_json::from_str(lines.next().expect("envelope line")).expect("envelope json");
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["collapse"]["mode"], "normalized-text");
    assert_eq!(envelope["collapse"]["mode_requested"], "normalized-text");
    let threshold = envelope["collapse"]["similarity_threshold"]
        .as_f64()
        .expect("threshold is a number");
    assert!(
        (threshold - f64::from(DEFAULT_COLLAPSE_SIMILARITY_THRESHOLD)).abs() < 1e-6,
        "threshold echoed verbatim, got {threshold}"
    );
    assert_eq!(envelope["collapse"]["source_records"], 25);
    assert_eq!(envelope["collapse"]["representatives"], 5);

    let mut member_ids: Vec<String> = Vec::new();
    let mut rows = 0;
    for line in lines {
        let row: serde_json::Value = serde_json::from_str(line).expect("row json");
        rows += 1;
        assert_eq!(row["cluster_size"], 5);
        assert!(
            row["retrieval_score"].is_null(),
            "no ranking without vectors"
        );
        let members = row["member_ids"].as_array().expect("member_ids");
        assert_eq!(members.len(), 5);
        assert_eq!(
            members[0], row["record_id"],
            "representative is first in member_ids"
        );
        assert!(row["cluster_observed_at_min"].is_string());
        assert!(row["cluster_observed_at_max"].is_string());
        assert!(row["trust_spread"].is_object());
        for m in members {
            member_ids.push(m.as_str().expect("id").to_owned());
        }
    }
    assert_eq!(rows, 5, "five claims -> five representatives");
    member_ids.sort();
    member_ids.dedup();
    assert_eq!(member_ids.len(), 25, "100% of record IDs retrievable");

    // AC2 at the workflow level: the store is untouched.
    let after = store_fingerprint(&seed.data_dir);
    assert_eq!(before, after, "collapsed recall must not modify the store");
}

#[test]
fn cli_collapse_resolves_triple_only_evidence_links() {
    // Issue #163: a memory record citing its target only via the
    // (path, span, commit) triple — the shape CLI ingest stores, since it
    // does not canonicalize triples at write time — must resolve to the same
    // target record ID as a direct-ID citation, so the two collapse together
    // instead of stranding the triple-only record as a singleton.
    let temp = tempfile::tempdir().expect("temp dir");
    let data_dir = temp.path().join("store");
    let commit = "abc123def456";
    let mut target = file_node("src/triple.rs");
    if let GraphRecord::Node {
        temporal: ref mut t,
        ..
    } = target
    {
        *t = Some(TemporalMetadata {
            git_commit: commit.to_owned(),
            git_parent_commits: Vec::new(),
            valid_time: "2026-06-01T00:00:00Z".to_owned(),
            author_time: None,
            observed_at: "2026-06-01T00:00:00Z".to_owned(),
            valid_time_source: None,
        });
    }
    let target_id = target.id().to_owned();
    let mut sink = EmbeddedAletheiaSink::open(&data_dir).expect("store opens");
    sink.write_record(&target).expect("target writes");

    // Record A cites the target by direct record ID; record B cites the same
    // target via the triple only. Identical normalized text.
    let mut a = memory_node(0, 0, &target_id, false);
    let mut b = memory_node(0, 1, &target_id, false);
    for node in [&mut a, &mut b] {
        if let GraphRecord::Node { text: t, .. } = node {
            *t = Some("the daemon holds a lease file".to_owned());
        }
    }
    if let GraphRecord::Node {
        evidence_links: ref mut el,
        ..
    } = b
    {
        *el = Some(vec![triple_link("src/triple.rs", span(1, 100), commit)]);
    }
    sink.write_record(&a).expect("record writes");
    sink.write_record(&b).expect("record writes");
    drop(sink);
    let seed = Seed {
        _temp: temp,
        data_dir,
    };

    let output = run_collapse_cli(
        &seed.data_dir,
        &["--collapse-mode", "normalized-text", "--limit", "25"],
    );
    assert!(
        output.status.success(),
        "triple collapse must exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let mut lines = stdout.lines();
    let envelope: serde_json::Value =
        serde_json::from_str(lines.next().expect("envelope line")).expect("envelope json");
    assert_eq!(envelope["collapse"]["source_records"], 2);
    assert_eq!(
        envelope["collapse"]["representatives"], 1,
        "direct-ID and triple-only citations of the same target collapse together"
    );
    let row: serde_json::Value =
        serde_json::from_str(lines.next().expect("row line")).expect("row json");
    assert_eq!(row["cluster_size"], 2);
    assert_eq!(row["member_ids"].as_array().expect("member_ids").len(), 2);
    assert_eq!(
        row["primary_cited_target"]["record_id"], target_id,
        "representative names the resolved target"
    );
}

#[test]
fn cli_collapse_fail_closed_on_bad_threshold_and_forced_embedding_mode() {
    // Both run against a store WITHOUT embeddings: an out-of-range threshold
    // must be rejected up front, and forcing embedding-cosine must be refused
    // rather than silently falling back to normalized-text.
    let seed = seed_store_degraded_text();

    let bad_threshold = run_collapse_cli(
        &seed.data_dir,
        &[
            "--collapse-mode",
            "normalized-text",
            "--similarity-threshold",
            "1.5",
            "--limit",
            "25",
        ],
    );
    assert!(
        !bad_threshold.status.success(),
        "threshold 1.5 must be rejected"
    );
    let stderr = String::from_utf8_lossy(&bad_threshold.stderr);
    assert!(
        stderr.contains("--similarity-threshold must lie in [0.0, 1.0]"),
        "stable rejection message, got: {stderr}"
    );

    let forced = run_collapse_cli(
        &seed.data_dir,
        &["--collapse-mode", "embedding-cosine", "--limit", "25"],
    );
    assert!(
        !forced.status.success(),
        "forced embedding-cosine without an index must be refused"
    );
    let stderr = String::from_utf8_lossy(&forced.stderr);
    assert!(
        stderr.contains("no vector index"),
        "refusal must name the missing index, got: {stderr}"
    );
}

#[test]
fn cli_collapse_is_byte_identical_across_five_runs() {
    let seed = seed_store_degraded_text();
    let mut payloads = Vec::new();
    for _ in 0..5 {
        let output = run_collapse_cli(
            &seed.data_dir,
            &["--collapse-mode", "normalized-text", "--limit", "25"],
        );
        assert!(output.status.success());
        payloads.push(output.stdout);
    }
    for (i, p) in payloads.iter().enumerate().skip(1) {
        assert_eq!(p, &payloads[0], "CLI run {i} must be byte-identical");
    }
}

#[test]
fn cli_without_collapse_still_refuses_store_without_embeddings() {
    // Degradation is opt-in via --collapse; plain recall keeps its existing
    // exit-2 contract on a store without embeddings.
    let seed = seed_store_degraded_text();
    let output = egregore()
        .arg("query")
        .arg("semantic-memory")
        .arg("what did past sessions learn?")
        .arg("--data-dir")
        .arg(&seed.data_dir)
        .output()
        .expect("cli runs");
    assert_eq!(output.status.code(), Some(2));
}

// ---------------------------------------------------------------------------
// AC1/AC4-embedding: full `eg` workflow with real embeddings (model-gated).
// ---------------------------------------------------------------------------

/// Builds the fixture as JSONL and ingests it with the real local embedding
/// model (`eg ingest --embed`). Skips gracefully when the model cannot load
/// (e.g. no network for the first download and nothing cached): the
/// injected-vector tests above already prove the clustering contract without
/// a model.
fn seed_store_real_embeddings() -> Option<Seed> {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph_path = temp.path().join("collapse.graph.jsonl");
    let data_dir = temp.path().join("store");

    let target = file_node("src/daemon.rs");
    let target_id = target.id().to_owned();
    let mut graph = Graph::new();
    graph.push(GraphRecord::node(
        stable_id(&["repository", "operator-override", "collapse-fixture"]),
        NodeKind::Repository,
        None,
        None,
        Some("collapse-fixture".to_owned()),
        "Repository collapse-fixture".to_owned(),
    ));
    graph.push(target);
    graph.push(verification_node());
    for claim in 0..5 {
        for rest in 0..5 {
            graph.push(memory_node(claim, rest, &target_id, claim == 2 && rest < 2));
        }
    }
    std::fs::write(&graph_path, graph.to_jsonl().expect("jsonl")).expect("write");

    let ingest = egregore()
        .arg("ingest")
        .arg(&graph_path)
        .arg("--adapter")
        .arg("embedded")
        .arg("--data-dir")
        .arg(&data_dir)
        .arg("--embed")
        .output()
        .expect("ingest runs");
    if !ingest.status.success() {
        let stderr = String::from_utf8_lossy(&ingest.stderr);
        if stderr.contains("failed to load embedding model") || stderr.contains("failed to embed") {
            eprintln!(
                "SKIP: embedding model unavailable ({})",
                stderr.lines().next().unwrap_or("")
            );
            return None;
        }
        panic!(
            "ingest --embed failed: {}",
            String::from_utf8_lossy(&ingest.stderr)
        );
    }
    Some(Seed {
        _temp: temp,
        data_dir,
    })
}

fn run_semantic_memory(data_dir: &Path, collapse: bool) -> std::process::Output {
    let mut cmd = egregore();
    cmd.arg("query")
        .arg("semantic-memory")
        .arg("what lessons did past sessions learn about the daemon and parser?")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--limit")
        .arg("25")
        .arg("--format")
        .arg("json");
    if collapse {
        cmd.arg("--collapse");
        // An explicit threshold keeps the end-to-end workflow deterministic
        // across model versions: the pinned default (0.85) is proven by the
        // injected-vector tests above, where cosines are exact.
        cmd.arg("--similarity-threshold").arg("0.5");
    }
    cmd.output().expect("cli runs")
}

#[test]
fn cli_collapse_embedding_cosine_end_to_end() {
    let Some(seed) = seed_store_real_embeddings() else {
        return;
    };
    let before = store_fingerprint(&seed.data_dir);

    // Un-collapsed baseline: every original record, unchanged.
    let plain = run_semantic_memory(&seed.data_dir, false);
    assert!(
        plain.status.success(),
        "un-collapsed recall: {}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain_text = String::from_utf8(plain.stdout).expect("utf8");
    let plain_rows: Vec<&str> = plain_text.lines().collect();
    assert_eq!(
        plain_rows.len(),
        25,
        "un-collapsed recall returns all 25 records"
    );
    let plain_bytes: usize = plain_rows.iter().map(|l| l.len() + 1).sum();

    // Collapsed answer.
    let collapsed = run_semantic_memory(&seed.data_dir, true);
    assert!(
        collapsed.status.success(),
        "collapsed recall: {}",
        String::from_utf8_lossy(&collapsed.stderr)
    );
    let stdout = String::from_utf8(collapsed.stdout).expect("utf8");
    let mut lines = stdout.lines();
    let envelope: serde_json::Value =
        serde_json::from_str(lines.next().expect("envelope")).expect("envelope json");
    assert_eq!(envelope["collapse"]["mode"], "embedding-cosine");
    assert_eq!(envelope["collapse"]["mode_requested"], "auto");
    let threshold = envelope["collapse"]["similarity_threshold"]
        .as_f64()
        .expect("threshold is a number");
    assert!(
        (threshold - 0.5).abs() < 1e-6,
        "threshold echoed verbatim, got {threshold}"
    );

    let mut member_ids: Vec<String> = Vec::new();
    let mut rows = 0;
    let mut rep_confidences: BTreeMap<String, f32> = BTreeMap::new();
    for line in lines {
        let row: serde_json::Value = serde_json::from_str(line).expect("row json");
        rows += 1;
        assert!(row["cluster_size"].as_u64().unwrap() >= 1);
        assert!(row["retrieval_score"].as_f64().is_some());
        let members = row["member_ids"].as_array().expect("member_ids");
        assert_eq!(members[0], row["record_id"]);
        rep_confidences.insert(
            row["record_id"].as_str().unwrap().to_owned(),
            row["confidence"].as_str().unwrap().parse::<f32>().unwrap(),
        );
        for m in members {
            member_ids.push(m.as_str().unwrap().to_owned());
        }
    }
    assert!(
        rows <= 5,
        "five claims -> at most five representatives, got {rows}"
    );
    member_ids.sort();
    member_ids.dedup();
    assert_eq!(
        member_ids.len(),
        25,
        "100% of record IDs retrievable via member lists"
    );
    // Every representative is the highest-confidence member of its cluster.
    for (rep, conf) in &rep_confidences {
        assert!(
            *conf >= 0.94,
            "representative {rep} must be the 0.95-confidence restatement, got {conf}"
        );
    }

    let collapsed_bytes = stdout.len();
    #[allow(clippy::cast_precision_loss)]
    let drop = 1.0 - (collapsed_bytes as f64 / plain_bytes as f64);
    assert!(
        drop >= 0.60,
        "payload must drop >= 60%: plain={plain_bytes} collapsed={collapsed_bytes} drop={:.1}%",
        drop * 100.0
    );

    // AC2: the workflow is read-only.
    let after = store_fingerprint(&seed.data_dir);
    assert_eq!(before, after, "collapsed recall must not modify the store");

    // AC10: byte-identical across 5 runs.
    let mut payloads = Vec::new();
    for _ in 0..5 {
        let out = run_semantic_memory(&seed.data_dir, true);
        assert!(out.status.success());
        payloads.push(out.stdout);
    }
    for (i, p) in payloads.iter().enumerate().skip(1) {
        assert_eq!(p, &payloads[0], "run {i} must be byte-identical");
    }
}

#[test]
fn cli_explicit_normalized_text_on_embedded_store_never_loads_a_model() {
    // Regression test for the routing bug where explicit
    // `--collapse-mode normalized-text` on an embedded store fell through to
    // the semantic path and loaded the embedding model. The local collapse
    // path is the only one that emits `mode: normalized-text` with a null
    // `retrieval_score` (the semantic path always attaches a ranking score),
    // so those two observables prove no model was touched.
    let Some(seed) = seed_store_real_embeddings() else {
        return;
    };

    let mut cmd = egregore();
    cmd.arg("query")
        .arg("semantic-memory")
        .arg("what did past sessions learn?")
        .arg("--data-dir")
        .arg(&seed.data_dir)
        .arg("--collapse")
        .arg("--collapse-mode")
        .arg("normalized-text")
        .arg("--limit")
        .arg("25")
        .arg("--format")
        .arg("json");
    let output = cmd.output().expect("cli runs");
    assert!(
        output.status.success(),
        "explicit normalized-text must exit 0 on an embedded store: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8");
    let mut lines = stdout.lines();
    let envelope: serde_json::Value =
        serde_json::from_str(lines.next().expect("envelope")).expect("envelope json");
    assert_eq!(envelope["collapse"]["mode"], "normalized-text");
    assert_eq!(envelope["collapse"]["mode_requested"], "normalized-text");

    let mut rows = 0;
    let mut member_ids: Vec<String> = Vec::new();
    for line in lines {
        let row: serde_json::Value = serde_json::from_str(line).expect("row json");
        rows += 1;
        assert!(
            row["retrieval_score"].is_null(),
            "local collapse path attaches no ranking score"
        );
        for m in row["member_ids"].as_array().expect("member_ids") {
            member_ids.push(m.as_str().unwrap().to_owned());
        }
    }
    // This fixture's restatements differ in text, so normalized-text equality
    // leaves all 25 as singletons; the routing proof is the mode pair plus the
    // null ranking scores, not the cluster count.
    assert_eq!(rows, 25, "no text-equal pairs -> 25 singleton rows");
    member_ids.sort();
    member_ids.dedup();
    assert_eq!(member_ids.len(), 25, "100% of record IDs retrievable");
}
