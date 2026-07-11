use crate::shutdown;
use crate::{Failure, FailureBudget};

use super::fill_and_verify;

pub(super) fn run_ones(
    buf: &mut [u64],
    parallel: bool,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    for bit in 0..64 {
        if budget.is_exhausted() || shutdown::quit_requested() {
            break;
        }
        let pattern = 1u64 << bit;
        failures.extend(fill_and_verify(
            buf,
            pattern,
            parallel,
            budget,
            on_subpass,
            on_activity,
        ));
    }
    failures
}

pub(super) fn run_zeros(
    buf: &mut [u64],
    parallel: bool,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    for bit in 0..64 {
        if budget.is_exhausted() || shutdown::quit_requested() {
            break;
        }
        let pattern = !(1u64 << bit);
        failures.extend(fill_and_verify(
            buf,
            pattern,
            parallel,
            budget,
            on_subpass,
            on_activity,
        ));
    }
    failures
}
