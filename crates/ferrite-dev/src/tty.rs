//! The DbC tty opened raw, as a `Read + Write` link with bounded waits.
//!
//! The link contract used by every consumer: `read` returns `Ok(0)` only when the far side is
//! gone, and `TimedOut` or `WouldBlock` when nothing arrived yet. Any other error is fatal.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags, open};
use rustix::termios::{ControlModes, OptionalActions, SpecialCodeIndex, tcgetattr, tcsetattr};
use snafu::{ResultExt, Snafu};

/// How long one `read` waits for data before reporting that it is idle.
pub const IDLE_SLICE: Duration = Duration::from_millis(100);
const WRITE_STALL: Duration = Duration::from_secs(1);

#[derive(Debug, Snafu)]
pub enum TtyError {
    #[snafu(display("cannot open {}", path.display()))]
    Open { path: PathBuf, source: rustix::io::Errno },

    #[snafu(display("cannot put {} in raw mode", path.display()))]
    Configure { path: PathBuf, source: rustix::io::Errno },
}

/// Whether a link error only means that nothing has happened yet.
pub fn is_idle(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock)
}

pub struct Tty {
    file: File,
}

impl Tty {
    /// Open `path` without taking it as the controlling terminal, and switch it to raw mode.
    pub fn open(path: &Path) -> Result<Self, TtyError> {
        let fd = open(path, OFlags::RDWR | OFlags::NOCTTY | OFlags::NONBLOCK | OFlags::CLOEXEC, Mode::empty())
            .context(OpenSnafu { path })?;
        let file = File::from(fd);
        let mut termios = tcgetattr(&file).context(ConfigureSnafu { path })?;
        termios.make_raw();
        termios.control_modes |= ControlModes::CLOCAL | ControlModes::CREAD;
        termios.special_codes[SpecialCodeIndex::VMIN] = 1;
        termios.special_codes[SpecialCodeIndex::VTIME] = 0;
        tcsetattr(&file, OptionalActions::Now, &termios).context(ConfigureSnafu { path })?;
        Ok(Self { file })
    }

    fn wait(&self, flags: PollFlags, timeout: Duration) -> io::Result<bool> {
        let limit = Timespec { tv_sec: timeout.as_secs().cast_signed(), tv_nsec: i64::from(timeout.subsec_nanos()) };
        let mut fds = [PollFd::new(&self.file, flags)];
        match poll(&mut fds, Some(&limit)) {
            Ok(ready) => Ok(ready > 0),
            Err(rustix::io::Errno::INTR) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

impl Read for Tty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.file.read(buf) {
                Ok(0) if !buf.is_empty() => return Err(io::ErrorKind::BrokenPipe.into()),
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {},
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !self.wait(PollFlags::IN, IDLE_SLICE)? {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                },
                Err(e) => return Err(e),
            }
        }
    }
}

impl Write for Tty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            match self.file.write(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {},
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if !self.wait(PollFlags::OUT, WRITE_STALL)? {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                },
                other => return other,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::time::Instant;

    use assert2::{assert, check};
    use rustix::io::{read, write};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

    use super::*;

    fn pty() -> (OwnedFd, PathBuf) {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("ptmx");
        grantpt(&master).expect("grantpt");
        unlockpt(&master).expect("unlockpt");
        let slave = ptsname(&master, Vec::new()).expect("ptsname");
        (master, PathBuf::from(slave.to_str().expect("ascii path")))
    }

    #[test]
    fn bytes_arrive_untranslated_and_silence_is_idle() {
        let (master, path) = pty();
        assert!(let Ok(mut tty) = Tty::open(&path));

        let started = Instant::now();
        assert!(let Err(idle) = tty.read(&mut [0; 16]));
        check!(is_idle(&idle));
        check!(started.elapsed() < Duration::from_secs(2));

        write(&master, b"x\r\x03\x11y").expect("master write");
        let mut buf = [0; 16];
        assert!(let Ok(n) = tty.read(&mut buf));
        check!(buf[..n] == *b"x\r\x03\x11y", "raw mode passes CR, ^C and ^Q through");
    }

    #[test]
    fn written_bytes_are_not_echoed_or_translated() {
        let (master, path) = pty();
        assert!(let Ok(mut tty) = Tty::open(&path));
        assert!(let Ok(()) = tty.write_all(b"line\n"));
        let mut buf = [0; 16];
        assert!(let Ok(n) = read(&master, &mut buf));
        check!(buf[..n] == *b"line\n", "no CRLF expansion from OPOST");
    }
}
