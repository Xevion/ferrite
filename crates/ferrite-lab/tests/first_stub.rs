//! Runs the lab binary: the Linux C reaches a kernel symbol nothing implements and stops in the halt
//! hook, naming it and the C frames that led there. When the symbol gets a real implementation, the run
//! moves on to the next one and these expectations move with it.

use std::process::Command;

use assert2::check;

const EXIT_HALTED: i32 = 86;

#[test]
fn the_first_initcall_stops_in_a_named_stub_with_a_backtrace_through_the_c() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(output.status.code() == Some(EXIT_HALTED), "stopped outside the halt hook:\n{stderr}");
    check!(stderr.contains("initcall level 4 ieee80211_init"));
    check!(stderr.contains("halted in stub get_random_bytes"));

    let frames: Vec<&str> = stderr.lines().filter_map(|line| line.split_once(": ").map(|(_, name)| name)).collect();
    let position = |name: &str| frames.iter().position(|frame| *frame == name);
    let (stub, sample, minstrel, init) = (
        position("get_random_bytes"),
        position("init_sample_table"),
        position("rc80211_minstrel_init"),
        position("ieee80211_init"),
    );
    check!(stub < sample && sample < minstrel && minstrel < init, "backtrace frames out of order:\n{stderr}");
    check!(stub.is_some(), "backtrace has no C frames:\n{stderr}");
}
