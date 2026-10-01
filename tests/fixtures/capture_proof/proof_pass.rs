// Seeded Verus fixture: the stub verifier "proves" this file.
// (Fixtures are data, never compiled; the verus syntax is illustrative.)
use vstd::prelude::*;

verus! {

pub fn add_one(x: u64) -> (result: u64)
    requires
        x < 100,
    ensures
        result == x + 1,
{
    x + 1
}

} // verus!
