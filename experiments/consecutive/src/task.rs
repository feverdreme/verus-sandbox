use crate::dependencies::*;
use vstd::prelude::*;

verus! {

proof fn consecutive_product_is_even(n: int)
    ensures
        is_even(n * (n + 1)),
{
    todo!("prove that n * (n + 1) is even")
}

} // verus!
