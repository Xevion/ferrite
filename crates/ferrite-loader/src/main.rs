//! `ferrite-loader`: stays resident, keeps the DbC link to the desktop up and runs the `.efi`
//! images the desktop pushes over it.
//!
//! The loader owns the DbC and offers it to the apps it starts through the link protocol (see
//! `ferrite_efi::link`). Before each run it arms the firmware watchdog, so an app that hangs
//! resets the machine into the default OS, and while idle it keeps a shorter one armed, so a hang
//! in the loader does too. A pushed image of kind loader replaces this loader: the DbC is
//! released, the new image started, and the link taken up again if it returns.
//!
//! Its log is `\EFI\ferrite\ferrite-loader.txt`.

#![no_std]
#![no_main]

extern crate alloc;

mod image;
mod serve;

use alloc::format;
use core::time::Duration;

use ferrite_efi::bootlog::{BootLog, Echo};
use ferrite_efi::clock::Clock;
use ferrite_efi::link::provider::{Provider, Retained};
use ferrite_efi::xhci::OpenDbc;
use ferrite_efi::{entropy, link, logger, session, watchdog};
use ferrite_hal::{Clock as _, Instant};
use ferrite_recon::bringup::{self, Link};
use ferrite_recon::display::Chain;
use ferrite_wire::link::{Identity, Message, ShortStr};
use ferrite_xhci::Config;
use log::LevelFilter;
use uefi::prelude::*;

const BUILD: &str = concat!("ferrite-loader ", env!("CARGO_PKG_VERSION"));

/// How long to keep the result on screen before giving up when there is no DbC.
const EXIT_AFTER: Duration = Duration::from_secs(10);
/// How long to wait for the desktop to configure the DbC before leaving.
const CONFIGURE_TIMEOUT: Duration = Duration::from_secs(120);

#[entry]
fn main() -> Status {
    let helpers = uefi::helpers::init();
    logger::init(BootLog::open("ferrite-loader", Echo::Console), LevelFilter::Debug);
    if let Err(e) = helpers {
        log::warn!("uefi helpers init failed: {e}");
    }
    let clock = logger::with(|log| log.clock()).unwrap_or_else(Clock::start);
    let start = clock.now();
    log::info!("{BUILD}");

    if link::is_present() {
        log::error!("a loader already owns the link; push a replacement with kind loader instead");
        return Status::ALREADY_STARTED;
    }
    watchdog::disarm();

    let cfg = Config { timeout: CONFIGURE_TIMEOUT, ..Config::default() };
    let Some(Link { dbc, .. }) = bringup::bring_up(clock, start, &cfg) else {
        return give_up("no DbC was configured");
    };
    run(dbc, entropy::random_u64(), clock, start, &cfg)
}

/// Serve, hand over to a replacement loader when one is pushed, and take the link up again if it
/// returns. `instance` identifies this start in every `Ready`; the boot count grows each time the
/// link is served again.
fn run(mut dbc: OpenDbc, instance: u64, clock: Clock, start: Instant, cfg: &Config) -> Status {
    let mut boot = 0u32;
    loop {
        boot = boot.saturating_add(1);
        let identity = Identity { boot, instance };
        let mut provider = match Provider::install(dbc, ShortStr::new(BUILD).unwrap_or_default(), identity) {
            Ok(provider) => provider,
            Err(e) => return give_up(&format!("the link did not install: {}", Chain(&e))),
        };
        logger::attach_link();

        let (released, chain) = loop {
            let chain = serve::serve(&provider, clock);
            logger::detach_link();
            match provider.close() {
                Ok(released) => break (released, chain),
                Err(Retained { provider: kept, source }) => {
                    log::error!("the loader cannot hand over: uninstalling the link failed: {source}");
                    image::unload(chain.child);
                    provider = kept;
                    logger::attach_link();
                    let reason = format!("{} not started: the link would not uninstall", chain.name);
                    if let Err(e) = provider.send(&Message::PushRejected { reason }) {
                        log::warn!("rejection not sent: {e}");
                    }
                },
            }
        };

        // Dropping the DbC clears DCE, so the replacement finds the controller idle.
        drop(released);
        // The idle watchdog is no longer re-armed, so it must not be left to expire if arming for
        // the replacement fails below.
        watchdog::disarm();
        if let Err(e) = watchdog::arm(chain.watchdog) {
            log::warn!("watchdog not armed for the replacement: {e}");
        }
        log::info!("starting replacement loader {} (watchdog {} s)", chain.name, chain.watchdog.as_secs());
        let result = boot::start_image(chain.child);
        watchdog::disarm();
        log::warn!(
            "replacement loader returned {}; taking the link up again",
            result.err().map_or(Status::SUCCESS, |e| e.status())
        );

        dbc = match bringup::bring_up(clock, start, cfg) {
            Some(Link { dbc, .. }) => dbc,
            None => return give_up("the DbC did not come back after the replacement returned"),
        };
    }
}

/// Log why, save the log, and leave; an unattended run resets into the default OS.
fn give_up(why: &str) -> Status {
    log::error!("{why}");
    logger::with(|log| session::finish(log, EXIT_AFTER));
    Status::NOT_FOUND
}
