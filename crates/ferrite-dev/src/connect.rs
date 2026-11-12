//! Waiting for the DbC device to appear, and re-enabling the USB port while it is absent.
//!
//! Linux disables the port after failing to enumerate the target's firmware-time host port, so a
//! DbC device that shows up seconds later is never detected until the port is toggled again.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use rustix::io::Errno;
use snafu::{ResultExt, Snafu};

use crate::config::Config;
use crate::emit::Emitter;
use crate::tty::{Tty, TtyError};

pub const KICK_EVERY: Duration = Duration::from_secs(6);
const KICK_PAUSE: Duration = Duration::from_millis(300);
const POLL_EVERY: Duration = Duration::from_millis(200);

#[derive(Debug, Snafu)]
pub enum ConnectError {
    #[snafu(display("the device exists but cannot be used"))]
    Device { source: TtyError },
}

#[derive(Debug, Snafu)]
enum KickError {
    #[snafu(display("cannot write {}", path.display()))]
    Write { path: PathBuf, source: io::Error },
}

pub enum Waited {
    Connected(Tty),
    TimedOut,
    Stopped,
}

pub struct Connector<'a> {
    config: &'a Config,
    kick_every: Duration,
    last_kick: Instant,
    port_unset_noted: bool,
    last_open_error: Option<Errno>,
}

impl<'a> Connector<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self {
            config,
            kick_every: KICK_EVERY,
            last_kick: Instant::now(),
            port_unset_noted: false,
            last_open_error: None,
        }
    }

    #[must_use]
    pub const fn kick_every(mut self, every: Duration) -> Self {
        self.kick_every = every;
        self
    }

    /// Restart the re-enable timer after a link was lost.
    pub fn link_lost(&mut self) {
        self.last_kick = Instant::now();
    }

    /// Wait until the device can be opened, `deadline` passes or `stop` is set.
    pub fn wait(
        &mut self,
        emitter: &mut Emitter,
        deadline: Option<Instant>,
        stop: &AtomicBool,
    ) -> Result<Waited, ConnectError> {
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(Waited::Stopped);
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(Waited::TimedOut);
            }
            if self.config.device.exists() {
                match Tty::open(&self.config.device) {
                    Ok(tty) => {
                        self.last_open_error = None;
                        emitter.line("--- link up ---");
                        return Ok(Waited::Connected(tty));
                    },
                    Err(TtyError::Open { source, .. } | TtyError::Configure { source, .. })
                        if !matches!(source, Errno::ACCESS | Errno::PERM) =>
                    {
                        if self.last_open_error != Some(source) {
                            emitter.line(&format!("--- device present but not usable yet: {source} ---"));
                            self.last_open_error = Some(source);
                        }
                    },
                    Err(e) => return Err(ConnectError::Device { source: e }),
                }
            } else if self.last_kick.elapsed() >= self.kick_every {
                self.kick(emitter);
            }
            let nap = deadline
                .map_or(POLL_EVERY, |deadline| POLL_EVERY.min(deadline.saturating_duration_since(Instant::now())));
            thread::sleep(nap);
        }
    }

    fn kick(&mut self, emitter: &mut Emitter) {
        let Some(port) = self.config.port_path.as_deref() else {
            if !self.port_unset_noted {
                emitter.line("--- port_path is not set, so the port is not re-enabled ---");
                self.port_unset_noted = true;
            }
            self.last_kick = Instant::now();
            return;
        };
        let state =
            fs::read_to_string(port.join("state")).map_or_else(|e| format!("unreadable: {e}"), |s| s.trim().to_owned());
        match toggle(port) {
            Ok(()) => emitter.line(&format!("--- port re-enabled (state was: {state}) ---")),
            Err(e) => {
                emitter.line(&format!("--- port re-enable failed (state was: {state}): {} ---", crate::chain(&e)));
            },
        }
        self.last_kick = Instant::now();
    }
}

fn toggle(port: &Path) -> Result<(), KickError> {
    let disable = port.join("disable");
    fs::write(&disable, "1").context(WriteSnafu { path: &disable })?;
    thread::sleep(KICK_PAUSE);
    fs::write(&disable, "0").context(WriteSnafu { path: &disable })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use assert2::{assert, check};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    use super::*;
    use crate::emit::tests::Shared;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ferrite-dev-test-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn emitter(out: &Shared) -> Emitter {
        Emitter::new().with_sink(out.clone()).with_stamp(|| "T".into())
    }

    #[test]
    fn an_absent_device_gets_the_port_toggled_and_logged() {
        let dir = scratch("kick");
        fs::write(dir.join("state"), "disabled\n").expect("state");
        let config = Config { device: dir.join("no-such-tty"), port_path: Some(dir.clone()), ..Config::default() };
        let out = Shared::default();
        let mut connector = Connector::new(&config).kick_every(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_millis(700);
        let waited = connector.wait(&mut emitter(&out), Some(deadline), &AtomicBool::new(false));
        assert!(let Ok(Waited::TimedOut) = waited);
        check!(out.text().contains("T --- port re-enabled (state was: disabled) ---\n"), "{}", out.text());
        check!(fs::read_to_string(dir.join("disable")).ok().as_deref() == Some("0"), "ends enabled");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_unset_port_path_is_noted_once_and_not_toggled() {
        let config = Config { device: "/nonexistent/ferrite-tty".into(), ..Config::default() };
        let out = Shared::default();
        let mut connector = Connector::new(&config).kick_every(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_millis(700);
        let waited = connector.wait(&mut emitter(&out), Some(deadline), &AtomicBool::new(false));
        assert!(let Ok(Waited::TimedOut) = waited);
        check!(out.text() == "T --- port_path is not set, so the port is not re-enabled ---\n", "{}", out.text());
    }

    #[test]
    fn a_present_device_is_opened_and_announced() {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("ptmx");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let slave = PathBuf::from(ptsname(&master, Vec::new()).expect("ptsname").to_str().expect("ascii"));
        let config = Config { device: slave, ..Config::default() };
        let out = Shared::default();
        let waited = Connector::new(&config).wait(&mut emitter(&out), None, &AtomicBool::new(false));
        assert!(let Ok(Waited::Connected(_)) = waited);
        check!(out.text() == "T --- link up ---\n");
    }

    #[test]
    fn stop_ends_the_wait() {
        let config = Config { device: PathBuf::from("/nonexistent/ferrite-dev-test"), ..Config::default() };
        let waited = Connector::new(&config).wait(&mut Emitter::new(), None, &AtomicBool::new(true));
        assert!(let Ok(Waited::Stopped) = waited);
    }
}
