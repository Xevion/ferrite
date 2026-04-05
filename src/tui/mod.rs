pub mod activity;
pub mod palette;
pub mod render;
pub mod run;

pub use activity::ActivityBuffer;
pub use render::SymbolSet;

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::prelude::Widget;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tracing::info;
use tracing_subscriber::fmt::MakeWriter;

use render::render_heatmap;

/// A test error record for TUI display. String-based so it's decoupled from
/// the main crate's `Failure` type.
#[derive(Debug)]
pub struct TuiError {
    pub region_idx: usize,
    pub region_name: String,
    pub address: u64,
    pub expected: u64,
    pub actual: u64,
    pub bit_position: u8,
    pub pattern: String,
    pub progress_fraction: f64,
}

/// Events flowing into the TUI event loop.
#[derive(Debug)]
pub enum TuiEvent {
    Key(event::KeyEvent),
    Tick,
    /// A pre-formatted ANSI log line from `tracing_subscriber::fmt`.
    Log(String),
    Error(TuiError),
    RegionDone(usize),
}

/// TUI display configuration.
pub struct TuiConfig {
    pub symbols: SymbolSet,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            symbols: SymbolSet::Braille,
        }
    }
}

/// Shared state for a single memory region being tested.
///
/// Workers update atomics from their threads; the TUI reads them for rendering.
pub struct RegionState {
    pub name: String,
    pub size_bytes: usize,
    patterns: Vec<String>,
    pub current_pattern_idx: AtomicUsize,
    pub progress_bp: AtomicU64,
    pub error_count: AtomicUsize,
    pub paused: AtomicBool,
    pub activity: ActivityBuffer,
    last_error_time: Mutex<Option<Instant>>,
}

impl RegionState {
    #[must_use]
    pub fn new(name: String, size_bytes: usize, patterns: Vec<String>) -> Self {
        Self {
            name,
            size_bytes,
            patterns,
            current_pattern_idx: AtomicUsize::new(0),
            progress_bp: AtomicU64::new(0),
            error_count: AtomicUsize::new(0),
            paused: AtomicBool::new(false),
            activity: ActivityBuffer::new(),
            last_error_time: Mutex::new(None),
        }
    }

    /// Current pattern name, or "done" if all patterns are complete.
    pub fn current_pattern(&self) -> &str {
        let idx = self.current_pattern_idx.load(Ordering::Relaxed);
        self.patterns
            .get(idx)
            .map_or("done", std::string::String::as_str)
    }

    /// Advance to the given pattern index and reset progress.
    pub fn set_pattern(&self, idx: usize) {
        self.current_pattern_idx.store(idx, Ordering::Relaxed);
        self.progress_bp.store(0, Ordering::Relaxed);
    }

    /// Record that an error was found (increments count, updates timestamp).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn record_error(&self) {
        self.error_count.fetch_add(1, Ordering::Relaxed);
        *self.last_error_time.lock().unwrap() = Some(Instant::now());
    }

    /// Seconds since the last error, or `f64::MAX` if none.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn last_error_age_secs(&self) -> f64 {
        self.last_error_time
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(f64::MAX)
    }
}

#[allow(clippy::missing_fields_in_debug)]
impl fmt::Debug for RegionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegionState")
            .field("name", &self.name)
            .field("size_bytes", &self.size_bytes)
            .field("pattern", &self.current_pattern())
            .field("progress_bp", &self.progress_bp.load(Ordering::Relaxed))
            .field("errors", &self.error_count.load(Ordering::Relaxed))
            .finish()
    }
}

/// A [`MakeWriter`] that sends `tracing_subscriber::fmt`-formatted lines through
/// a channel while the TUI is active. When the channel disconnects (TUI exited),
/// output falls back to stderr so post-TUI tracing isn't lost.
#[derive(Clone)]
pub struct TuiMakeWriter {
    tx: mpsc::SyncSender<TuiEvent>,
}

impl TuiMakeWriter {
    #[must_use]
    pub fn new(tx: mpsc::SyncSender<TuiEvent>) -> Self {
        Self { tx }
    }
}

impl<'a> MakeWriter<'a> for TuiMakeWriter {
    type Writer = TuiWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TuiWriter {
            tx: self.tx.clone(),
            buf: Vec::with_capacity(256),
        }
    }
}

/// Per-event writer that buffers a single formatted log line.
/// On drop, sends the line through the channel or falls back to stderr.
pub struct TuiWriter {
    tx: mpsc::SyncSender<TuiEvent>,
    buf: Vec<u8>,
}

impl Write for TuiWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for TuiWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let msg = String::from_utf8_lossy(&self.buf);
        let trimmed = msg.trim_end_matches('\n').to_string();
        if trimmed.is_empty() {
            return;
        }
        // Send to TUI for inline display (best-effort).
        // A separate tracing layer handles stderr output independently.
        let _ = self.tx.try_send(TuiEvent::Log(trimmed));
    }
}

/// Run the TUI event loop. Blocks until the user quits or all regions complete.
///
/// The caller should:
/// 1. Create a channel: `mpsc::sync_channel::<TuiEvent>(256)`
/// 2. Spawn worker threads that send `TuiEvent::Error` and `TuiEvent::RegionDone`
/// 3. Optionally set up tracing with `TuiLogLayer` using the sender
/// 4. Call this function with both ends of the channel and a shared quit flag
///
/// `run_tui` spawns internal threads for keyboard input and tick events.
///
/// # Errors
///
/// Returns an error if raw mode cannot be enabled, terminal initialization
/// fails, or drawing to the terminal fails.
///
/// # Panics
///
/// Panics if the input or tick thread cannot be spawned.
#[allow(clippy::too_many_lines)]
pub fn run_tui(
    config: &TuiConfig,
    regions: &[Arc<RegionState>],
    tx: &mpsc::SyncSender<TuiEvent>,
    rx: &mpsc::Receiver<TuiEvent>,
    quit: &Arc<AtomicBool>,
) -> anyhow::Result<()> {
    // Panic hook for terminal cleanup
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        original_hook(info);
    }));

    enable_raw_mode().context("failed to enable raw mode (is stdout a terminal?)")?;

    let viewport_height = (regions.len() + 8) as u16;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )
    .context("failed to initialize terminal")?;

    // Input reader thread
    let input_tx = tx.clone();
    let input_quit = Arc::clone(quit);
    thread::Builder::new()
        .name("tui-input".into())
        .spawn(move || {
            while !input_quit.load(Ordering::Relaxed) {
                if event::poll(Duration::from_millis(50)).unwrap_or(false)
                    && let Ok(Event::Key(key)) = event::read()
                {
                    let _ = input_tx.try_send(TuiEvent::Key(key));
                }
            }
        })
        .expect("failed to spawn tui-input thread");

    // Tick thread
    let tick_tx = tx.clone();
    let tick_quit = Arc::clone(quit);
    thread::Builder::new()
        .name("tui-tick".into())
        .spawn(move || {
            while !tick_quit.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(100));
                let _ = tick_tx.try_send(TuiEvent::Tick);
            }
        })
        .expect("failed to spawn tui-tick thread");

    let start_time = Instant::now();
    let mut errors: Vec<TuiError> = Vec::new();
    let mut regions_done = 0;
    let mut verbose = false;
    let total_regions = regions.len();

    // Establish the inline viewport before processing any events — insert_before
    // misbehaves if called before the first draw.
    terminal.draw(|frame| {
        render_heatmap(
            frame,
            regions,
            &errors,
            start_time.elapsed(),
            verbose,
            config.symbols,
        );
    })?;

    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(TuiEvent::Key(key)) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    info!("interrupted");
                    quit.store(true, Ordering::Relaxed);
                    break;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        info!("user requested quit");
                        quit.store(true, Ordering::Relaxed);
                        break;
                    }
                    KeyCode::Char('p') => {
                        let any_paused = regions.iter().any(|r| r.paused.load(Ordering::Relaxed));
                        for r in regions {
                            r.paused.store(!any_paused, Ordering::Relaxed);
                        }
                        if any_paused {
                            info!("resumed all regions");
                        } else {
                            info!("paused all regions");
                        }
                    }
                    KeyCode::Char('s') => {
                        info!("skip requested (would skip current pattern)");
                    }
                    KeyCode::Char('v') => {
                        verbose = !verbose;
                        info!(verbose, "toggled verbosity");
                    }
                    _ => {}
                }
            }
            Ok(TuiEvent::Log(msg)) => {
                if let Ok(text) = ansi_to_tui::IntoText::into_text(&msg) {
                    terminal.insert_before(1, |buf| {
                        Paragraph::new(text).render(buf.area, buf);
                    })?;
                }
            }
            Ok(TuiEvent::Error(err)) => {
                errors.push(err);
            }
            Ok(TuiEvent::RegionDone(idx)) => {
                regions_done += 1;
                info!(region = regions[idx].name.as_str(), "region complete");
                if regions_done >= total_regions {
                    info!("all regions complete");
                    quit.store(true, Ordering::Relaxed);
                    break;
                }
            }
            Ok(TuiEvent::Tick) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        let elapsed = start_time.elapsed();
        terminal.draw(|frame| {
            render_heatmap(frame, regions, &errors, elapsed, verbose, config.symbols);
        })?;
    }

    let elapsed = start_time.elapsed();
    terminal.draw(|frame| {
        render_heatmap(frame, regions, &errors, elapsed, verbose, config.symbols);
    })?;

    let total_errors: usize = regions
        .iter()
        .map(|r| r.error_count.load(Ordering::Relaxed))
        .sum();
    let (summary_text, summary_style) = if total_errors > 0 {
        (
            format!(
                "FAIL: {total_errors} error(s) found in {:.1}s",
                elapsed.as_secs_f64()
            ),
            ratatui::style::Style::default()
                .fg(palette::ERR_HIGH)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )
    } else {
        (
            format!("PASS: no errors in {:.1}s", elapsed.as_secs_f64()),
            ratatui::style::Style::default()
                .fg(palette::ERR_NONE)
                .add_modifier(ratatui::style::Modifier::BOLD),
        )
    };
    terminal.insert_before(1, |buf| {
        use ratatui::text::{Line, Span};
        Paragraph::new(Line::from(Span::styled(summary_text, summary_style))).render(buf.area, buf);
    })?;

    // Clear the inline viewport content while raw mode is still active,
    // then restore the terminal so the shell prompt appears cleanly below.
    terminal.clear().context("failed to clear terminal")?;
    disable_raw_mode().context("failed to disable raw mode")?;
    println!();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn tui_config_default_uses_braille() {
        let config = TuiConfig::default();
        assert_eq!(config.symbols, SymbolSet::Braille);
    }

    #[test]
    fn region_state_new_defaults() {
        let rs = RegionState::new("test".into(), 4096, vec!["solid".into(), "walk".into()]);
        assert_eq!(rs.name, "test");
        assert_eq!(rs.size_bytes, 4096);
        assert_eq!(rs.current_pattern(), "solid");
        assert_eq!(rs.progress_bp.load(Ordering::Relaxed), 0);
        assert_eq!(rs.error_count.load(Ordering::Relaxed), 0);
        assert!(!rs.paused.load(Ordering::Relaxed));
    }

    #[test]
    fn current_pattern_returns_correct_pattern() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(rs.current_pattern(), "a");
        rs.current_pattern_idx.store(1, Ordering::Relaxed);
        assert_eq!(rs.current_pattern(), "b");
        rs.current_pattern_idx.store(2, Ordering::Relaxed);
        assert_eq!(rs.current_pattern(), "c");
    }

    #[test]
    fn current_pattern_returns_done_past_end() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        rs.current_pattern_idx.store(5, Ordering::Relaxed);
        assert_eq!(rs.current_pattern(), "done");
    }

    #[test]
    fn set_pattern_updates_index_and_resets_progress() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into(), "b".into()]);
        rs.progress_bp.store(5000, Ordering::Relaxed);
        rs.set_pattern(1);
        assert_eq!(rs.current_pattern(), "b");
        assert_eq!(rs.progress_bp.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn record_error_increments_count() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        assert_eq!(rs.error_count.load(Ordering::Relaxed), 0);
        rs.record_error();
        assert_eq!(rs.error_count.load(Ordering::Relaxed), 1);
        rs.record_error();
        assert_eq!(rs.error_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn last_error_age_max_when_no_errors() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        assert_eq!(rs.last_error_age_secs(), f64::MAX);
    }

    #[test]
    fn last_error_age_small_after_error() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        rs.record_error();
        let age = rs.last_error_age_secs();
        assert!(
            age < 1.0,
            "age should be very small immediately after error, got {age}"
        );
    }

    #[test]
    fn debug_format_includes_fields() {
        let rs = RegionState::new("test-region".into(), 8192, vec!["solid".into()]);
        rs.error_count.store(3, Ordering::Relaxed);
        rs.progress_bp.store(5000, Ordering::Relaxed);
        let debug = format!("{rs:?}");
        assert!(debug.contains("test-region"));
        assert!(debug.contains("8192"));
        assert!(debug.contains("solid"));
        assert!(debug.contains("5000"));
        assert!(debug.contains('3'));
    }

    #[test]
    fn tui_writer_sends_through_channel() {
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
        let mut writer = TuiWriter {
            tx,
            buf: Vec::new(),
        };
        writer.write_all(b" INFO ferrite: hello world\n").unwrap();
        drop(writer);
        match rx.try_recv() {
            Ok(TuiEvent::Log(msg)) => {
                assert!(msg.contains("hello world"));
                assert!(!msg.ends_with('\n'), "trailing newline should be trimmed");
            }
            other => panic!("expected TuiEvent::Log, got {other:?}"),
        }
    }

    #[test]
    fn tui_writer_empty_buffer_sends_nothing() {
        let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
        let writer = TuiWriter {
            tx,
            buf: Vec::new(),
        };
        drop(writer);
        assert!(
            rx.try_recv().is_err(),
            "empty buffer should not send an event"
        );
    }
}
