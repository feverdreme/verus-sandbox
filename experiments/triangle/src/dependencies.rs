use vstd::prelude::*;

verus! {

/// 0 + 1 + ... + n
pub open spec fn triangle(n: nat) -> nat
    decreases n,
{
    if n == 0 {
        0
    } else {
        n + triangle((n - 1) as nat)
    }
}

pub proof fn lemma_triangle_monotonic(a: nat, b: nat)
    requires
        a <= b,
    ensures
        triangle(a) <= triangle(b),
    decreases b,
{
    if a < b {
        lemma_triangle_monotonic(a, (b - 1) as nat);
        lemma_triangle_step(b);
    }
}

proof fn lemma_triangle_step(n: nat)
    requires
        n > 0,
    ensures
        triangle(n) == n + triangle((n - 1) as nat),
{
}

} // verus!
