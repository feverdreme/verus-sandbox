use vstd::prelude::*;

verus! {

pub open spec fn is_even(x: int) -> bool {
    exists|k: int| x == #[trigger] (2 * k)
}

/// An even number times any number is even.
pub proof fn lemma_even_times(x: int, y: int)
    requires
        is_even(x),
    ensures
        is_even(x * y),
{
    let k = choose|k: int| x == #[trigger] (2 * k);
    assert(x * y == 2 * (k * y)) by (nonlinear_arith)
        requires
            x == 2 * k,
    ;
}

} // verus!
