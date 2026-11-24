use vstd::prelude::*;

verus! {

spec const UNIT_PRICE: int = 7;

/// Whether a cost can be paid for exactly. How is not part of the interface.
pub closed spec fn affordable(cost: int) -> bool {
    cost % UNIT_PRICE == 0
}

pub proof fn lemma_zero_affordable()
    ensures
        affordable(0),
{
}

pub proof fn lemma_affordable_add(a: int, b: int)
    requires
        affordable(a),
        affordable(b),
    ensures
        affordable(a + b),
{
}

} // verus!
