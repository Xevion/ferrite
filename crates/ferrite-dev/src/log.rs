//! `log`: timestamped capture of everything the target prints over the DbC link.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use snafu::{ResultExt, Snafu};

use crate::config::Config;
use crate::connect::{ConnectError, Connector, Waited};
use crate::emit::{Emitter, LineBuffer};
use crate::tty::is_idle;

pub const PROBE_DELAY: Duration = Duration::from_secs(4);

#[derive(Debug, Snafu)]
pub enum LogError {
    #[snafu(display("cannot connect to the DbC link"))]
    Connect { source: ConnectError },
}

#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Stop after this long; `None` captures until stopped.
    pub duration: Option<Duration>,
    /// One line sent to the target a while after each connect.
    pub probe: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Pumped {
    LinkLost,
    Finished,
}

/// Capture until the duration passes or `stop` is set, reconnecting whenever the link drops.
pub fn run(config: &Config, options: &LogOptions, emitter: &mut Emitter, stop: &AtomicBool) -> Result<(), LogError> {
    let deadline = options.duration.map(|d| Instant::now() + d);
    emitter.line(
        &options
            .duration
            .map_or_else(|| "--- capture start ---".to_owned(), |d| format!("--- capture start, {}s ---", d.as_secs())),
    );
    let mut connector = Connector::new(config);
    while let Waited::Connected(mut tty) = connector.wait(emitter, deadline, stop).context(ConnectSnafu)? {
        let present = || config.device.exists();
        if pump(&mut tty, options, PROBE_DELAY, &present, deadline, stop, emitter) == Pumped::Finished {
            break;
        }
        emitter.line("--- link down ---");
        connector.link_lost();
    }
    emitter.line("--- capture end ---");
    Ok(())
}

fn pump<L: Read + Write>(
    link: &mut L,
    options: &LogOptions,
    probe_delay: Duration,
    present: &dyn Fn() -> bool,
    deadline: Option<Instant>,
    stop: &AtomicBool,
    emitter: &mut Emitter,
) -> Pumped {
    let connected = Instant::now();
    let mut probe = options.probe.as_deref();
    let mut lines = LineBuffer::default();
    let mut buf = [0; 4096];
    let ended = loop {
        if stop.load(Ordering::Relaxed) || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break Pumped::Finished;
        }
        if let Some(text) = probe.filter(|_| connected.elapsed() >= probe_delay) {
            probe = None;
            if let Err(e) = link.write_all(format!("{text}\n").as_bytes()) {
                emitter.line(&format!("--- probe not sent: {e} ---"));
            }
        }
        match link.read(&mut buf) {
            Ok(0) => break Pumped::LinkLost,
            Ok(n) => lines.push(&buf[..n], |line| emitter.line(line)),
            Err(e) if is_idle(&e) => {
                if !present() {
                    break Pumped::LinkLost;
                }
            },
            Err(_) => break Pumped::LinkLost,
        }
    };
    lines.finish(|line| emitter.line(line));
    ended
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;
    use crate::emit::tests::Shared;
    use crate::testing::FakeLink;

    fn emitter(out: &Shared) -> Emitter {
        Emitter::new().with_sink(out.clone()).with_stamp(|| "T".into())
    }

    fn pump_once(link: &mut FakeLink, options: &LogOptions, out: &Shared, present: bool) -> Pumped {
        let deadline = Some(Instant::now() + Duration::from_secs(5));
        pump(link, options, Duration::ZERO, &|| present, deadline, &AtomicBool::new(false), &mut emitter(out))
    }

    #[test]
    fn lines_are_stamped_and_the_unterminated_tail_is_kept_when_the_link_closes() {
        let mut link = FakeLink::new().with_rx(b"boot\r\nline two\npart").max_read(5).closing();
        let out = Shared::default();
        let ended = pump_once(&mut link, &LogOptions::default(), &out, true);
        check!(ended == Pumped::LinkLost);
        check!(out.text() == "T boot\nT line two\nT part\n");
    }

    #[test]
    fn the_probe_goes_out_once_after_the_delay() {
        let mut link = FakeLink::new().with_rx(b"hi\n").closing();
        let options = LogOptions { duration: None, probe: Some("probe-from-host".into()) };
        pump_once(&mut link, &options, &Shared::default(), true);
        check!(link.written == b"probe-from-host\n");
    }

    #[test]
    fn no_probe_unless_asked() {
        let mut link = FakeLink::new().with_rx(b"hi\n").closing();
        pump_once(&mut link, &LogOptions::default(), &Shared::default(), true);
        check!(link.written.is_empty());
    }

    #[test]
    fn a_vanished_device_ends_an_idle_link() {
        let mut link = FakeLink::new();
        check!(pump_once(&mut link, &LogOptions::default(), &Shared::default(), false) == Pumped::LinkLost);
    }

    #[test]
    fn stop_finishes_without_reporting_a_loss() {
        let mut link = FakeLink::new();
        let out = Shared::default();
        let ended = pump(
            &mut link,
            &LogOptions::default(),
            PROBE_DELAY,
            &|| true,
            None,
            &AtomicBool::new(true),
            &mut emitter(&out),
        );
        check!(ended == Pumped::Finished);
    }
}
