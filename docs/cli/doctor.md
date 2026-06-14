# `eg doctor` — Local Setup Preflight Report

`eg doctor` reports whether this machine is ready for the documented `eg` scan →
ingest → semantic-search workflow **before** agents rely on it. It is the first
command to run on a fresh machine or after a toolchain change.

```
eg doctor [PATH] [--out <path>] [--data-dir <path>] [--require-history] [--network] [--format json|text]
```

Exit **0** when all required checks pass. Exit **1** when any required check fails.

---

## Quick start

```sh
# Check the current directory (default PATH is ".")
eg doctor

# Check a specific repository
eg doctor /path/to/my-repo

# Require git-history readability (for scan-history workflows)
eg doctor . --require-history

# Human-readable output
eg doctor . --format text

# Optional: check Hugging Face network reachability
eg doctor . --network
```

---

## Distinction from issue #71 (semantic index readiness)

`eg doctor` is a **setup preflight**: it runs _before_ any `eg scan` or `eg ingest`
command and diagnoses whether the local machine has the tools and permissions needed
to start the workflow. It never reads a graph store and never contacts the semantic
query engine.

The semantic index readiness report (issue #71) is a **post-ingest diagnostic**: it
runs after `eg ingest --embed` has populated a store and checks whether the
embedding coverage, dimensions, and model identity in that store match expectations.
Those two concerns are completely separate scopes and are intentionally kept in
separate commands.

---

## Flags

| Flag | Default | Effect |
|---|---|---|
| `PATH` | `.` | Repository path to inspect |
| `--out <path>` | `graph.jsonl` | Output JSONL path whose parent writability is probed |
| `--data-dir <path>` | `.egregore` | Embedded data directory whose writability is probed |
| `--require-history` | off | Promote git-history readability from optional to required |
| `--network` | off | Perform one optional Hugging Face TCP reachability check |
| `--format json\|text` | `json` | Output format (JSON is the stable machine-readable contract) |

---

## Check matrix

| Check ID | Gate | Requirement | Pass condition |
|---|---|---|---|
| `repository_path` | structural | required | PATH exists on disk |
| `git_available` | structural | required | `git --version` succeeds |
| `git_history_readable` | structural | optional → **required** with `--require-history` | `git -C <PATH> log -1` succeeds |
| `output_path_writable` | structural | required | parent of `--out` is writable |
| `data_dir_writable` | structural | required | `--data-dir` (or its ancestor) is writable |
| `hf_cache_location` | none (info) | optional | always Pass — reports resolved cache path |
| `embedding_model_identity` | none (info) | optional | always Pass — reports `sentence-transformers/all-MiniLM-L6-v2` |
| `embedding_model_dimension` | none (info) | optional | always Pass — reports 384 |
| `model_cache_present` | semantic | optional\* | model directory found under HF cache |
| `python_available` | semantic | optional\* | `python3` or `python` found on PATH |
| `python_priming_runnable` | semantic | optional\* | `import sentence_transformers` succeeds; **Skipped** when python absent |
| `hf_cache_readable` | semantic | optional (warn) | HF cache directory is readable |
| `windows_symlink_support` | semantic | optional (warn) | non-Windows → **Skipped**; Windows + Developer Mode → Pass; Windows + disabled → Warn |
| `hf_reachable` | semantic | optional | TCP connect to `huggingface.co:443`; **only emitted with `--network`** |

\* Optional in the sense that they do not affect the exit code. Their failure
  makes `semantic_ready: false` in the JSON output.

### Readiness gates

- **`structural_ready`**: all `required` structural checks pass. This is the
  minimum needed for `eg scan` and `eg ingest` to work.
- **`semantic_ready`**: `structural_ready` AND all semantic checks that are
  required-for-semantic (`model_cache_present`, `python_available`,
  `python_priming_runnable`) pass.
- **`overall_ready`**: equals `structural_ready`. Semantic is an enhancement,
  not required for overall readiness.

---

## Exit codes

| Code | Meaning |
|---|---|
| `0` | All required checks pass (`structural_ready: true`) |
| `1` | At least one required check failed (`structural_ready: false`) |

Warn, Skipped, and optional failures never affect the exit code.

---

## JSON output (stable contract)

```json
{
  "schema_version": 1,
  "repository_path": "/path/to/repo",
  "overall_ready": true,
  "structural_ready": true,
  "semantic_ready": false,
  "next_command": "eg scan /path/to/repo --out graph.jsonl",
  "checks": [
    {
      "id": "repository_path",
      "status": "pass",
      "requirement": "required",
      "gate": "structural",
      "summary": "repository path exists: /path/to/repo",
      "path": "/path/to/repo"
    },
    {
      "id": "git_available",
      "status": "pass",
      "requirement": "required",
      "gate": "structural",
      "summary": "git binary is available on PATH"
    },
    ...
    {
      "id": "model_cache_present",
      "status": "fail",
      "requirement": "optional",
      "gate": "semantic",
      "summary": "model cache not found under: /home/user/.cache/huggingface/hub",
      "remediation": "prime the model cache: pip install -U sentence-transformers && python -c \"from sentence_transformers import SentenceTransformer; SentenceTransformer('sentence-transformers/all-MiniLM-L6-v2')\"",
      "path": "/home/user/.cache/huggingface/hub"
    }
  ]
}
```

Stable fields: `schema_version`, `repository_path`, `overall_ready`,
`structural_ready`, `semantic_ready`, `next_command`, and in each check: `id`,
`status`, `requirement`, `gate`, `summary`. Optional fields (`remediation`, `path`)
are omitted when not applicable.

Check order is always sorted by the canonical `CheckId` discriminant order
(the order shown in the check matrix table above). The JSON schema version is
bumped if the field layout changes.

---

## Text output (human-readable, not a stable parsing contract)

```
eg doctor — setup preflight (schema v1)
repository: /path/to/repo

[ok]   repository path exists: /path/to/repo
[ok]   git binary is available on PATH
[ok]   git history is readable
[ok]   output path parent is writable: graph.jsonl
[ok]   data directory is writable: .egregore
[ok]   Hugging Face cache: /home/user/.cache/huggingface/hub
[ok]   expected model: sentence-transformers/all-MiniLM-L6-v2 (provider: aletheiadb_re_export)
[ok]   expected embedding dimension: 384 (all-MiniLM-L6-v2)
[fail] model cache not found under: /home/user/.cache/huggingface/hub
        → prime the model cache: pip install -U sentence-transformers && python -c "..."
[fail] python3 and python not found on PATH
        → install Python 3.8+ to enable model priming
[--]   skipped — python not available
[ok]   Hugging Face cache directory is readable
[--]   skipped — not running on Windows

overall: ready  structural: ready  semantic: not ready
next: eg scan /path/to/repo --out graph.jsonl
```

---

## Read-only guarantee

`eg doctor` never downloads models, creates graph records, mutates `.egregore`,
starts or stops the daemon, rewrites runtime metadata, or contacts hosted services
unless `--network` is explicitly passed.

The one pragmatic exception: writability probing creates a uniquely-named
temporary file (`.egregore-doctor-probe-<pid>`) in the target directory and
immediately removes it. This probe is self-cleaning and invisible to concurrent
processes.

---

## Secret safety

The preflight report never prints bearer tokens, Hugging Face tokens, environment
variable values that look secret-bearing, raw transcript text, command output
payloads, patch hunks, issue bodies, or graph record payloads.

Allowed in the report: safe filesystem paths, stable check IDs, boolean-derived
status strings, public compile-time embedding constants (model name, dimension,
provider), and static remediation templates.

This guarantee is structural: the `Observations` type that carries environment
probe results has no field capable of holding a raw env value — every
secret-adjacent input is reduced to a `bool` or a safe resolved `PathBuf`
before the report is built.

---

## Hugging Face cache resolution

The HF cache directory is resolved using the standard Hugging Face convention:

1. `HF_HUB_CACHE` if set and non-empty
2. `$HF_HOME/hub` if `HF_HOME` is set and non-empty
3. `<home>/.cache/huggingface/hub` (default)

The resolved path (never the env value) appears in the report as the
`hf_cache_location` check's `path` field.

When `HF_HUB_OFFLINE=1` or `TRANSFORMERS_OFFLINE=1` is set and the model cache
is absent, the remediation text for `model_cache_present` explains that offline
mode is enabled and instructs you to disable it before running the Python priming
command.
