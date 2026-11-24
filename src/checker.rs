//! Decides whether a proposal changes only what a proposal may change.

use crate::{config::Experiment, source::Index};

/// Returns the reasons the proposal is rejected. Empty means accepted.
pub fn check(_experiment: &Experiment, _index: &Index, _proposal: &str) -> Vec<String> {
    todo!()
}
