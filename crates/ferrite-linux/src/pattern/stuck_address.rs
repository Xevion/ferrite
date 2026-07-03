use crate::ops;
use crate::{Failure, FailureBudget};

pub(super) fn run(
    buf: &mut [u64],
    parallel: bool,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let failures = ops::fill_verify_indexed(buf, parallel, budget, on_activity);
    on_subpass();
    failures
}
