use crate::Failure;

use super::fill_verify_indexed;

pub(super) fn run(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let failures = fill_verify_indexed(buf, parallel, on_activity);
    on_subpass();
    failures
}
