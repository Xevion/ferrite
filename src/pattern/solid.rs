use crate::Failure;

use super::fill_and_verify;

pub(super) fn run(
    buf: &mut [u64],
    parallel: bool,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let mut failures = Vec::new();
    failures.extend(fill_and_verify(
        buf,
        0x0000_0000_0000_0000,
        parallel,
        on_subpass,
        on_activity,
    ));
    failures.extend(fill_and_verify(
        buf,
        0xFFFF_FFFF_FFFF_FFFF,
        parallel,
        on_subpass,
        on_activity,
    ));
    failures
}
