#![no_std]
#![no_main]

#[macro_use]
extern crate alloc;

mod app;
mod backend;

use uefi::prelude::*;

use crate::app::App;

#[entry]
fn main() -> Status {
    uefi::helpers::init().unwrap();

    system::with_stdout(|output| {
        output.reset(false).unwrap();
        system::with_stdin(|input| {
            let mut app = App::new(input, output);
            app.run();
        });
    });

    Status::SUCCESS
}
