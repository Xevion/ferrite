//! Runs the lab binary: the Linux C runs on the shim's scheduler through the time and driver-core
//! bring-up, the page model check, every initcall, the firmware loader, an nl80211 scan, a WPA2-PSK
//! connection between the two simulated radios with DHCP, ICMP, UDP and TCP over it, and every exitcall,
//! and the lab exits 0. A run that stops in a stub or on
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
    check!(
        stderr.contains("beacons the WPA2-PSK network \"ferrite-lab\" on 2412 MHz; group key 1 installed"),
        "{stderr}"
    );
    check!(stderr.contains("nl80211: scan on wlan0 done after "), "{stderr}");
    check!(stderr.contains("BSS 02:00:00:00:01:00 2412 MHz -30.00 dBm RSN \"ferrite-lab\""), "{stderr}");
    check!(stderr.contains("the scan on wlan0 found wlan1's network \"ferrite-lab\" (RSN)"), "{stderr}");

    // wlan0 connects to wlan1's AP, the 4-way handshake runs in Rust on both sides, keys are installed
    // on both, the port opens, and the disconnect is announced.
    check!(stderr.contains("connect: associated after "), "{stderr}");
    check!(stderr.contains("(status 0)"), "{stderr}");
    check!(stderr.contains("keys: pairwise CCMP key installed for 02:00:00:00:01:00"), "{stderr}");
    check!(stderr.contains("keys: group CCMP key 1 installed"), "{stderr}");
    check!(stderr.contains("4-way: complete; the port to 02:00:00:00:01:00 is open"), "{stderr}");
    check!(stderr.contains("lab AP: pairwise and group keys installed and the station authorized"), "{stderr}");
    check!(!stderr.contains("4-way: dropped a frame"), "{stderr}");

    // Once the port is open, init hands both netdevs to the main loop's smoltcp through the frame seam.
    // The AP's DHCP server leases the station 10.42.0.2/24, and the station pings the AP, gets a UDP echo
    // back and fetches the AP's HTTP page over TCP, all over the CCMP-protected link.
    check!(stderr.contains("dhcp: lease 10.42.0.2 (prefix /24), router 10.42.0.1"), "{stderr}");
    for seq in 0..3 {
        check!(stderr.contains(&format!("ping 10.42.0.1: reply from 10.42.0.1 seq {seq} after ")), "{stderr}");
    }
    check!(stderr.contains("ip: ping 10.42.0.1: 3 of 3 replies"), "{stderr}");
    check!(stderr.contains("ip: udp echo through 10.42.0.1:7 came back: 20 bytes"), "{stderr}");
    check!(stderr.contains("ip: tcp: \"HTTP/1.1 200 \" from 10.42.0.1:80"), "{stderr}");
    check!(stderr.contains("ip: lab IP checks passed over the encrypted link"), "{stderr}");
    check!(stderr.contains("the main loop finished IP after "), "{stderr}");
    check!(stderr.contains(" ms: passed"), "{stderr}");
    check!(!stderr.contains("dropped 1") && !stderr.contains("errors 1"), "{stderr}");
    check!(stderr.contains("disconnected after holding "), "{stderr}");
    check!(stderr.contains("reason 3 (DeauthLeaving)"), "{stderr}");

    let ran =
        stderr.lines().filter(|line| line.starts_with("INFO  exitcall ") && !line.contains("exitcalls done")).count();
    check!(ran > 0);
    check!(stderr.contains(&format!("exitcalls done: {ran} ran")), "not every exitcall ran:\n{stderr}");
    check!(stderr.contains("exitcall rtw89_8852ce_driver_exit"), "{stderr}");
    check!(output.status.code() == Some(0), "the lab did not exit 0:\n{stderr}");
}
