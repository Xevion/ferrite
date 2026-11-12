//! Runs the lab binary: the Linux C reaches a kernel symbol nothing implements and stops in the halt
//! hook, naming it and the C frames that led there. When the symbol gets a real implementation, the run
//! moves on to the next one and these expectations move with it.

use std::process::Command;

use assert2::check;

const EXIT_HALTED: i32 = 86;

#[test]
fn the_first_initcall_stops_in_a_named_stub_with_a_backtrace() {
    let output = Command::new(env!("CARGO_BIN_EXE_ferrite-lab")).output().expect("run the lab binary");
    let stderr = String::from_utf8_lossy(&output.stderr);

    check!(output.status.code() == Some(EXIT_HALTED), "stopped outside the halt hook:\n{stderr}");
    check!(stderr.contains("initcall level early kthreads_init"));
    check!(stderr.contains("halted in stub __cpuhp_setup_state"));

    // kthreads_init tail-calls the stub, so no C frame of its own is left to name.
    let mut frames = stderr.lines().filter_map(|line| line.split_once(": ").map(|(_, name)| name));
    check!(frames.any(|name| name == "__cpuhp_setup_state"), "backtrace has no stub frame:\n{stderr}");
}
