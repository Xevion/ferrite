//! A text log kept in memory and saved to `\EFI\ferrite\<name>.txt` on the boot volume.

use alloc::format;
use alloc::string::String;

use uefi::boot::{self, ScopedProtocol};
use uefi::proto::media::file::{Directory, File, FileAttribute, FileMode, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CStr16, CString16, Status, cstr16, println};

use crate::clock::Clock;

/// Largest amount of log text held in memory and written to the file.
const CAP_BYTES: usize = 1 << 20;

/// Minimum time between saves requested by [`BootLog::poll`].
const FLUSH_INTERVAL_MS: u64 = 2000;

/// Whether log lines are also printed on the firmware console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    Console,
    /// For apps that own the screen (the ratatui UI).
    Silent,
}

/// Text collected so far and the bookkeeping that decides what to write and when.
#[derive(Debug)]
struct LogBuffer {
    text: String,
    saved: usize,
    dropped: u32,
    marker_saved: u32,
    last_save_ms: u64,
    cap: usize,
}

impl LogBuffer {
    fn new(cap: usize) -> Self {
        Self { text: String::new(), saved: 0, dropped: 0, marker_saved: 0, last_save_ms: 0, cap }
    }

    /// Append a line, or count it as dropped once the cap is reached. Dropping is permanent so the
    /// kept text is always a prefix of the run.
    fn push_line(&mut self, line: &str) {
        if self.dropped == 0 && self.text.len() + line.len() < self.cap {
            self.text.push_str(line);
            self.text.push('\n');
        } else {
            self.dropped = self.dropped.saturating_add(1);
        }
    }

    fn is_dirty(&self) -> bool {
        self.saved < self.text.len() || self.dropped != self.marker_saved
    }

    fn due(&self, now_ms: u64) -> bool {
        self.is_dirty() && now_ms.saturating_sub(self.last_save_ms) >= FLUSH_INTERVAL_MS
    }

    fn text_len(&self) -> usize {
        self.text.len()
    }

    /// The text not yet in the file.
    fn unsaved(&self) -> &str {
        &self.text[self.saved..]
    }

    /// The marker line to write after the text, fixed in width so a later marker overwrites it exactly.
    fn marker(&self) -> Option<String> {
        (self.dropped != self.marker_saved).then(|| format!("[log full: {:>10} lines dropped]\n", self.dropped))
    }

    fn mark_saved(&mut self, now_ms: u64) {
        self.saved = self.text.len();
        self.marker_saved = self.dropped;
        self.last_save_ms = now_ms;
    }
}

/// An open, truncated log file. The volume stays open for as long as the file does.
struct Backing {
    file: RegularFile,
    _volume: ScopedProtocol<SimpleFileSystem>,
}

impl Backing {
    fn create(path: &str) -> uefi::Result<Self> {
        let mut volume = boot::get_image_file_system(boot::image_handle())?;
        let mut root = volume.open_volume()?;
        ensure_dir(&mut root, cstr16!("\\EFI"))?;
        ensure_dir(&mut root, cstr16!("\\EFI\\ferrite"))?;
        let name = CString16::try_from(path).map_err(|_| uefi::Error::from(Status::INVALID_PARAMETER))?;
        if let Ok(old) = root.open(&name, FileMode::ReadWrite, FileAttribute::empty()) {
            let _ = old.delete();
        }
        let file = root
            .open(&name, FileMode::CreateReadWrite, FileAttribute::empty())?
            .into_regular_file()
            .ok_or_else(|| uefi::Error::from(Status::UNSUPPORTED))?;
        Ok(Self { file, _volume: volume })
    }

    /// Append the unsaved text, then the marker (which the next write overwrites), and flush.
    fn write_out(&mut self, buf: &LogBuffer) -> uefi::Result {
        let text = buf.unsaved();
        if !text.is_empty() {
            self.file.write(text.as_bytes()).map_err(|e| e.to_err_without_payload())?;
        }
        if let Some(marker) = buf.marker() {
            self.file.write(marker.as_bytes()).map_err(|e| e.to_err_without_payload())?;
            self.file.set_position(buf.text_len() as u64)?;
        }
        self.file.flush()
    }
}

fn ensure_dir(root: &mut Directory, path: &CStr16) -> uefi::Result {
    if root.open(path, FileMode::Read, FileAttribute::empty()).is_ok() {
        return Ok(());
    }
    root.open(path, FileMode::CreateReadWrite, FileAttribute::DIRECTORY).map(drop)
}

#[expect(clippy::disallowed_macros, reason = "the console echo is one of the two sanctioned println homes")]
fn report_failure(echo: Echo, what: &str, err: &uefi::Error) {
    if echo == Echo::Console {
        println!("boot log: {what} failed ({:?}); further lines stay in memory only", err.status());
    }
}

pub struct BootLog {
    backing: Option<Backing>,
    buf: LogBuffer,
    display: String,
    echo: Echo,
    clock: Clock,
}

impl BootLog {
    /// Open `\EFI\ferrite\<name>.txt`, truncating it. Without a writable boot volume the log
    /// still collects lines in memory and echoes them. Starts the clock, which blocks for 10 ms.
    #[must_use]
    pub fn open(name: &str, echo: Echo) -> Self {
        let clock = Clock::start();
        let display = format!("\\EFI\\ferrite\\{name}.txt");
        let backing = Backing::create(&display).inspect_err(|err| report_failure(echo, "open", err)).ok();
        Self { backing, buf: LogBuffer::new(CAP_BYTES), display, echo, clock }
    }

    /// Append a line. The file is saved according to the flush policy, not on every line.
    #[expect(clippy::disallowed_macros, reason = "the console echo is one of the two sanctioned println homes")]
    pub fn line(&mut self, s: &str) {
        self.buf.push_line(s);
        if self.echo == Echo::Console {
            println!("{s}");
        }
    }

    /// Append a line and save the file now: use before anything that may hang or reset.
    pub fn milestone(&mut self, s: &str) {
        self.line(s);
        self.save();
    }

    /// Save the file if unsaved text has waited at least two seconds. Call from long-running loops.
    pub fn poll(&mut self) {
        if self.buf.due(self.clock.ms()) {
            self.save();
        }
    }

    /// Save the file now. Returns whether the file is intact and up to date.
    pub fn flush(&mut self) -> bool {
        self.save()
    }

    /// Path shown to the user, e.g. `\EFI\ferrite\dbcping.txt`.
    #[must_use]
    pub fn display_path(&self) -> &str {
        &self.display
    }

    #[must_use]
    pub fn echo(&self) -> Echo {
        self.echo
    }

    /// The clock started by [`BootLog::open`], for timestamping lines.
    #[must_use]
    pub fn clock(&self) -> Clock {
        self.clock
    }

    fn save(&mut self) -> bool {
        let Some(backing) = self.backing.as_mut() else {
            return false;
        };
        if !self.buf.is_dirty() {
            return true;
        }
        match backing.write_out(&self.buf) {
            Ok(()) => {
                self.buf.mark_saved(self.clock.ms());
                true
            },
            Err(err) => {
                self.backing = None;
                report_failure(self.echo, "write", &err);
                false
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    fn buffer_with(cap: usize, lines: &[&str]) -> LogBuffer {
        let mut buf = LogBuffer::new(cap);
        for line in lines {
            buf.push_line(line);
        }
        buf
    }

    #[test]
    fn unsaved_is_only_the_text_after_the_last_save() {
        let mut buf = buffer_with(1024, &["one", "two"]);
        check!(buf.unsaved() == "one\ntwo\n");
        buf.mark_saved(0);
        check!(buf.unsaved() == "");
        buf.push_line("three");
        check!(buf.unsaved() == "three\n");
    }

    #[rstest]
    #[case::clean_and_idle(false, 10_000, false)]
    #[case::dirty_inside_interval(true, 1_999, false)]
    #[case::dirty_at_interval(true, 2_000, true)]
    #[case::dirty_past_interval(true, 9_000, true)]
    #[case::clean_past_interval(false, 9_000, false)]
    fn poll_saves_only_dirty_text_after_the_interval(#[case] dirty: bool, #[case] now_ms: u64, #[case] expect: bool) {
        let mut buf = LogBuffer::new(1024);
        if dirty {
            buf.push_line("x");
        }
        check!(buf.due(now_ms) == expect);
    }

    #[test]
    fn a_save_restarts_the_interval() {
        let mut buf = buffer_with(1024, &["x"]);
        buf.mark_saved(5_000);
        buf.push_line("y");
        check!(!buf.due(6_999));
        check!(buf.due(7_000));
    }

    #[test]
    fn lines_past_the_cap_are_counted_and_never_kept() {
        let buf = buffer_with(10, &["abcd", "efg", "h", "ij", "k"]);
        check!(buf.text_len() == 9);
        check!(buf.dropped == 3);
        check!(buf.unsaved() == "abcd\nefg\n");
    }

    #[test]
    fn dropping_is_permanent_even_when_a_shorter_line_would_fit() {
        let buf = buffer_with(10, &["abcd", "efghijkl", "a"]);
        check!(buf.unsaved() == "abcd\n");
        check!(buf.dropped == 2);
    }

    #[test]
    fn the_marker_is_written_once_per_change_in_drops() {
        let mut buf = buffer_with(6, &["abcde", "f"]);
        assert!(let Some(marker) = buf.marker());
        check!(marker.contains("1 lines dropped"));
        buf.mark_saved(0);
        check!(buf.marker().is_none());
        check!(!buf.is_dirty());
        buf.push_line("g");
        check!(buf.is_dirty());
        check!(buf.marker().is_some());
    }

    #[rstest]
    #[case(1)]
    #[case(12_345)]
    #[case(u32::MAX)]
    fn the_marker_width_never_changes(#[case] dropped: u32) {
        let mut buf = LogBuffer::new(0);
        buf.dropped = 1;
        let reference = buf.marker().map(|m| m.len());
        buf.dropped = dropped;
        check!(buf.marker().map(|m| m.len()) == reference);
    }
}
