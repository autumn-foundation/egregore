// Seeded Verus fixture: the stub verifier FAILS this file.
// VERUS_SHOULD_FAIL
use vstd::prelude::*;

verus! {

pub fn add_one(x: u64) -> (result: u64)
    requires
        x < 100,
    ensures
        result == x + 1,
{
    x + 2 // wrong on purpose
}

} // verus!
