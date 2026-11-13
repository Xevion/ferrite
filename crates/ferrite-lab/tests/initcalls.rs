//! Runs the lab binary: the Linux C runs on the shim's scheduler through the driver-core bring-up and
//! the initcalls until it reaches something nothing implements, and the lab names where. When that gets
//! an implementation, the run moves on to the next one and these expectations move with it.

use std::process::Command;

use assert2::check;

/// The lab's status when init never finished because every task is blocked.
const EXIT_FAILURE: i32 = 1;

#[test]
fn the_initcalls_run_until_the_timekeeping_the_clocksource_needs_is_missing() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(stderr.contains("kthread round trip through kthreadd: created, ran, stopped"), "{stderr}");
    for call in [
        "kthreads_init",
        "spawn_ksoftirqd",
        "wq_sysfs_init",
        "irq_sysfs_init",
        "pci_driver_init",
        "net_dev_init",
        "rfkill_init",
        "ieee80211_init",
        "iomem_init_inode",
    ] {
        check!(stderr.contains(&format!("initcall {call} returned 0")), "{call} did not return 0:\n{stderr}");
    }

    // timekeeping_init never ran, so clocksource_done_booting takes clocksource_mutex twice and init sleeps
    // for good.
    check!(output.status.code() == Some(EXIT_FAILURE), "stopped elsewhere:\n{stderr}");
    check!(stderr.contains("initcall level 5 clocksource_done_booting"), "{stderr}");
    check!(!stderr.contains("initcall clocksource_done_booting returned"), "{stderr}");
    check!(stderr.contains("every task is blocked"), "{stderr}");
}
