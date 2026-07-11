use crate::shutdown;
use crate::{Failure, FailureBudget};

use super::fill_and_verify;

pub(super) fn run(
    buf: &mut [u64],
    parallel: bool,
    budget: &FailureBudget,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(fill_and_verify(
        buf,
        0xAAAA_AAAA_AAAA_AAAA,
        parallel,
        budget,
        on_subpass,
        on_activity,
    ));
    if budget.is_exhausted() || shutdown::quit_requested() {
        return failures;
    }
    failures.extend(fill_and_verify(
        buf,
        0x5555_5555_5555_5555,
        parallel,
        budget,
        on_subpass,
        on_activity,
    ));
    failures
}
