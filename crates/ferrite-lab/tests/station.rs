//! Runs `ferrite-lab station`: the station service joins the simulated access point and offers its
//! port, which carries IP; the access point deauthenticates it, the service withdraws the port, backs
//! off, reconnects with a new port that leases an address again, and after a connection that stayed up
//! backs off from the start again; then it stops on request.

use std::process::Command;

use assert2::check;
use rstest::rstest;

#[test]
fn the_station_reconnects_over_a_new_port_after_each_deauthentication_and_stops() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).arg("station").output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(output.status.success(), "{stderr}");
    check!(stderr.contains("lab station: port up while connected to \"ferrite-lab\" (02:00:00:00:01:00)"), "{stderr}");
    check!(stderr.matches("lab AP: deauthenticated 02:00:00:00:00:00").count() == 2, "{stderr}");
    check!(
        stderr.matches("lab AP: station 02:00:00:00:00:00 removed").count() == 2,
        "the AP drops its entry\n{stderr}"
    );
    check!(stderr.matches("station: disconnected during the hold: reason 2").count() == 2, "{stderr}");
    check!(
        stderr.matches("lab station: port withdrawn while failed: the link was lost; retrying in 1 s").count() == 2,
        "{stderr}"
    );
    check!(stderr.contains("lab station: reconnecting: scanning"), "{stderr}");
    check!(stderr.matches("ip: station leased 10.42.0.2/24").count() == 2, "one lease per port\n{stderr}");
    check!(stderr.contains("lab station: reconnected with a new port, which leased an address"), "{stderr}");
    check!(stderr.contains("lab station: backoff reset to 1s after the stable connection"), "{stderr}");
    check!(stderr.contains("lab station: stopped; "), "{stderr}");
    check!(!stderr.contains("ERROR"), "{stderr}");
}

/// Run `ferrite-lab station <scenario>` and return its log, which must not hold an error.
fn scenario(name: &str) -> String {
    let output =
        Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).args(["station", name]).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    check!(output.status.success(), "{stderr}");
    check!(!stderr.contains("ERROR"), "{stderr}");
    stderr
}

/// The lines every scenario that joins prints: the station is connected, leases an address and the IP checks
/// pass over the link, and the service stops.
fn joined(name: &str, stderr: &str) {
    check!(stderr.contains(&format!("lab station: {name}: connected to ")), "{stderr}");
    check!(stderr.contains("ip: station leased 10.42.0.2/24"), "{stderr}");
    check!(stderr.contains("ip: ping 10.42.0.1: 3 of 3 replies"), "{stderr}");
    check!(stderr.contains("ip: lab IP checks passed over the encrypted link"), "{stderr}");
    check!(stderr.contains(&format!("lab station: {name}: stopped")), "{stderr}");
}

#[test]
fn an_open_network_joins_with_no_handshake_and_no_keys() {
    let stderr = scenario("open");
    joined("open", &stderr);
    check!(stderr.contains("beacons the open network \"ferrite-open\""), "{stderr}");
    check!(stderr.contains("station: connecting to 02:00:00:00:01:00 on 2412 MHz at -30.00 dBm (Open)"), "{stderr}");
    check!(stderr.contains("on an open network; station added and authorized"), "{stderr}");
    check!(!stderr.contains("4-way") && !stderr.contains("keys:") && !stderr.contains("sae:"), "{stderr}");
}

#[test]
fn a_hidden_network_is_found_by_a_probe_that_names_it_and_not_by_the_usual_scan() {
    let stderr = scenario("hidden");
    joined("hidden", &stderr);
    check!(stderr.contains("(hidden)"), "{stderr}");
    check!(stderr.contains("lab AP: probe response with the hidden name sent"), "{stderr}");
    check!(stderr.contains("4-way: complete"), "{stderr}");

    let stderr = scenario("hidden-unmarked");
    check!(stderr.contains("none that a configured network can join"), "{stderr}");
    check!(stderr.contains("the station did not join, as expected: failed: the network was not found"), "{stderr}");
    check!(!stderr.contains("probe response with the hidden name"), "{stderr}");
}

#[rstest]
#[case::hash_to_element("sae", "HashToElement")]
#[case::hunting_and_pecking("sae-hunt", "HuntAndPeck")]
#[case::transition_prefers_sae("transition", "HashToElement")]
fn wpa3_joins_through_sae_with_protected_management_frames(#[case] name: &str, #[case] method: &str) {
    let stderr = scenario(name);
    joined(name, &stderr);
    check!(stderr.contains(&format!("(Sae({method}))")), "{stderr}");
    check!(stderr.contains(&format!("sae: commit sent to 02:00:00:00:01:00 ({method})")), "{stderr}");
    check!(stderr.contains("sae: confirm of 02:00:00:00:01:00 verified; the PMK is derived"), "{stderr}");
    check!(stderr.contains("lab AP: SAE with 02:00:00:00:00:00 complete; its confirm verified"), "{stderr}");
    check!(stderr.contains("keys: pairwise CCMP key installed for 02:00:00:00:01:00"), "{stderr}");
    check!(stderr.contains("keys: integrity group key 4 installed"), "{stderr}");
    check!(stderr.contains("4-way: complete; the port to 02:00:00:00:01:00 is open"), "{stderr}");
    check!(!stderr.contains("4-way: dropped a frame"), "{stderr}");
}

#[test]
fn a_transition_network_still_takes_a_station_that_is_limited_to_wpa2() {
    let stderr = scenario("transition-psk");
    joined("transition-psk", &stderr);
    check!(stderr.contains("(Psk)"), "{stderr}");
    check!(!stderr.contains("sae:") && !stderr.contains("integrity group key"), "{stderr}");
}

#[test]
fn a_wrong_sae_password_fails_the_confirm_on_both_sides_and_the_station_backs_off() {
    let stderr = scenario("sae-wrong-password");
    check!(stderr.contains("SAE: the confirm message does not verify"), "{stderr}");
    check!(stderr.contains("lab AP: SAE confirm of 02:00:00:00:00:00 refused"), "{stderr}");
    check!(
        stderr.contains("the station did not join, as expected: failed: the connection failed; retrying in 1 s"),
        "{stderr}"
    );
    check!(!stderr.contains("4-way"), "{stderr}");
}

#[test]
fn a_failing_network_is_followed_by_the_next_before_any_backoff() {
    let stderr = scenario("multi");
    joined("multi", &stderr);
    check!(stderr.contains("no 4-way handshake within 3000 ms; trying the next candidate"), "{stderr}");
    check!(stderr.matches("station: connecting to").count() == 2, "{stderr}");
    check!(!stderr.contains("retrying in"), "no backoff before the list was exhausted\n{stderr}");
    check!(stderr.contains("4-way: complete"), "{stderr}");
}
