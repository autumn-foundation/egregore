# Issue #224 — Incrementally update history from new commits (SPEC)

## Goal

`scan-history` can resume from the last-ingested commit for a repository
identity, processing only new commits. A resumed run over N new commits on an
M-commit repo produces output **byte-identical** to a full replay of all M+N
commits; a resumed run with zero new commits is a no-op.

## Design

### Resume marker: `HistoryReplayTip` node (issue #224)

Every **full** (unwindowed) `scan-history` stamps one `HistoryReplayTip` node,
carrying `HistoryReplayTipPayload`:

- `repository_id` — stable repository identity this tip belongs to.
- `tip_sha` — newest commit covered (inclusive).
- `covered_commit_count` — commits covered by this replay (frontier size).
- `tip_committed_at` — tip committer date, UTC `Z` RFC 3339.

Stable ID: `stable_id(&["node", "history-replay-tip", &repository_id])` — one
node per repository identity, so a resumed run **upserts** it (same ID,
refreshed payload). Attached to `Repository` by a `CONTAINS` edge (citable,
never orphan, `eg validate`-clean — same treatment as `HistoryReplayWindow`).
`valid_time` inferred from the transaction time (= tip committer date).
Redaction-exempt deterministic code-graph data (SHAs, counts, one instant —
no paths, no PII), `TrustClass::SourceDerived` like the window node.

Windowed replays stamp **no** tip: a windowed frontier does not represent full
history, so it is never a valid resume base. `--resume-from` combined with any
window flag is refused (`resume_with_window`).

### Library API (`src/history.rs`)

```rust
pub struct HistoryResumePoint {
    pub repository_id: String,   // must equal the current repo's identity
    pub tip_sha: String,          // prior replay's HEAD (inclusive)
    pub prior_records: Vec<GraphRecord>, // frontier records (multi-repo OK)
}

pub struct HistoryResumeOutcome {
    pub graph: Graph,      // merged graph; to_jsonl == full replay at new HEAD
    pub processed: usize,  // commits newly processed
    pub skipped: usize,    // commits already represented (not re-walked)
    pub new_tip_sha: String,
}

pub fn history_resume_point(
    records: &[GraphRecord],
    repository_id: &str,
) -> Option<HistoryResumePoint>

pub fn scan_repository_history_resumed(
    repo_path: impl AsRef<Path>,
    repo_id_override: Option<&str>,
    resume: HistoryResumePoint,
) -> Result<HistoryResumeOutcome>
```

### Resume algorithm

1. Compute the repository identity; require `resume.repository_id ==
   identity.id` (`repository_identity_mismatch` otherwise).
2. Ancestry check: `git merge-base --is-ancestor <tip> HEAD`. Non-ancestor
   (force-push, rebase, or an unknown tip SHA) → `history_rewrite_detected`
   (machine-readable code; documented recovery is a full replay — the run
   never produces a forked timeline).
3. New commits: `git rev-list --reverse --topo-order <tip>..HEAD` (+ metadata).
   - Empty → no-op: `processed = 0`, `skipped = <prior covered count>`. The
     merged graph is still byte-identical to the frontier (same HEAD ⇒ same
     transaction time ⇒ identical per-run stamps and producer envelope), so
     the caller may skip the write entirely.
   - Non-empty → replay **only** those commits through the same per-commit
     record builder the full path uses (refactored, not duplicated).
4. Merge: prior records minus the two per-run stamps (Repository node,
   old tip node — both stable IDs) plus the new records (fresh Repository
   node, fresh tip node, new-commit records), then re-stamp the producer
   envelope over **all** records with the new transaction time.

### Why byte-identity holds

- `Graph::to_jsonl` sorts serialized lines ⇒ merge order is irrelevant; only
  the record **multiset** matters.
- Every per-commit record is a pure function of (commit SHA, parent trees,
  blob contents). The manifest-OID memo is order-independent. Slice-scoped
  passes (issue #134 labeling, #223/#238 test scope, #117 attribution) key
  off `graph.records().len()` at slice start — correct regardless of prior
  records present.
- The two per-run stamps (Repository `source_snapshot`, tip node) are
  re-emitted with the new HEAD; with zero new commits they re-emit
  identically.
- `stamp_producer` **overwrites** the envelope on every record, so prior
  records pick up the new `producer_started_at` (= new HEAD committer date,
  not wall-clock).
- Redaction stays CLI-level and runs over the merged set — same input set as
  a full replay ⇒ same output.

### CLI: `scan-history --resume-from <prior.jsonl>`

- Reads the frontier, extracts the tip for the current repo identity.
- Codes (single JSON line on stderr, exit 2, no partial output):
  - `resume_with_window` — window flag + `--resume-from`.
  - `invalid_frontier` — frontier JSONL unreadable.
  - `no_resume_point` — no tip for this repo identity; recovery: full replay.
  - `history_rewrite_detected` — tip not an ancestor of HEAD; recovery: full
    replay (documented; never a forked timeline).
- No-op (`processed == 0`): the `--out` file is **not written** (left
  untouched); report still printed.
- Report (stderr, one JSON line, always on a resume run):
  `{"code":"history_resume","repository_id":"…","tip":"…","new_tip":"…",
  "processed":N,"skipped":M,"noop":bool,"out":"…"}`.

### Multi-repo (AC4)

The tip is extracted **per repository identity** from the frontier, so one
frontier/store can advance each repo's history independently; other repos'
records pass through the merge untouched.

### Verification

- Unit: tip node stamped on full replay (id, payload, CONTAINS edge);
  `history_resume_point` extraction incl. per-identity selection and None
  case; rewrite detection; processed/skipped counts.
- Integration (fixture repo): M commits → full replay F(M). N more commits →
  resumed R(M+N). Fresh full replay F(M+N). Assert `R == F` **byte-for-byte**.
  Zero-new-commit resume: frontier untouched (byte-identical, no write).
- `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features
  -- -D warnings`, full test matrix per AGENTS.md.

### Out of scope (per issue)

- Resume from the embedded store directly (the JSONL frontier is the resume
  surface; the store ingests the merged JSONL unchanged).
- Reconciling rewritten history in place (falls back to full replay).
- Daemon/file-watching triggers, working-tree edits (#98), read-side delta
  feed (#197).
