//! Fixture crate for the issue #252 five-domain context join acceptance gate.
//!
//! The gate (`tests/integration/five_domain_context_join.rs`) drives the real
//! `eg` commands end to end against this crate:
//!
//! - `eg scan` records the `five_domain_probe` symbol and its file,
//! - `eg capture-tests --graph` anchors a `TestRun` to the probe via the
//!   test below (resolved by final `::` name segment),
//! - `eg write observation` / `eg write failure` / `eg write artifact` /
//!   `eg import-local-tasks` / `eg link-evidence` hang the agent-memory,
//!   project, artifact, and verification domains off the same symbol.
//!
//! Keep this crate dependency-free so the gate runs fully offline.

/// Probe symbol for the five-domain join gate.
///
/// Returns the sentinel the captured test asserts on, so the `TestRun` →
/// `Symbol` `MENTIONS_SYMBOL` edge minted by `eg capture-tests --graph` has a
/// real passing run behind it instead of a fabricated stream.
pub fn five_domain_probe() -> &'static str {
    "egregore-join-probe"
}

#[cfg(test)]
mod tests {
    /// Deliberately shares the probe's final name segment: `eg capture-tests
    /// --graph` resolves test names by final `::` segment, and the extractor
    /// qualifies this test as `tests::five_domain_probe` while the probe
    /// itself is `five_domain_probe`, so resolution stays unambiguous and
    /// mints exactly one `TestRun` → `Symbol` edge.
    #[test]
    fn five_domain_probe() {
        assert_eq!(super::five_domain_probe(), "egregore-join-probe");
    }
}
