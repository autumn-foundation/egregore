//! Seeded fixture for `eg capture-coverage` integration tests (issue #230):
//! a partially-covered module.
//!
//! COVERAGE_SECRET_MARKER: this comment text must never appear inline in
//! capture output — the normalized summary carries figures and identifiers
//! only, never source text.

/// Fully covered by tests.
#[must_use]
pub fn covered_add(a: u64, b: u64) -> u64 {
    a.wrapping_add(b)
}

/// Never called by any test: 0% line coverage.
#[must_use]
pub fn never_called() -> u64 {
    42
}

/// Partially covered: only the `true` arm is exercised.
#[must_use]
pub fn maybe_double(flag: bool, x: u64) -> u64 {
    if flag {
        x * 2
    } else {
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_works() {
        assert_eq!(covered_add(2, 3), 5);
    }

    #[test]
    fn double_true_arm() {
        assert_eq!(maybe_double(true, 21), 42);
    }
}
