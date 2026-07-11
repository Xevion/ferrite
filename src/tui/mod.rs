//! The inline TUI: a ratatui-based live view over stdout, active when `--tui auto` detects a
//! terminal. [`bridge`] hot-swaps tracing's writer to route log lines into the TUI channel.
#![cfg_attr(coverage_nightly, coverage(off))]

pub mod activity;
pub mod bridge;
pub mod event;
pub mod palette;
pub mod render;
pub mod run;
pub mod segment;
pub mod trace;

pub use activity::ActivityBuffer;
pub use event::{FlippedBits, TuiEvent, TuiFailure, TuiLoopResult, TuiOutcome};
pub use render::SymbolSet;
pub use segment::Segment;
pub use trace::{TuiMakeWriter, TuiTraceGuard, TuiTraceState, TuiWriter};

use std::collections::VecDeque;
use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, poll, read};
use ratatui::backend::Backend;
use ratatui::prelude::Widget;
use ratatui::widgets::Paragraph;
use ratatui::{Terminal, TerminalOptions, Viewport};
use snafu::{ResultExt, Whatever};
use tracing::info;

use render::render_heatmap;

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

/// Core event loop: processes events from `rx`, renders to `terminal`.
///
/// Generic over the backend so tests can use `TestBackend`. Returns a
/// [`TuiLoopResult`] with the exit reason, collected failures, and verbose state.
///
/// # Errors
///
/// Returns an error if drawing to the terminal fails.
pub fn run_event_loop<B>(
    terminal: &mut Terminal<B>,
    config: &TuiConfig,
    segment: &Segment,
    rx: &mpsc::Receiver<TuiEvent>,
) -> Result<TuiLoopResult, Whatever>
where
    B: ratatui::backend::Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let start_time = Instant::now();
    let mut failures: Vec<TuiFailure> = Vec::new();
    // Pending log lines, drained once per tick to bound insert_before calls.
    let mut log_buf: VecDeque<ratatui::text::Text<'static>> = VecDeque::with_capacity(32);
    let mut verbose = false;

    // Establish the viewport before processing any events -- insert_before
    // misbehaves if called before the first draw.
    terminal
        .draw(|frame| {
            render_heatmap(
                frame,
                segment,
                &failures,
                start_time.elapsed(),
                verbose,
                config.symbols,
            );
        })
        .whatever_context("failed to draw initial frame")?;

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
                        let paused = !segment.is_paused();
                        segment.set_paused(paused);
                        if paused {
                            info!("paused segment");
                        } else {
                            info!("resumed segment");
                        }
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
            Ok(TuiEvent::Failure(failure)) => {
                failures.push(failure);
            }
            Ok(TuiEvent::Done) => {
                info!(segment = segment.name.as_str(), "segment complete");
                crate::shutdown::request_quit(crate::shutdown::QuitReason::UserQuit);
                break TuiOutcome::AllComplete;
            }
            Ok(TuiEvent::Tick) | Err(mpsc::RecvTimeoutError::Timeout) => {
                if !log_buf.is_empty() {
                    let lines = Vec::from(std::mem::take(&mut log_buf));
                    terminal
                        .insert_before(lines.len() as u16, |buf| {
                            for (i, text) in lines.into_iter().enumerate() {
                                let area = ratatui::layout::Rect {
                                    y: buf.area.y + i as u16,
                                    height: 1,
                                    ..buf.area
                                };
                                Paragraph::new(text).render(area, buf);
                            }
                        })
                        .whatever_context("failed to insert log lines")?;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break TuiOutcome::Disconnected,
        }

        let elapsed = start_time.elapsed();
        terminal
            .draw(|frame| {
                render_heatmap(frame, segment, &failures, elapsed, verbose, config.symbols);
            })
            .whatever_context("failed to draw frame")?;
    };

    // Final render to capture end state.
    let elapsed = start_time.elapsed();
    terminal
        .draw(|frame| {
            render_heatmap(frame, segment, &failures, elapsed, verbose, config.symbols);
        })
        .whatever_context("failed to draw final frame")?;

    Ok(TuiLoopResult {
        outcome,
        failures,
        verbose,
    })
}

/// Finalize the inline viewport on exit.
///
/// First drains any log lines still buffered in `rx` into the scrollback, in
/// arrival order, so late diagnostics read continuously with the lines already
/// shown during the run instead of being dumped after teardown. Then collapses
/// the viewport: the region is cleared and the cursor is parked at its
/// top-left, so the post-run summary begins exactly where the viewport was —
/// with no dead whitespace. Finally restores cursor visibility for the shell.
///
/// Generic over the backend so tests can drive it with `TestBackend`.
///
/// # Errors
///
/// Returns an error if inserting scrollback lines, clearing, or repositioning
/// the cursor fails.
pub fn finish_viewport<B>(
    terminal: &mut Terminal<B>,
    rx: &mpsc::Receiver<TuiEvent>,
) -> Result<(), Whatever>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    // Push any log lines still queued at exit into the scrollback, in order,
    // so they flow directly below the output already shown during the run.
    let lines: Vec<ratatui::text::Text<'static>> = rx
        .try_iter()
        .filter_map(|event| match event {
            TuiEvent::Log(msg) => ansi_to_tui::IntoText::into_text(&msg).ok(),
            _ => None,
        })
        .collect();
    if !lines.is_empty() {
        terminal
            .insert_before(lines.len() as u16, |buf| {
                for (i, text) in lines.into_iter().enumerate() {
                    let area = ratatui::layout::Rect {
                        y: buf.area.y + i as u16,
                        height: 1,
                        ..buf.area
                    };
                    Paragraph::new(text).render(area, buf);
                }
            })
            .whatever_context("failed to insert scrollback lines")?;
    }

    // Collapse the viewport: clear it, then park the cursor at its top-left so
    // the summary printed afterwards starts exactly where the viewport was.
    // `clear()` alone restores the pre-clear cursor (deep in the viewport),
    // which is what left the dead whitespace; the explicit reposition fixes it.
    let top = terminal.get_frame().area().as_position();
    terminal
        .clear()
        .whatever_context("failed to clear viewport")?;
    terminal
        .set_cursor_position(top)
        .whatever_context("failed to reposition cursor")?;
    terminal
        .show_cursor()
        .whatever_context("failed to show cursor")?;
    terminal
        .backend_mut()
        .flush()
        .whatever_context("failed to flush terminal")?;
    Ok(())
}

/// Run the TUI event loop with a real terminal. Blocks until the user quits
/// or the segment completes.
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
    segment: &Segment,
    tx: &mpsc::SyncSender<TuiEvent>,
    rx: &mpsc::Receiver<TuiEvent>,
) -> Result<(), Whatever> {
    let guard = crate::shutdown::TerminalGuard::new()?;

    // Header + memory map + labels + segment row + separator + failures (min 3) + controls.
    let viewport_height: u16 = 9;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(viewport_height),
        },
    )
    .whatever_context("failed to initialize terminal")?;

    // Input reader thread
    let input_tx = tx.clone();
    thread::Builder::new()
        .name("tui-input".into())
        .spawn(move || {
            while !crate::shutdown::quit_requested() {
                if poll(Duration::from_millis(50)).unwrap_or(false)
                    && let Ok(Event::Key(key)) = read()
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

    run_event_loop(&mut terminal, config, segment, rx)?;

    // Drain trailing diagnostics into the scrollback and collapse the viewport
    // (while raw mode is still active) so the summary starts cleanly where the
    // viewport was, with no dead whitespace.
    finish_viewport(&mut terminal, rx)?;
    // Explicitly drop the guard before println so the terminal is restored first.
    drop(guard);
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    #[test]
    fn tui_config_default_uses_braille() {
        let config = TuiConfig::default();
        check!(config.symbols == SymbolSet::Braille);
    }

    mod event_loop {
        use std::sync::Arc;
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

        fn make_segment(name: &str, patterns: &[&str]) -> Arc<Segment> {
            let names: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
            Arc::new(Segment::new(name.to_string(), 8 * 1024 * 1024, names))
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

        fn make_failure() -> TuiEvent {
            TuiEvent::Failure(TuiFailure {
                segment_name: "r0".into(),
                address: 0xdead_0000,
                expected: 0xFF,
                actual: 0xFE,
                flipped_bits: FlippedBits::Single(0),
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
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Disconnected);
        }

        #[test]
        #[serial]
        fn exits_on_quit_key() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid", "walk"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Quit);
        }

        #[test]
        #[serial]
        fn exits_on_esc_key() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Esc)).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Quit);
        }

        #[test]
        #[serial]
        fn exits_on_ctrl_c() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press_modified(KeyCode::Char('c'), KeyModifiers::CONTROL))
                .unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Quit);
        }

        #[test]
        #[serial]
        fn exits_on_segment_done() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 18);

            tx.send(TuiEvent::Done).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.outcome == TuiOutcome::AllComplete);
        }

        #[test]
        #[serial]
        fn key_release_ignored() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            // Release 'q' should not cause a quit.
            tx.send(release(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.outcome == TuiOutcome::Disconnected);
        }

        #[test]
        #[serial]
        fn pause_toggles_segment_state() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            // Press 'p' to pause, then disconnect.
            tx.send(press(KeyCode::Char('p'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            assert!(segment.is_paused());
        }

        #[test]
        #[serial]
        fn unpause_toggles_back() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            // Two presses: pause then unpause.
            tx.send(press(KeyCode::Char('p'))).unwrap();
            tx.send(press(KeyCode::Char('p'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            assert!(!segment.is_paused());
        }

        #[test]
        #[serial]
        fn verbose_toggle() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('v'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            assert!(result.verbose);
        }

        #[test]
        #[serial]
        fn verbose_double_toggle_off() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('v'))).unwrap();
            tx.send(press(KeyCode::Char('v'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            assert!(!result.verbose);
        }

        #[test]
        #[serial]
        fn failure_events_collected() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(make_failure()).unwrap();
            tx.send(make_failure()).unwrap();
            tx.send(make_failure()).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.failures.len() == 3);
        }

        #[test]
        #[serial]
        fn progress_renders_correctly() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid", "walk"]);
            segment.set_progress(1, 2);
            let mut term = make_terminal(80, 15);

            // Single tick to trigger a render, then quit.
            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("50.0%"), "expected '50.0%' in: {text}");
        }

        #[test]
        #[serial]
        fn pattern_name_renders() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid", "walk"]);
            segment.set_pattern(1); // "walk"
            let mut term = make_terminal(80, 15);

            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("walk"), "expected 'walk' in: {text}");
        }

        #[test]
        #[serial]
        fn failure_table_renders() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            segment.record_failure();
            let mut term = make_terminal(120, 15);

            tx.send(make_failure()).unwrap();
            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);
            assert!(
                text.contains("r0"),
                "expected segment name in failure table"
            );
            assert!(
                text.contains("solid"),
                "expected pattern name in failure table"
            );
        }

        #[test]
        #[serial]
        fn header_shows_segment_name() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("4.0 GiB", &["solid"]);
            let mut term = make_terminal(80, 18);

            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);
            assert!(
                text.contains("4.0 GiB"),
                "expected segment name in header: {text}"
            );
        }

        #[test]
        #[serial]
        fn header_shows_failure_count() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            for _ in 0..7 {
                segment.record_failure();
            }
            let mut term = make_terminal(80, 15);

            tx.send(TuiEvent::Tick).unwrap();
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);
            assert!(
                text.contains("7 failures"),
                "expected '7 failures' in header: {text}"
            );
        }

        #[test]
        #[serial]
        fn controls_bar_present() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(80, 15);

            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);
            assert!(text.contains("ause"), "expected pause control");
            assert!(text.contains("uit"), "expected quit control");
        }

        #[test]
        #[serial]
        fn log_events_dont_corrupt_viewport() {
            shutdown::reset();
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(64);
            let segment = make_segment("r0", &["solid", "walk"]);
            segment.set_progress(1, 2);
            let mut term = make_terminal(100, 18);

            // Flood with log events interleaved with ticks.
            for i in 0..20 {
                tx.send(TuiEvent::Log(format!(
                    "2026-01-01 INFO segment: test log line {i}"
                )))
                .unwrap();
                if i % 5 == 0 {
                    tx.send(TuiEvent::Tick).unwrap();
                }
            }
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let _ = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            let text = buf_text(&term);

            // The viewport should contain the heatmap content, not log fragments.
            assert!(text.contains("ferrite"), "header should be present");
            assert!(
                text.contains("50.0%"),
                "segment progress should render cleanly"
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
            let segment = make_segment("r0", &["solid"]);
            let mut term = make_terminal(100, 18);

            // Burst of mixed events.
            for i in 0..10 {
                tx.send(TuiEvent::Log(format!("log line {i}"))).unwrap();
                tx.send(make_failure()).unwrap();
                tx.send(TuiEvent::Tick).unwrap();
            }
            tx.send(press(KeyCode::Char('q'))).unwrap();
            drop(tx);

            let result = run_event_loop(&mut term, &config(), &segment, &rx).unwrap();
            check!(result.failures.len() == 10);

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

    mod teardown {
        use std::sync::mpsc;

        use assert2::{assert, check};
        use ratatui::backend::{Backend, TestBackend};
        use ratatui::layout::Position;
        use ratatui::style::Style;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        use super::super::*;

        fn inline_terminal(
            w: u16,
            h: u16,
            viewport_h: u16,
            cursor_row: u16,
        ) -> Terminal<TestBackend> {
            let mut backend = TestBackend::new(w, h);
            backend
                .set_cursor_position(Position {
                    x: 0,
                    y: cursor_row,
                })
                .unwrap();
            Terminal::with_options(
                backend,
                TerminalOptions {
                    viewport: Viewport::Inline(viewport_h),
                },
            )
            .unwrap()
        }

        fn fill_viewport(term: &mut Terminal<TestBackend>, marker: &str) {
            term.draw(|frame| {
                let area = frame.area();
                for y in area.top()..area.bottom() {
                    frame
                        .buffer_mut()
                        .set_string(area.x, y, marker, Style::default());
                }
            })
            .unwrap();
        }

        fn visible_text(term: &Terminal<TestBackend>) -> String {
            term.backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol().chars().next().unwrap_or(' '))
                .collect()
        }

        /// Scrollback followed by the visible buffer, flattened to a string.
        fn all_text(term: &Terminal<TestBackend>) -> String {
            let scroll = term.backend().scrollback().content().iter();
            let visible = term.backend().buffer().content().iter();
            scroll
                .chain(visible)
                .map(|c| c.symbol().chars().next().unwrap_or(' '))
                .collect()
        }

        #[test]
        fn parks_cursor_at_viewport_top() {
            // Viewport anchored at row 3; after a draw the backend cursor sits
            // deep inside it. Teardown must return the cursor to the top row so
            // the summary starts exactly where the viewport was.
            let mut term = inline_terminal(20, 15, 9, 3);
            fill_viewport(&mut term, "HEATMAPXX");
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(8);
            drop(tx);

            finish_viewport(&mut term, &rx).unwrap();

            let pos = term.get_cursor_position().unwrap();
            check!(pos.x == 0);
            check!(pos.y == 3);
        }

        #[test]
        fn clears_viewport_content() {
            let mut term = inline_terminal(20, 15, 9, 3);
            fill_viewport(&mut term, "HEATMAPXX");
            check!(visible_text(&term).contains("HEATMAPXX"));
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(8);
            drop(tx);

            finish_viewport(&mut term, &rx).unwrap();

            check!(!visible_text(&term).contains("HEATMAPXX"));
        }

        #[test]
        fn restores_cursor_visibility() {
            let mut term = inline_terminal(20, 15, 9, 3);
            fill_viewport(&mut term, "x");
            // draw() hides the cursor; teardown must restore it for the shell.
            check!(!term.backend().cursor_visible());
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(8);
            drop(tx);

            finish_viewport(&mut term, &rx).unwrap();

            check!(term.backend().cursor_visible());
        }

        #[test]
        fn flushes_buffered_logs_to_scrollback() {
            // Logs still queued at exit must land in the scrollback (in order),
            // not be dumped after the viewport is gone.
            let mut term = inline_terminal(40, 20, 5, 2);
            fill_viewport(&mut term, "heat");
            let (tx, rx) = mpsc::sync_channel::<TuiEvent>(16);
            tx.send(TuiEvent::Log(" INFO ferrite: tail ALPHA".into()))
                .unwrap();
            tx.send(TuiEvent::Log(" INFO ferrite: tail BETA".into()))
                .unwrap();
            drop(tx);

            finish_viewport(&mut term, &rx).unwrap();

            let text = all_text(&term);
            assert!(text.contains("ALPHA"), "tail log ALPHA missing: {text}");
            assert!(text.contains("BETA"), "tail log BETA missing: {text}");
        }
    }
}
