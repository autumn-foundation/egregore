//! Fixture crate for issue #162: per-symbol structural complexity.
//!
//! The expected scores follow the counting rules in
//! `docs/schema/complexity.md`: base 1 plus one per decision point
//! (`if`/`else if`, `for`, `while`, `loop`, each `match` arm, each `?`,
//! each `&&` / `||`). Nested `fn` bodies are NOT counted toward the
//! enclosing function (they get their own symbol); closure bodies ARE.

/// Straight-line function: the documented minimum, complexity 1.
pub fn trivial_add(a: i32, b: i32) -> i32 {
    a + b
}

/// Engineered high-complexity function.
///
/// Decision points: `for` (1), `if` (1), `else if` (1), `while` (1),
/// `match` arms (3), `if` (1), `&&` (1), `||` (1) = 10, plus the base 1:
/// expected complexity 11.
pub fn gnarly(n: i32) -> i32 {
    let mut total = 0;
    for i in 0..n {
        if i % 2 == 0 {
            total += i;
        } else if i % 3 == 0 {
            total -= 1;
        }
        while total > 100 {
            total -= 10;
        }
    }
    let v = match total {
        0 => 1,
        1..=10 => 2,
        _ => 3,
    };
    if v > 0 && n < 100 || n == 0 {
        v + 1
    } else {
        v - 1
    }
}

/// Monotonic sanity chain: each function adds exactly one decision point
/// over the previous, so complexities must read 1, 2, 3, 4, 5, 6, 7.
pub fn chain_0() -> i32 {
    42
}

pub fn chain_1(x: bool) -> i32 {
    if x { 1 } else { 0 }
}

pub fn chain_2(x: bool) -> i32 {
    if x && x { 1 } else { 0 }
}

pub fn chain_3(x: bool) -> i32 {
    if x && x || x { 1 } else { 0 }
}

pub fn chain_4(x: bool) -> i32 {
    if x && x || x {
        if x { 1 } else { 0 }
    } else {
        0
    }
}

pub fn chain_5(x: bool) -> i32 {
    while x {
        break;
    }
    if x && x || x {
        if x { 1 } else { 0 }
    } else {
        0
    }
}

pub fn chain_6(x: bool) -> i32 {
    for _ in 0..1 {}
    while x {
        break;
    }
    if x && x || x {
        if x { 1 } else { 0 }
    } else {
        0
    }
}

/// `?` is a decision point: expected complexity 3.
pub fn chain_try() -> Option<i32> {
    let x = Some(1)?;
    if x > 0 { Some(x) } else { None }
}

/// `loop` is a decision point: expected complexity 2.
pub fn chain_loop() -> i32 {
    loop {
        break 7;
    }
}

/// Closure bodies count toward the enclosing function: expected 2.
pub fn with_closure() -> i32 {
    let negate = |x: i32| if x > 0 { -x } else { x };
    negate(3)
}

/// A nested `fn` keeps its own symbol and is NOT counted toward the
/// enclosing body: outer expected 2 (one `if`), inner expected 2.
pub fn outer_with_nested() -> i32 {
    fn inner(x: bool) -> i32 {
        if x { 1 } else { 0 }
    }
    if true { inner(true) } else { 0 }
}

pub trait Greet {
    /// Body-less trait method declaration: complexity 1.
    fn hello(&self) -> String;
    /// Default-bodied trait method: complexity 2.
    fn loud_hello(&self, shout: bool) -> String {
        if shout {
            "HELLO".to_owned()
        } else {
            "hello".to_owned()
        }
    }
}

pub struct Counter {
    pub value: i32,
}

impl Counter {
    /// Impl method: complexity 2.
    pub fn bump(&mut self, by: i32) {
        if by > 0 {
            self.value += by;
        }
    }
}

#[test]
fn complexity_smoke() {
    assert_eq!(trivial_add(1, 2), 3);
}
