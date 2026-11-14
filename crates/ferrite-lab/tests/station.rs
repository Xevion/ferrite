//! Runs `ferrite-lab station`: the station service joins the simulated access point and offers its
//! port, which carries IP; the access point deauthenticates it, the service withdraws the port, backs
//! off, reconnects with a new port that leases an address again, and after a connection that stayed up
//! backs off from the start again; then it stops on request.

use std::process::Command;

use assert2::check;

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
