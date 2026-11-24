use crate::dependencies::*;
use vstd::prelude::*;

verus! {

fn compute_triangle(n: u32) -> (sum: u32)
    requires
        triangle(n as nat) <= u32::MAX,
    ensures
        sum == triangle(n as nat),
{
    let mut i: u32 = 0;
    let mut sum: u32 = 0;
    while i < n
        invariant
            i <= n,
            sum == triangle(i as nat),
            triangle(n as nat) <= u32::MAX,
        decreases n - i,
    {
        i = i + 1;
        proof {
            lemma_triangle_monotonic(i as nat, n as nat);
        }
        sum = sum + i;
    }
    sum
}

} // verus!
