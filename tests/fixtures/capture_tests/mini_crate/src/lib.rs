//! Tiny fixture crate whose public function names match the test names in the
//! sibling libtest JSON fixtures, so `eg capture-tests --graph` can resolve
//! each captured test to exactly one `Symbol`.

/// A function exercised by `mini_crate::test_alpha`.
pub fn test_alpha() -> u32 {
    1
}

/// A function exercised by `mini_crate::test_beta`.
pub fn test_beta() -> u32 {
    2
}

/// A function exercised by `mini_crate::test_gamma`.
pub fn test_gamma() -> u32 {
    3
}

/// A function exercised by `mini_crate::test_delta`.
pub fn test_delta() -> u32 {
    4
}
