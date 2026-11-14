//! Runs `ferrite-lab station`: the station service joins the simulated access point and offers its
//! port, notices the access point deauthenticating it and withdraws the port, starts a new attempt after
//! its backoff, and stops on request.

use std::process::Command;

use assert2::check;

#[test]
fn the_station_connects_survives_a_deauthentication_and_stops() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).arg("station").output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(output.status.success(), "{stderr}");
    check!(stderr.contains("lab station: port up while connected to \"ferrite-lab\" (02:00:00:00:01:00)"), "{stderr}");
    check!(stderr.contains("lab AP: deauthenticated 02:00:00:00:00:00"), "{stderr}");
    check!(stderr.contains("station: disconnected during the hold: reason 2"), "{stderr}");
    check!(stderr.contains("lab station: port withdrawn while failed: the link was lost; retrying in 1 s"), "{stderr}");
    check!(stderr.contains("lab station: reconnecting: scanning"), "{stderr}");
    check!(stderr.contains("lab station: stopped; "), "{stderr}");
    check!(!stderr.contains("ERROR"), "{stderr}");
}
