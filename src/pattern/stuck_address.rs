use crate::Failure;
use crate::ops;

pub(super) fn run(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let failures = ops::fill_verify_indexed(buf, parallel, on_activity);
    on_subpass();
    failures
}
