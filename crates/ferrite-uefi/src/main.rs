#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

mod app;
mod backend;
mod diag;
mod mdns;
mod net;
mod sweep;

use uefi::prelude::*;

use crate::app::App;
use crate::diag::Diag;

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    // Open the on-USB log first: if console setup below never returns, the log
    // still records that we booted and where we got stuck.
    let mut diag = Diag::open();
    diag.event(0, "console: acquiring stdout/stdin");

    system::with_stdout(|output| {
        output.reset(false).unwrap();
        system::with_stdin(|input| {
            let mut app = App::new(input, output, &mut diag);
            app.run();
        });
    });

    Status::SUCCESS
}
