//! Anti-drift registry for the CAPTURE-ONLY verification lanes (issue #230).
//!
//! `eg`'s `capture-*` top-level subcommands turn existing machine-readable
//! tool artefacts into citable `verification`-domain records. They never
//! execute the tool that produced the artefact: the caller runs the tool and
//! hands the artefact file here. Each lane documents its workflow, flags,
//! records, exit codes, and redaction contract in `docs/cli/<lane>.md`.
//!
//! This module mirrors the query-lane `LANE_TABLE` contract (issue #251,
//! `super::lanes`): the per-lane classification is a static table
//! ([`CAPTURE_LANE_TABLE`]) keyed by the lane's clap subcommand name, and
//! the drift tests below fail CI when a `capture-*` subcommand ships
//! without a table entry (orphan), a table entry names a subcommand that
//! no longer exists (phantom), or an entry's doc file is missing or does
//! not name its lane.
//!
//! Scope: lanes whose clap name starts with `capture-` — the
//! verification-evidence capture lanes. The `capture` protected-artifact
//! storage command is a different kind (a storage operator, not a
//! verification-evidence producer) and is out of scope.
//!
//! Exception: `capture-proof` (issue #69) EXECUTES the verifier binary
//! instead of ingesting a pre-made artefact — the proof only exists after
//! the run. It is still a `capture-*` lane for the anti-drift contract
//! (it emits `verification`-domain records from a tool run), but its doc
//! states the execution contract explicitly.

// The table is a first-class registry consumed by the drift tests below;
// a future `eg capture lanes` command could render it the way
// `eg query lanes` renders LANE_TABLE.
#![allow(dead_code)]

/// Static per-lane classification: claims about the lane's evidence
/// contract, grounded in the lane's docs and implementation.
#[derive(Debug, Clone, Copy)]
struct CaptureLaneStatic {
    /// Doc file, relative to the repository root, documenting the lane's
    /// workflow, flags, emitted records, exit codes, and redaction contract.
    doc: &'static str,
    /// Verification-domain node kind the lane emits for each captured run.
    record_kind: &'static str,
}

/// Per-lane classification table, keyed by stable clap lane id
/// (alphabetical). See the module docs for the anti-drift contract.
const CAPTURE_LANE_TABLE: &[(&str, CaptureLaneStatic)] = &[
    (
        "capture-bench",
        CaptureLaneStatic {
            doc: "docs/cli/capture-bench.md",
            record_kind: "BenchmarkRun",
        },
    ),
    (
        "capture-coverage",
        CaptureLaneStatic {
            doc: "docs/cli/capture-coverage.md",
            record_kind: "CoverageReport",
        },
    ),
    (
        "capture-proof",
        CaptureLaneStatic {
            doc: "docs/cli/capture-proof.md",
            record_kind: "ProofResult",
        },
    ),
    (
        "capture-tests",
        CaptureLaneStatic {
            doc: "docs/cli/capture-tests.md",
            record_kind: "TestRun",
        },
    ),
];

/// Static classification for a lane id, if the table lists it.
fn classify(id: &str) -> Option<CaptureLaneStatic> {
    CAPTURE_LANE_TABLE
        .iter()
        .find(|(key, _)| *key == id)
        .map(|(_, meta)| *meta)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use clap::Subcommand;

    use super::super::Commands;
    use super::*;

    /// Run a closure on a thread with a 64 MiB stack. Debug builds of
    /// clap's `augment_subcommands` for the 100+-variant `Commands` enum
    /// use tens of MB of stack, overflowing the default 2 MiB test-thread
    /// stack.
    fn with_big_stack<T>(f: impl FnOnce() -> T + Send + 'static) -> T
    where
        T: Send + 'static,
    {
        std::thread::Builder::new()
            .name("capture-lanes-big-stack".to_owned())
            .stack_size(64 * 1024 * 1024)
            .spawn(f)
            .expect("failed to spawn big-stack thread")
            .join()
            .expect("big-stack thread panicked")
    }

    /// Clap-derived capture-lane ids: every top-level subcommand whose name
    /// starts with `capture-` (the verification-evidence capture lanes).
    fn clap_capture_ids() -> BTreeSet<String> {
        with_big_stack(|| {
            <Commands as Subcommand>::augment_subcommands(clap::Command::new("eg"))
                .get_subcommands()
                .filter(|sub| sub.get_name().starts_with("capture-"))
                .map(|sub| sub.get_name().to_owned())
                .collect()
        })
    }

    #[test]
    fn table_covers_capture_surface_exactly() {
        let table_ids: BTreeSet<&str> = CAPTURE_LANE_TABLE.iter().map(|(key, _)| *key).collect();
        let clap_ids = clap_capture_ids();
        for id in &clap_ids {
            assert!(
                table_ids.contains(id.as_str()),
                "orphan capture lane {id}: implemented but unlisted — add a CAPTURE_LANE_TABLE entry"
            );
        }
        for id in &table_ids {
            assert!(
                clap_ids.contains(*id),
                "phantom entry {id}: listed but unimplemented — remove it from CAPTURE_LANE_TABLE"
            );
        }
    }

    #[test]
    fn doc_files_exist_and_name_their_lane() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for (id, meta) in CAPTURE_LANE_TABLE {
            let path = root.join(meta.doc);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|_| panic!("missing doc file for lane {id}: {}", meta.doc));
            assert!(
                !text.trim().is_empty(),
                "empty doc file for lane {id}: {}",
                meta.doc
            );
            assert!(
                text.contains(id),
                "doc file {} does not name its lane {id}",
                meta.doc,
            );
            // The classification is load-bearing: a missing entry must fail
            // the lookup, not silently classify.
            assert!(
                classify(id).is_some(),
                "classify({id}) returned None despite a table entry"
            );
        }
        assert!(
            classify("capture-nope").is_none(),
            "classify must return None for unlisted lanes"
        );
    }

    #[test]
    fn record_kinds_match_the_verification_schema_vocabulary() {
        // Membership in the verification node-kind vocabulary from
        // docs/schema/verification.md §2: a capture lane may only claim to
        // emit a kind the schema defines.
        const KINDS: &[&str] = &[
            "CommandRun",
            "Verification",
            "TestRun",
            "CIStatus",
            "BenchmarkRun",
            "CoverageReport",
            "ProofResult",
        ];
        for (id, meta) in CAPTURE_LANE_TABLE {
            assert!(
                KINDS.contains(&meta.record_kind),
                "lane {id} claims unknown record kind {}",
                meta.record_kind
            );
        }
    }
}
