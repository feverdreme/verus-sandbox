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
    total_induction(costs);
}

proof fn total_induction(costs: Seq<int>)
    requires
        forall|i: int| 0 <= i < costs.len() ==> affordable(#[trigger] costs[i]),
    ensures
        affordable(total(costs)),
    decreases costs.len(),
{
    if costs.len() == 0 {
        lemma_zero_affordable();
    } else {
        let rest = costs.drop_last();
        assert forall|i: int| 0 <= i < rest.len() implies affordable(#[trigger] rest[i]) by {
            assert(rest[i] == costs[i]);
        }
        total_induction(rest);
        assert(affordable(costs[costs.len() - 1]));
        lemma_affordable_add(total(rest), costs.last());
    }
}

} // verus!
