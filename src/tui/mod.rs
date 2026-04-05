#![cfg_attr(coverage_nightly, coverage(off))]

pub mod activity;
pub mod palette;
pub mod render;
pub mod run;

pub use activity::ActivityBuffer;
pub use render::SymbolSet;

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, thread};

use anyhow::Context;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::prelude::Widget;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tracing::info;
use tracing_subscriber::fmt::MakeWriter;

use render::render_heatmap;

/// Outcome of the TUI event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiOutcome {
    /// User pressed 'q', Esc, or Ctrl+C.
    Quit,
    /// All regions finished testing.
    AllComplete,
    /// Event channel disconnected (all senders dropped).
    Disconnected,
}

/// Result returned by [`run_event_loop`], capturing loop state for the caller.
pub struct TuiLoopResult {
    pub outcome: TuiOutcome,
    pub errors: Vec<TuiError>,
    pub verbose: bool,
}

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

/// Core event loop: processes events from `rx`, renders to `terminal`.
///
/// Generic over the backend so tests can use `TestBackend`. Returns a
/// [`TuiLoopResult`] with the exit reason, collected errors, and verbose state.
///
/// # Errors
///
/// Returns an error if drawing to the terminal fails.
#[allow(clippy::too_many_lines)]
pub fn run_event_loop<B>(
    terminal: &mut Terminal<B>,
    config: &TuiConfig,
    regions: &[Arc<RegionState>],
    rx: &mpsc::Receiver<TuiEvent>,
) -> anyhow::Result<TuiLoopResult>
where
    B: ratatui::backend::Backend,
    B::Error: Send + Sync + 'static,
{
    let start_time = Instant::now();
    let mut errors: Vec<TuiError> = Vec::new();
    // Pending log lines, drained once per tick to bound insert_before calls.
    let mut log_buf: VecDeque<ratatui::text::Text<'static>> = VecDeque::with_capacity(32);
    let mut regions_done = 0;
    let mut verbose = false;
    let total_regions = regions.len();

    // Establish the viewport before processing any events -- insert_before
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

    let outcome = loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(TuiEvent::Key(key)) => {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    info!("interrupted");
                    crate::shutdown::escalate();
                    break TuiOutcome::Quit;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        info!("user requested quit");
                        crate::shutdown::request_quit(crate::shutdown::QuitReason::UserQuit);
                        break TuiOutcome::Quit;
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
                    if log_buf.len() >= 32 {
                        log_buf.pop_front();
                    }
                    log_buf.push_back(text);
                }
            }
            Ok(TuiEvent::Error(err)) => {
                errors.push(err);
            }
            Ok(TuiEvent::RegionDone(idx)) => {
                regions_done += 1;
                let region = regions.get(idx).with_context(|| {
                    format!("RegionDone({idx}) out of bounds (len={})", regions.len())
                })?;
                info!(region = region.name.as_str(), "region complete");
                if regions_done >= total_regions {
                    info!("all regions complete");
                    crate::shutdown::request_quit(crate::shutdown::QuitReason::UserQuit);
                    break TuiOutcome::AllComplete;
                }
            }
            Ok(TuiEvent::Tick) | Err(mpsc::RecvTimeoutError::Timeout) => {
                if !log_buf.is_empty() {
                    let lines: Vec<_> = log_buf.drain(..).collect();
                    terminal.insert_before(lines.len() as u16, |buf| {
                        for (i, text) in lines.into_iter().enumerate() {
                            let area = ratatui::layout::Rect {
                                y: buf.area.y + i as u16,
                                height: 1,
                                ..buf.area
                            };
                            Paragraph::new(text).render(area, buf);
                        }
                    })?;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break TuiOutcome::Disconnected,
        }

        let elapsed = start_time.elapsed();
        terminal.draw(|frame| {
            render_heatmap(frame, regions, &errors, elapsed, verbose, config.symbols);
        })?;
    };

    // Final render to capture end state.
    let elapsed = start_time.elapsed();
    terminal.draw(|frame| {
        render_heatmap(frame, regions, &errors, elapsed, verbose, config.symbols);
    })?;

    Ok(TuiLoopResult {
        outcome,
        errors,
        verbose,
    })
}

/// Run the TUI event loop with a real terminal. Blocks until the user quits
/// or all regions complete.
///
/// This is the production entry point: it sets up raw mode, an inline viewport,
/// and spawns input/tick threads, then delegates to [`run_event_loop`].
///
/// # Errors
///
/// Returns an error if raw mode cannot be enabled, terminal initialization
/// fails, or drawing to the terminal fails.
///
/// # Panics
///
/// Panics if the input or tick thread cannot be spawned.
pub fn run_tui(
    config: &TuiConfig,
    regions: &[Arc<RegionState>],
    tx: &mpsc::SyncSender<TuiEvent>,
    rx: &mpsc::Receiver<TuiEvent>,
) -> anyhow::Result<()> {
    let guard = crate::shutdown::TerminalGuard::new()?;

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
    thread::Builder::new()
        .name("tui-input".into())
        .spawn(move || {
            while !crate::shutdown::quit_requested() {
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
    thread::Builder::new()
        .name("tui-tick".into())
        .spawn(move || {
            while !crate::shutdown::quit_requested() {
                thread::sleep(Duration::from_millis(100));
                let _ = tick_tx.try_send(TuiEvent::Tick);
            }
        })
        .expect("failed to spawn tui-tick thread");

    let start_time = Instant::now();
    run_event_loop(&mut terminal, config, regions, rx)?;

    // Render summary line above the viewport.
    let elapsed = start_time.elapsed();
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

    // Clear the inline viewport while raw mode is still active (guard drops after return).
    terminal.clear().context("failed to clear terminal")?;
    // Explicitly drop the guard before println so the terminal is restored first.
    drop(guard);
    println!();
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn tui_config_default_uses_braille() {
        let config = TuiConfig::default();
        check!(config.symbols == SymbolSet::Braille);
    }

    #[test]
    fn region_state_new_defaults() {
        let rs = RegionState::new("test".into(), 4096, vec!["solid".into(), "walk".into()]);
        check!(rs.name == "test");
        check!(rs.size_bytes == 4096);
        check!(rs.current_pattern() == "solid");
        check!(rs.progress_bp.load(Ordering::Relaxed) == 0);
        check!(rs.error_count.load(Ordering::Relaxed) == 0);
        assert!(!rs.paused.load(Ordering::Relaxed));
    }

    #[test]
    fn current_pattern_returns_correct_pattern() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into(), "b".into(), "c".into()]);
        check!(rs.current_pattern() == "a");
        rs.current_pattern_idx.store(1, Ordering::Relaxed);
        check!(rs.current_pattern() == "b");
        rs.current_pattern_idx.store(2, Ordering::Relaxed);
        check!(rs.current_pattern() == "c");
    }

    #[test]
    fn current_pattern_returns_done_past_end() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        rs.current_pattern_idx.store(5, Ordering::Relaxed);
        check!(rs.current_pattern() == "done");
    }

    #[test]
    fn set_pattern_updates_index_and_resets_progress() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into(), "b".into()]);
        rs.progress_bp.store(5000, Ordering::Relaxed);
        rs.set_pattern(1);
        check!(rs.current_pattern() == "b");
        check!(rs.progress_bp.load(Ordering::Relaxed) == 0);
    }

    #[test]
    fn record_error_increments_count() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        check!(rs.error_count.load(Ordering::Relaxed) == 0);
        rs.record_error();
        check!(rs.error_count.load(Ordering::Relaxed) == 1);
        rs.record_error();
        check!(rs.error_count.load(Ordering::Relaxed) == 2);
    }

    #[test]
    fn last_error_age_max_when_no_errors() {
        let rs = RegionState::new("r0".into(), 1024, vec!["a".into()]);
        check!(rs.last_error_age_secs() == f64::MAX);
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

    mod event_loop {
        use std::sync::Arc;
        use std::sync::atomic::Ordering;
        use std::sync::mpsc;

        use assert2::{assert, check};
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};
        use serial_test::serial;

        use super::super::*;
        use crate::shutdown;

        fn make_terminal(w: u16, h: u16) -> Terminal<TestBackend> {
            Terminal::with_options(
                TestBackend::new(w, h),
                TerminalOptions {
                    viewport: Viewport::Inline(h),
                },
            )
            .unwrap()
        }

        fn make_regions(n: usize, patterns: &[&str]) -> Vec<Arc<RegionState>> {
            let names: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
            (0..n)
                .map(|i| {
                    Arc::new(RegionState::new(
                        format!("r{i}"),
                        8 * 1024 * 1024,
                        names.clone(),
                    ))
                })
                .collect()
        }

        fn press(code: KeyCode) -> TuiEvent {
            TuiEvent::Key(KeyEvent::new(code, KeyModifiers::NONE))
        }

        fn press_modified(code: KeyCode, modifiers: KeyModifiers) -> TuiEvent {
            TuiEvent::Key(KeyEvent::new(code, modifiers))
        }

        fn release(code: KeyCode) -> TuiEvent {
            TuiEvent::Key(KeyEvent {
                code,
                modifiers: KeyModifiers::NONE,
                kind: KeyEventKind::Release,
                state: KeyEventState::NONE,
            })
        }

        fn make_error(region_idx: usize) -> TuiEvent {
            TuiEvent::Error(TuiError {
                region_idx,
                region_name: format!("r{region_idx}"),
                address: 0xdead_0000 + region_idx as u64,
                expected: 0xFF,
                actual: 0xFE,
                bit_position: 0,
                pattern: "solid".into(),
                progress_fraction: 0.5,
            })
        }

        fn buf_text(term: &Terminal<TestBackend>) -> String {
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol().chars().next().unwrap_or(' '))
                .collect()
        }

        fn config() -> TuiConfig {
            TuiConfig::default()
        }

        #[test]
        #[serial]
        fn exits_on_channel_disconnect() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            drop(tx);
            let regions = make_regions(2, &["solid"]);
            let mut term = make_terminal(80, 15);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Disconnected);
        }

        #[test]
        #[serial]
        fn exits_on_quit_key() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(2, &["solid", "walk"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Quit);
        }

        #[test]
        #[serial]
        fn exits_on_esc_key() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Esc)).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Quit);
        }

        #[test]
        #[serial]
        fn exits_on_ctrl_c() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press_modified(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Quit);
        }

        #[test]
        #[serial]
        fn exits_on_all_regions_done() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(3, &["solid"]);
            let mut term = make_terminal(80, 18);

            tx.send(TuiEvent::RegionDone(0)).unwrap();
            tx.send(TuiEvent::RegionDone(1)).unwrap();
            tx.send(TuiEvent::RegionDone(2)).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::AllComplete);
        }

        #[test]
        #[serial]
        fn region_done_partial_continues() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(2, &["solid"]);
            let mut term = make_terminal(80, 15);

            // Only 1 of 2 regions done — loop should continue until disconnect.
            tx.send(TuiEvent::RegionDone(0)).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Disconnected);
        }

        #[test]
        #[serial]
        fn key_release_ignored() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            let mut term = make_terminal(80, 15);

            // Release 'q' should not cause a quit.
            tx.send(release(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Disconnected);
        }

        #[test]
        #[serial]
        fn pause_toggles_region_state() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(2, &["solid"]);
            let mut term = make_terminal(80, 15);

            // Press 'p' to pause, then disconnect.
            tx.send(press(KeyCode::Char('p'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            for r in &regions {
                assert!(r.paused.load(Ordering::Relaxed));
            }
        }

        #[test]
        #[serial]
        fn unpause_toggles_back() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(2, &["solid"]);
            let mut term = make_terminal(80, 15);

            // Two presses: pause then unpause.
            tx.send(press(KeyCode::Char('p'))).unwrap();
            tx.send(press(KeyCode::Char('p'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            for r in &regions {
                assert!(!r.paused.load(Ordering::Relaxed));
            }
        }

        #[test]
        #[serial]
        fn verbose_toggle() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('v'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            assert!(result.verbose);
        }

        #[test]
        #[serial]
        fn verbose_double_toggle_off() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('v'))).unwrap();
            tx.send(press(KeyCode::Char('v'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            assert!(!result.verbose);
        }

        #[test]
        #[serial]
        fn error_events_collected() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(2, &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(make_error(0)).unwrap();
            tx.send(make_error(1)).unwrap();
            tx.send(make_error(0)).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.errors.len() == 3);
            check!(result.errors[0].region_idx == 0);
            check!(result.errors[1].region_idx == 1);
            check!(result.errors[2].region_idx == 0);
        }

        #[test]
        #[serial]
        fn progress_renders_correctly() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid", "walk"]);
            regions[0].progress_bp.store(5000, Ordering::Relaxed);
            let mut term = make_terminal(80, 15);

            // Single tick to trigger a render, then quit.
            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("50.0%"), "expected '50.0%' in: {text}");
        }

        #[test]
        #[serial]
        fn pattern_name_renders() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid", "walk"]);
            regions[0].set_pattern(1); // "walk"
            let mut term = make_terminal(80, 15);

            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("walk"), "expected 'walk' in: {text}");
        }

        #[test]
        #[serial]
        fn error_table_renders() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            regions[0].record_error();
            let mut term = make_terminal(120, 15);

            tx.send(make_error(0)).unwrap();
            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("r0"), "expected region name in error table");
            assert!(
                text.contains("solid"),
                "expected pattern name in error table"
            );
        }

        #[test]
        #[serial]
        fn header_shows_region_count() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(3, &["solid"]);
            let mut term = make_terminal(80, 18);

            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);
            assert!(
                text.contains("3 regions"),
                "expected '3 regions' in header: {text}"
            );
        }

        #[test]
        #[serial]
        fn header_shows_error_count() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            regions[0].error_count.store(7, Ordering::Relaxed);
            let mut term = make_terminal(80, 15);

            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);
            assert!(
                text.contains("7 errors"),
                "expected '7 errors' in header: {text}"
            );
        }

        #[test]
        #[serial]
        fn controls_bar_present() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let regions = make_regions(1, &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("ause"), "expected pause control");
            assert!(text.contains("uit"), "expected quit control");
        }

        #[test]
        #[serial]
        fn log_events_dont_corrupt_viewport() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(64);
            let regions = make_regions(2, &["solid", "walk"]);
            regions[0].progress_bp.store(5000, Ordering::Relaxed);
            regions[1].progress_bp.store(7500, Ordering::Relaxed);
            let mut term = make_terminal(100, 18);

            // Flood with log events interleaved with ticks.
            for i in 0..20 {
                tx.send(TuiEvent::Log(format!(
                    "2026-01-01 INFO region-0: test log line {i}"
                )))
                .unwrap();
                if i % 5 == 0 {
                    tx.send(TuiEvent::Tick).unwrap();
                }
            }
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            let text = buf_text(&term);

            // The viewport should contain the heatmap content, not log fragments.
            assert!(text.contains("ferrite"), "header should be present");
            assert!(
                text.contains("50.0%"),
                "region 0 progress should render cleanly"
            );
            assert!(
                text.contains("75.0%"),
                "region 1 progress should render cleanly"
            );
            // Log text should NOT appear in the rendered viewport buffer.
            assert!(
                !text.contains("test log line"),
                "log text leaked into viewport: {text}"
            );
        }

        #[test]
        #[serial]
        fn rapid_mixed_events() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(128);
            let regions = make_regions(2, &["solid"]);
            let mut term = make_terminal(100, 18);

            // Burst of mixed events.
            for i in 0..10 {
                tx.send(TuiEvent::Log(format!("log line {i}"))).unwrap();
                tx.send(make_error(i % 2)).unwrap();
                tx.send(TuiEvent::Tick).unwrap();
            }
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &regions, &rx).unwrap();
            check!(result.errors.len() == 10);

            let text = buf_text(&term);
            assert!(
                text.contains("ferrite"),
                "header should survive event burst"
            );
            assert!(
                !text.contains("log line"),
                "log text leaked into viewport after burst"
            );
        }
    }
}
