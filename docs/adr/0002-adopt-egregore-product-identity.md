# ADR 0002: Adopt Egregore Product Identity

## Status

Accepted

## Context

The repository began as Aletheia Codegraph, focused on extracting code facts and Git history into AletheiaDB. The product scope has expanded: the same graph should also hold agent memory, project and task state, artifacts, and verification evidence so agents can connect what the code is, what happened, what was decided, and what was proven.

AletheiaDB already has its own crate and product identity as the database substrate. This repo needs a separate identity for the agentic SWE knowledge layer that runs on that substrate.

## Decision

Rename the product to Egregore.

Use `aletheia-egregore` as the Rust package/repository identity because the bare `egregore` crate name is already taken. Use `egregore` as the primary CLI binary and user-facing product name. Also build `eg` as a short CLI alias.

Keep `codegraph` as a domain namespace inside Egregore for deterministic source-derived code facts.

## Consequences

Positive:

- The product name now fits the broader shared-memory graph vision.
- AletheiaDB remains the common database substrate instead of being overloaded with agent workflow responsibilities.
- Code graph extraction can evolve as one domain alongside agent memory, project/task, artifact, and verification domains.
- The primary CLI is legible, and the `eg` alias is short enough for repeated interactive use.

Negative:

- Existing docs, tests, package metadata, and future release automation must track the rename.
- Some internal schema names such as `codegraph_id` remain intentionally domain-specific and should not be mechanically renamed until the broader schema is designed.
- Any future crates.io or GitHub release needs to use the `aletheia-egregore` package/repo name unless the bare name becomes available.
