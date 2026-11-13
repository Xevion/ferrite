//! Runs the lab binary: the Linux C runs on the shim's scheduler through the time and driver-core
//! bring-up, the initcalls and the exitcalls until it reaches something nothing implements, and the lab
//! names where. When that gets an implementation, the run moves on to the next one and these
//! expectations move with it.

use std::process::Command;

use assert2::check;

/// The lab's status when it stops in a stub.
const EXIT_HALTED: i32 = 86;

#[test]
fn every_initcall_returns_0_and_the_exitcalls_run_until_a_kernel_parameter_lookup() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(stderr.contains("kthread round trip through kthreadd: created, ran, stopped"), "{stderr}");
    check!(stderr.contains("msleep(20) returned after"), "{stderr}");
    check!(stderr.contains("; a timer_list fired"), "{stderr}");

    let started = stderr.matches("initcall level ").count();
    let returned_0 = stderr.lines().filter(|line| line.contains("initcall ") && line.ends_with(" returned 0")).count();
    check!(started > 0);
    check!(returned_0 == started, "not every initcall returned 0:\n{stderr}");
    for call in ["clocksource_done_booting", "cfg80211_init", "rtw89_8852ce_driver_init", "regulatory_init_db"] {
        check!(stderr.contains(&format!("initcall {call} returned 0")), "{call} did not return 0:\n{stderr}");
    }

    // Unregistering the PCI driver asks the kernel command line about async probing.
    check!(output.status.code() == Some(EXIT_HALTED), "stopped elsewhere:\n{stderr}");
    check!(stderr.contains("exitcall rtw89_8852ce_driver_exit"), "{stderr}");
    check!(stderr.contains("halted in stub parse_option_str"), "{stderr}");
}
