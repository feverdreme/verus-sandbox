use crate::dependencies::*;
use vstd::prelude::*;

verus! {

spec fn total(costs: Seq<int>) -> int
    decreases costs.len(),
{
    if costs.len() == 0 {
        0
    } else {
        total(costs.drop_last()) + costs.last()
    }
}

proof fn total_is_affordable(costs: Seq<int>)
    requires
        forall|i: int| 0 <= i < costs.len() ==> affordable(#[trigger] costs[i]),
    ensures
        affordable(total(costs)),
{
    todo!("prove that the total is affordable; add a helper lemma if you need induction")
}

} // verus!
