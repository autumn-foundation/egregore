# Task: Add protected raw artifact capture

## Problem
Evidence handles rot when they only point at yesterday's local path.
Agents end up citing a hash that no one can inspect without finding the old
transcript by candlelight.

## Acceptance Criteria
- [ ] Operator workflow captures protected raw artifacts only when explicitly enabled.
- [ ] Disabled mode emits only provenance handles and content hashes.
- [ ] After original source files are moved, 100% of captured payloads retrievable.
- [ ] Hash mismatch and other error conditions fail with stable machine-readable diagnostics.
