use crate::dependencies::*;
use vstd::prelude::*;

verus! {

proof fn consecutive_product_is_even(n: int)
    ensures
        is_even(n * (n + 1)),
{
    if n % 2 == 0 {
        assert(n == 2 * (n / 2));
        lemma_even_times(n, n + 1);
    } else {
        assert(n + 1 == 2 * ((n + 1) / 2));
        lemma_even_times(n + 1, n);
        assert(n * (n + 1) == (n + 1) * n) by (nonlinear_arith);
    }
}

} // verus!
