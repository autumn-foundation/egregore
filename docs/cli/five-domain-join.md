# Five-domain context join — acceptance gate (issue #252)

Egregore's headline promise is that code facts, agent memory, project state,
artifacts, and verification evidence live in **one graph** where the
relationships are the product. This page is the runnable proof: a single
`eg`-only workflow (no library-internal record seeding, no hosted indexing,
no remote crawling) that builds a five-domain store from the checked-in
fixture and shows one `eg query context` call returning all five
trust-separated sections non-empty, every item citable, with trust separation
holding and dangling citations surfaced instead of silently dropped.

The same workflow is enforced by the test suite:
`tests/integration/five_domain_context_join.rs` runs every command below
through the real `eg` binary, asserts all five sections co-populate, asserts
100% citable handles, asserts disjoint trust sections, asserts the dangling
citation lands in `unresolved`, and asserts byte-stable sections across 5
consecutive runs. If any cross-domain edge regresses, the gate goes red.

## The fixture

`tests/fixtures/five_domain_join/` — a dependency-free Rust crate (builds
fully offline) exposing one probe symbol, `five_domain_probe`, plus:

| File | Role |
|------|------|
| `src/lib.rs` | The probe symbol + a unit test sharing its final name segment |
| `tasks/join-probe.jsonl` | Local-task source for `eg import-local-tasks` (project domain) |
| `fix.diff` | Unified diff for `eg write artifact` (artifact domain) |
| `egregore.toml` | Pins `[scan] transaction_time` so the gate is byte-deterministic |

## Golden path

Run from the fixture directory (the `egregore.toml` there pins the scan
timestamp; every other timestamp is passed explicitly, so re-running this
verbatim is byte-stable):

```sh
cd tests/fixtures/five_domain_join

# 1. Code facts: deterministic code-graph records for the probe symbol.
eg scan . --repo-id-override five-domain-join --out /tmp/join/code.jsonl

# 2. Agent memory: one observation citing the probe symbol, plus one
#    intentionally dangling citation (shaped like a codegraph ID, absent
#    from the store) to prove the gate distinguishes "linked" from
#    "claimed but unresolved".
SYMBOL_ID=$(eg query symbol five_domain_probe --graph /tmp/join/code.jsonl \
  | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["record_id"])')
eg write observation \
  --agent-id gate-agent --agent-kind other --session-id gate-session-1 \
  --observed-at 2026-09-16T05:30:00Z \
  --source-handle "gate-fixture:five_domain_probe" \
  --text "five_domain_probe returns the join sentinel; keep it stable." \
  --confidence 0.9 \
  --evidence-target "$SYMBOL_ID" \
  --evidence-target codegraph:v9:0000000000000000000000000000000000000000000000000000000000000000 \
  --out /tmp/join/observation.jsonl

# 3. Project state: one task + acceptance criterion via the documented
#    task-import path. The failure below references the task, which is what
#    pulls it into the symbol's project_state section.
eg import-local-tasks tasks --repo-root . \
  --transaction-time 2026-09-16T05:30:00Z \
  --out /tmp/join/tasks.jsonl
TASK_ID=$(grep '"kind":"Task"' /tmp/join/tasks.jsonl \
  | python3 -c 'import json,sys; print(json.loads(sys.stdin.readline())["id"])')
eg write failure \
  --agent-id gate-agent --agent-kind human --session-id gate-session-1 \
  --observed-at 2026-09-16T05:30:00Z \
  --source-handle "gate-fixture:five_domain_probe" \
  --failure-kind assumption_rejected \
  --text "Assumed the probe needed no test; the captured run below rejects that." \
  --failed-on "$SYMBOL_ID" \
  --references-task "$TASK_ID" \
  --out /tmp/join/failure.jsonl

# 4. Artifact: a patch artifact touching the probe's file, linked to the
#    code graph by eg link-evidence (TOUCHED_FILE edge).
eg write artifact \
  --agent-id gate-agent --agent-kind other --session-id gate-session-1 \
  --observed-at 2026-09-16T05:30:00Z \
  --patch-file fix.diff --source-artifact-path fix.diff \
  --source-artifact-hash 4106cd1be94c83cebd44c5ab1a16828841da7c857a92b6d85d454d9c1005235a \
  --validation-summary "Fixture patch: comment-only change to five_domain_probe; applies cleanly." \
  --target-file src/lib.rs \
  --out /tmp/join/artifact.jsonl
eg link-evidence \
  --code-graph /tmp/join/code.jsonl \
  --evidence /tmp/join/artifact.jsonl \
  --out /tmp/join/links.jsonl

# 5. Verification evidence: run the fixture's real tests, capture the
#    libtest stream, and anchor the TestRun to the probe symbol with --graph
#    (mints TestRun --MENTIONS_SYMBOL--> Symbol; no fabricated streams).
#    NOTE: the libtest JSON event stream is nightly-only (the JSON emitter is
#    baked into the test binary at compile time), so the fixture is built by
#    nightly here — the same requirement docs/cli/capture-tests.md already
#    documents for this workflow.
RUSTUP_TOOLCHAIN=nightly CARGO_TARGET_DIR=/tmp/join/target CARGO_NET_OFFLINE=true \
  cargo test --manifest-path Cargo.toml -- -Z unstable-options --format json > /tmp/join/run.json
eg capture-tests \
  --input /tmp/join/run.json --out /tmp/join/testrun.jsonl \
  --session-id gate-session-1 \
  --commit 0123456789abcdef0123456789abcdef01234567 \
  --suite five-domain-join --command "cargo test" --exit-code 0 \
  --executed-at 2026-09-16T05:30:00Z \
  --graph /tmp/join/code.jsonl

# 6. One store, one question.
cat /tmp/join/code.jsonl /tmp/join/tasks.jsonl /tmp/join/artifact.jsonl \
    /tmp/join/testrun.jsonl /tmp/join/observation.jsonl \
    /tmp/join/failure.jsonl /tmp/join/links.jsonl > /tmp/join/store.jsonl
eg query context five_domain_probe --graph /tmp/join/store.jsonl
```

## What the answer proves

The answer envelope carries the five trust-separated sections over one
symbol:

| Section | Trust class | Populated by |
|---------|-------------|--------------|
| `source_facts` | `source_derived` | `eg scan`: the `Symbol` + its `File` via `DEFINES` |
| `observations` | `agent_unverified` | `eg write observation` (`OBSERVES` evidence link) and `eg write failure` (`FAILED_ON` evidence link) |
| `project_state` | `project_state` | the `Task` cited by the failure's `REFERENCES_TASK` link |
| `artifacts` | `artifact` | the `PatchArtifact` via `eg link-evidence`'s `TOUCHED_FILE` edge |
| `verification_evidence` | `verification_evidence` | the `TestRun` via `eg capture-tests --graph`'s `MENTIONS_SYMBOL` edge |

And the diagnostics:

- **`unresolved`** holds the intentionally dangling
  `codegraph:v9:0000…` citation from the observation — the gate fails if a
  claimed-but-absent target is ever silently dropped.
- **Trust separation**: no `record_id` appears in both `source_facts` and any
  other section — an agent-authored guess can never masquerade as a source
  fact. The gate asserts the ID sets are disjoint.
- **Citable handles**: every item in every populated section carries a
  `record_id` plus a repo-relative file/span or commit handle (`span`,
  `repo_relative_path`, `target_files`, `git_commit`, `base_commit`, or an
  evidence-link `target_record_id`). The gate fails on any item without one —
  fast wrong answers are confetti.
- **Determinism**: re-running the six steps verbatim yields byte-identical
  section membership and ordering (sections sort by `record_id`); the
  integration gate runs the whole workflow 5 times and diffs the answers.

## Scope notes

- This gate introduces no new graph domain, query lane, edge vocabulary,
  embedding model, or language coverage — it only proves the existing
  commands compose (acceptance criterion 8).
- No latency bar here: time-to-first-answer is owned by issue #57.
- Empty-section semantics (domain-absent vs entity-absent labeling) are owned
  by issue #196; this gate asserts the populated case.
