//! Runs the lab binary: the Linux C runs on the shim's scheduler through the time and driver-core
//! bring-up, the page model check, every initcall, the firmware loader, an nl80211 scan between the two
//! simulated radios and every exitcall, and the lab exits 0. A run that stops in a stub or on
//! unimplemented data exits 86 or 87 instead.

use std::process::Command;

use assert2::check;

#[test]
fn every_initcall_returns_0_firmware_loads_and_every_exitcall_runs() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(stderr.contains("kthread round trip through kthreadd: created, ran, stopped"), "{stderr}");
    check!(stderr.contains("msleep(20) returned after"), "{stderr}");
    check!(stderr.contains("; a timer_list fired"), "{stderr}");
    check!(
        stderr.contains(
            "page model: two page frags shared one compound block through its head and it returned on drain; alloc_skb and kfree_skb balanced"
        ),
        "{stderr}"
    );

    let started = stderr.matches("initcall level ").count();
    let returned_0 = stderr.lines().filter(|line| line.contains("initcall ") && line.ends_with(" returned 0")).count();
    check!(started > 0);
    check!(returned_0 == started, "not every initcall returned 0:\n{stderr}");
    for call in ["clocksource_done_booting", "cfg80211_init", "rtw89_8852ce_driver_init", "regulatory_init_db"] {
        check!(stderr.contains(&format!("initcall {call} returned 0")), "{call} did not return 0:\n{stderr}");
    }

    // regulatory.db is the size of the file on disk, a missing name is -ENOENT, and cfg80211 loaded it.
    let size = std::fs::metadata("/lib/firmware/regulatory.db").expect("regulatory.db in /lib/firmware").len();
    check!(
        stderr.contains(&format!(
            "request_firmware(regulatory.db) returned {size} bytes; a missing file returned -ENOENT"
        )),
        "{stderr}"
    );
    check!(stderr.contains("regulatory.db loaded and applied by cfg80211 after "), "{stderr}");

    // The scan goes through nl80211 over netlink sockets, ends on the scan group and finds the AP.
    check!(stderr.contains("initcall init_mac80211_hwsim returned 0"), "{stderr}");
    check!(stderr.contains("beacons the open network \"ferrite-lab\" on 2412 MHz"), "{stderr}");
    check!(stderr.contains("nl80211: scan on wlan0 done after "), "{stderr}");
    check!(stderr.contains("BSS 02:00:00:00:01:00 2412 MHz"), "{stderr}");
    check!(stderr.contains("the scan on wlan0 found wlan1's network \"ferrite-lab\""), "{stderr}");

    let ran =
        stderr.lines().filter(|line| line.starts_with("INFO  exitcall ") && !line.contains("exitcalls done")).count();
    check!(ran > 0);
    check!(stderr.contains(&format!("exitcalls done: {ran} ran")), "not every exitcall ran:\n{stderr}");
    check!(stderr.contains("exitcall rtw89_8852ce_driver_exit"), "{stderr}");
    check!(output.status.code() == Some(0), "the lab did not exit 0:\n{stderr}");
}
