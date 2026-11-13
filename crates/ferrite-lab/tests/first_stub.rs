//! Runs the lab binary: the Linux C runs on the shim's scheduler until it reaches something nothing
//! implements, and the lab names where. When that gets an implementation, the run moves on to the next
//! one and these expectations move with it.

use std::process::Command;

use assert2::check;

/// A fault outside the fenced data, which the lab reports with a backtrace like a stub hit.
const EXIT_SEGV: i32 = 139;

#[test]
fn kthreads_run_and_the_first_unimplemented_init_stops_with_a_backtrace() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(stderr.contains("kthread round trip through kthreadd: created, ran, stopped"), "{stderr}");
    check!(stderr.contains("initcall kthreads_init returned 0"));
    check!(stderr.contains("initcall spawn_ksoftirqd returned 0"));

    // The driver core is not set up yet (no devices_init), so the workqueue's sysfs directory has no parent.
    check!(output.status.code() == Some(EXIT_SEGV), "stopped elsewhere:\n{stderr}");
    check!(stderr.contains("initcall level 1 wq_sysfs_init"));
    check!(stderr.contains("segmentation fault at 0x"));
    let frames: Vec<&str> = stderr.lines().filter_map(|line| line.split_once(": ").map(|(_, name)| name)).collect();
    for frame in ["kobject_get", "virtual_device_parent", "subsys_virtual_register"] {
        check!(frames.contains(&frame), "backtrace has no {frame} frame:\n{stderr}");
    }
}
