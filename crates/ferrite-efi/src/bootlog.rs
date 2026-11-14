//! A text log kept in memory and saved to `\EFI\ferrite\<name>.txt` on the boot volume.

use alloc::format;
use alloc::string::String;
use core::mem::ManuallyDrop;
use core::time::Duration;

use ferrite_hal::{Clock as _, Instant};
use uefi::boot::ScopedProtocol;
use uefi::proto::media::file::{Directory, File, FileAttribute, FileMode, RegularFile};
use uefi::proto::media::fs::SimpleFileSystem;
use uefi::{CStr16, CString16, Status, cstr16, println, system};

use crate::clock::Clock;
use crate::volume::image_volume;

/// Largest amount of log text held in memory and written to the file.
const CAP_BYTES: usize = 1 << 20;

/// Minimum time between saves requested by [`BootLog::poll`].
const FLUSH_INTERVAL: Duration = Duration::from_secs(2);

/// An echo taking this long means the console scrolls by copying its framebuffer.
const SLOW_ECHO: Duration = Duration::from_millis(20);
/// Console size assumed when the firmware does not report one; every console has mode 0.
const MODE0: Grid = Grid { columns: 80, rows: 25 };

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
    last_save: Instant,
    cap: usize,
}

impl LogBuffer {
    fn new(cap: usize) -> Self {
        Self { text: String::new(), saved: 0, dropped: 0, marker_saved: 0, last_save: Instant::default(), cap }
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

    fn due(&self, now: Instant) -> bool {
        self.is_dirty() && now - self.last_save >= FLUSH_INTERVAL
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

    fn mark_saved(&mut self, now: Instant) {
        self.saved = self.text.len();
        self.marker_saved = self.dropped;
        self.last_save = now;
    }
}

/// An open, truncated log file. The volume stays open for as long as the file does.
struct Backing {
    file: RegularFile,
    _volume: ManuallyDrop<ScopedProtocol<SimpleFileSystem>>,
}

impl Backing {
    fn create(path: &str, echo: Echo) -> uefi::Result<Self> {
        let mut volume = image_volume()?;
        let mut root = volume.open_volume()?;
        ensure_dir(&mut root, cstr16!("\\EFI"))?;
        ensure_dir(&mut root, cstr16!("\\EFI\\ferrite"))?;
        ensure_parents(&mut root, path)?;
        let name = CString16::try_from(path).map_err(|_| uefi::Error::from(Status::INVALID_PARAMETER))?;
        if let Ok(old) = root.open(&name, FileMode::ReadWrite, FileAttribute::empty())
            && let Err(err) = old.delete()
        {
            report_failure(echo, "deleting the old log", &err, "the end of the old run may follow this one");
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

/// Replace the file at `path` with exactly `bytes`, creating the directories.
///
/// `path` is `\`-separated from the volume's root, in `\EFI\ferrite` or below it, at any depth. This is for small
/// state kept between runs; a failure part way leaves the old file deleted or short, so readers
/// must tolerate that.
pub fn replace_file(path: &str, bytes: &[u8]) -> uefi::Result {
    let mut backing = Backing::create(path, Echo::Silent)?;
    backing.file.write(bytes).map_err(|e| e.to_err_without_payload())?;
    backing.file.flush()
}

/// Create the directories of `path` below `\EFI\ferrite`, whose own parent is already there.
fn ensure_parents(root: &mut Directory, path: &str) -> uefi::Result {
    const BASE: &str = "\\EFI\\ferrite\\";
    let Some(below) = path.strip_prefix(BASE) else { return Ok(()) };
    for (at, _) in below.match_indices('\\') {
        let dir =
            CString16::try_from(&path[..BASE.len() + at]).map_err(|_| uefi::Error::from(Status::INVALID_PARAMETER))?;
        ensure_dir(root, &dir)?;
    }
    Ok(())
}

fn ensure_dir(root: &mut Directory, path: &CStr16) -> uefi::Result {
    if root.open(path, FileMode::Read, FileAttribute::empty()).is_ok() {
        return Ok(());
    }
    root.open(path, FileMode::CreateReadWrite, FileAttribute::DIRECTORY).map(drop)
}

#[expect(clippy::disallowed_macros, reason = "the console echo is one of the two sanctioned println homes")]
fn report_failure(echo: Echo, what: &str, err: &uefi::Error, consequence: &str) {
    if echo == Echo::Console {
        println!("boot log: {what} failed ({:?}); {consequence}", err.status());
    }
}

const MEMORY_ONLY: &str = "further lines stay in memory only";

/// Text columns and rows of the console.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Grid {
    columns: usize,
    rows: usize,
}

/// Rows `text` takes when printed and followed by a newline. A segment that
/// exactly fills a row counts one extra, since the cursor wraps before the newline.
fn rows_needed(text: &str, columns: usize) -> usize {
    text.split('\n').map(|segment| segment.chars().count() / columns.max(1) + 1).sum()
}

/// The firmware console as an echo target.
///
/// A firmware console may scroll by moving the whole framebuffer. On the X1 Carbon that
/// costs about 200 ms per line once the screen is full, which stalls every loop
/// that logs. The first echo that slow switches the console to paging: it clears
/// the screen when the next line would scroll, so a screenful costs one clear
/// instead of a scroll per line. Fast consoles (QEMU, serial) keep scrolling.
struct Console {
    paging: Option<Grid>,
}

impl Console {
    #[expect(clippy::disallowed_macros, reason = "the console echo is one of the two sanctioned println homes")]
    fn echo(&mut self, clock: &Clock, line: &str) {
        if let Some(grid) = self.paging {
            let needed = rows_needed(line, grid.columns);
            system::with_stdout(|out| {
                if out.cursor_position().1 + needed >= grid.rows {
                    // A failed clear only means this line scrolls.
                    let _ = out.clear();
                }
            });
        }
        let t0 = clock.now();
        println!("{line}");
        if self.paging.is_none() && clock.elapsed(t0) >= SLOW_ECHO {
            self.paging = Some(current_grid());
        }
    }
}

fn current_grid() -> Grid {
    system::with_stdout(|out| out.current_mode().ok().flatten())
        .map_or(MODE0, |mode| Grid { columns: mode.columns(), rows: mode.rows() })
}

pub struct BootLog {
    backing: Option<Backing>,
    buf: LogBuffer,
    display: String,
    console: Option<Console>,
    clock: Clock,
}

impl BootLog {
    /// Open `\EFI\ferrite\<name>.txt`, truncating it. Without a writable boot volume the log
    /// still collects lines in memory and echoes them. Starts the clock, which blocks for 10 ms.
    #[must_use]
    pub fn open(name: &str, echo: Echo) -> Self {
        let clock = Clock::start();
        let display = format!("\\EFI\\ferrite\\{name}.txt");
        let backing =
            Backing::create(&display, echo).inspect_err(|err| report_failure(echo, "open", err, MEMORY_ONLY)).ok();
        let console = (echo == Echo::Console).then_some(Console { paging: None });
        Self { backing, buf: LogBuffer::new(CAP_BYTES), display, console, clock }
    }

    /// Append a line. The file is saved according to the flush policy, not on every line.
    pub fn line(&mut self, s: &str) {
        self.buf.push_line(s);
        if let Some(console) = self.console.as_mut() {
            console.echo(&self.clock, s);
        }
    }

    /// Append a line and save the file now: use before anything that may hang or reset.
    pub fn milestone(&mut self, s: &str) {
        self.line(s);
        self.save();
    }

    /// Save the file if unsaved text has waited at least two seconds. Call from long-running loops.
    pub fn poll(&mut self) {
        if self.buf.due(self.clock.now()) {
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
        if self.console.is_some() { Echo::Console } else { Echo::Silent }
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
                self.buf.mark_saved(self.clock.now());
                true
            },
            Err(err) => {
                self.backing = None;
                report_failure(self.echo(), "write", &err, MEMORY_ONLY);
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
        buf.mark_saved(Instant::default());
        check!(buf.unsaved() == "");
        buf.push_line("three");
        check!(buf.unsaved() == "three\n");
    }

    fn at(ms: u64) -> Instant {
        Instant::from_epoch(Duration::from_millis(ms))
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
        check!(buf.due(at(now_ms)) == expect);
    }

    #[test]
    fn a_save_restarts_the_interval() {
        let mut buf = buffer_with(1024, &["x"]);
        buf.mark_saved(at(5_000));
        buf.push_line("y");
        check!(!buf.due(at(6_999)));
        check!(buf.due(at(7_000)));
    }

    #[rstest]
    #[case::empty("", 1)]
    #[case::short("hello", 1)]
    #[case::one_short_of_a_row(&"x".repeat(79), 1)]
    #[case::exactly_a_row(&"x".repeat(80), 2)]
    #[case::wraps_twice(&"x".repeat(170), 3)]
    #[case::snapshot_lines("DCCTRL=0x8a000003\nDCST=0x03000000\n", 3)]
    fn rows_needed_counts_wraps_and_newlines(#[case] text: &str, #[case] rows: usize) {
        check!(rows_needed(text, 80) == rows);
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
        buf.mark_saved(Instant::default());
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
