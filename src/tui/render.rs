use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Table};

use super::activity::ACTIVITY_CELLS;
use super::palette;
use super::{Segment, TuiFailure};

/// Symbol sets for fine-grained activity display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolSet {
    Block,
    Braille,
    Eighth,
    Shade,
    Ascii,
}

impl SymbolSet {
    const fn chars(self) -> &'static [char] {
        match self {
            Self::Block => &['░', '▒', '▓', '█'],
            Self::Braille => &['⠂', '⠆', '⠖', '⠶', '⡶', '⣶', '⣾', '⣿'],
            Self::Eighth => &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'],
            Self::Shade => &['·', '∘', '○', '●', '◉'],
            Self::Ascii => &['.', ':', '-', '=', '+', '*', '#', '@'],
        }
    }

    /// Pick the character for a brightness level 0.0..1.0.
    #[must_use]
    pub fn char_for(self, brightness: f64) -> char {
        let chars = self.chars();
        let idx = (brightness.clamp(0.0, 1.0) * (chars.len() - 1) as f64).round() as usize;
        chars[idx.min(chars.len() - 1)]
    }
}

/// Top-level renderer: draws all TUI sections into the frame.
pub fn render_heatmap(
    frame: &mut Frame,
    segment: &Segment,
    failures: &[TuiFailure],
    elapsed: Duration,
    verbose: bool,
    symbols: SymbolSet,
) {
    let area = frame.area();

    let constraints = vec![
        Constraint::Length(1), // header
        Constraint::Length(1), // memory map
        Constraint::Length(1), // labels
        Constraint::Length(1), // segment bar
        Constraint::Length(1), // separator
        Constraint::Min(3),    // failures
        Constraint::Length(1), // controls
    ];

    let chunks = ratatui::layout::Layout::vertical(constraints).split(area);

    render_header(frame, segment, elapsed, verbose, chunks[0]);
    render_memory_map(frame, segment, failures, chunks[1], symbols);
    render_memory_map_labels(frame, segment, chunks[2]);
    render_segment_bar(frame, segment, failures, chunks[3], symbols);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(chunks[4].width as usize),
            Style::default().fg(palette::SEPARATOR),
        ))),
        chunks[4],
    );

    render_failure_area(frame, failures, chunks[5]);
    render_controls(frame, chunks[6]);
}

fn render_header(
    frame: &mut Frame,
    segment: &Segment,
    elapsed: Duration,
    verbose: bool,
    area: ratatui::layout::Rect,
) {
    let total_failures = segment.failure_count();

    let mut spans = vec![
        Span::styled(
            " ferrite ",
            Style::default().fg(palette::HEADER_CYAN).bold(),
        ),
        Span::styled("│", Style::default().fg(palette::SEPARATOR)),
        Span::styled(
            format!(" {} ", segment.name),
            Style::default().fg(palette::TEXT),
        ),
        Span::styled("│", Style::default().fg(palette::SEPARATOR)),
        Span::styled(
            format!(" {:.1}s ", elapsed.as_secs_f64()),
            Style::default().fg(palette::TEXT),
        ),
    ];

    if total_failures > 0 {
        spans.push(Span::styled("│", Style::default().fg(palette::SEPARATOR)));
        spans.push(Span::styled(
            format!(" {total_failures} failures "),
            Style::default()
                .fg(palette::failure_severity(total_failures))
                .bold(),
        ));
    }

    if verbose {
        spans.push(Span::styled("│", Style::default().fg(palette::SEPARATOR)));
        spans.push(Span::styled(
            " VERBOSE ",
            Style::default().fg(palette::LOG_WARN),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Continuous memory map bar: bg=failure severity, fg=activity brightness.
fn render_memory_map(
    frame: &mut Frame,
    segment: &Segment,
    failures: &[TuiFailure],
    area: ratatui::layout::Rect,
    symbols: SymbolSet,
) {
    if area.width < 4 {
        return;
    }
    let usable_width = (area.width - 2) as usize;

    // Build per-column failure counts
    let mut col_failures: Vec<usize> = vec![0; usable_width];
    for failure in failures {
        let col = (failure.progress_fraction * (usable_width as f64 - 1.0)).round() as usize;
        if col < usable_width {
            col_failures[col] += 1;
        }
    }

    let failure_age = segment.last_failure_age_secs();

    let mut spans = vec![Span::raw(" ")];
    for c in 0..usable_width {
        let cell_frac = c as f64 / usable_width as f64;
        let cell_idx =
            (cell_frac * ACTIVITY_CELLS as f64).min(ACTIVITY_CELLS as f64 - 1.0) as usize;
        let brightness = segment.activity.brightness(cell_idx);

        let ch = symbols.char_for(brightness);
        let fg = palette::activity_color(brightness);

        let local_failures = col_failures.get(c).copied().unwrap_or(0);
        let bg = if local_failures > 0 {
            palette::failure_bg(local_failures, failure_age)
        } else {
            None
        };

        let mut style = Style::default().fg(fg);
        if let Some(bg_color) = bg {
            style = style.bg(bg_color);
        }
        spans.push(Span::styled(ch.to_string(), style));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_memory_map_labels(frame: &mut Frame, segment: &Segment, area: ratatui::layout::Rect) {
    if area.width < 4 {
        return;
    }
    let usable_width = (area.width - 2) as usize;
    let label_len = segment.name.len().min(usable_width);
    let offset = usable_width.saturating_sub(label_len) / 2;

    let mut label_chars: Vec<(char, Color)> = vec![(' ', palette::DIM); usable_width];
    for (j, ch) in segment.name.chars().take(label_len).enumerate() {
        let idx = offset + j;
        if idx < usable_width {
            label_chars[idx] = (ch, palette::DIM);
        }
    }

    let mut spans = vec![Span::raw(" ")];
    for (ch, color) in &label_chars {
        spans.push(Span::styled(ch.to_string(), Style::default().fg(*color)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_segment_bar(
    frame: &mut Frame,
    segment: &Segment,
    failures: &[TuiFailure],
    area: ratatui::layout::Rect,
    symbols: SymbolSet,
) {
    let pattern_name = segment.current_pattern();
    let pct = segment.progress_percent();
    let fails = segment.failure_count();
    let paused = segment.is_paused();

    let bar_chars = 20;
    let failure_fractions: Vec<f64> = failures.iter().map(|e| e.progress_fraction).collect();

    let failure_age = segment.last_failure_age_secs();

    let mut bar_spans: Vec<Span> = Vec::with_capacity(bar_chars);
    for c in 0..bar_chars {
        let col_frac = (c as f64 + 0.5) / bar_chars as f64;
        let cell_idx = (col_frac * ACTIVITY_CELLS as f64).min(ACTIVITY_CELLS as f64 - 1.0) as usize;
        let brightness = segment.activity.brightness(cell_idx);

        let col_frac_start = c as f64 / bar_chars as f64;
        let col_frac_end = (c + 1) as f64 / bar_chars as f64;
        let failures_here = failure_fractions
            .iter()
            .filter(|&&f| f >= col_frac_start && f < col_frac_end)
            .count();

        let ch = symbols.char_for(brightness);
        let fg = if failures_here > 0 {
            palette::failure_severity(failures_here)
        } else if paused {
            palette::PROGRESS_PAUSED
        } else {
            palette::activity_color(brightness)
        };

        let bg = if failures_here > 0 {
            palette::failure_bg(failures_here, failure_age)
        } else {
            None
        };

        let mut style = Style::default().fg(fg);
        if let Some(bg_color) = bg {
            style = style.bg(bg_color);
        }
        bar_spans.push(Span::styled(ch.to_string(), style));
    }

    let status = if paused { " ⏸" } else { "" };
    let fail_span = if fails > 0 {
        Span::styled(
            format!(" {fails}fail"),
            Style::default().fg(palette::failure_severity(fails)).bold(),
        )
    } else {
        Span::styled(" ok", Style::default().fg(palette::DIM))
    };

    let mut line_spans = vec![Span::styled(
        format!(" {:<10}", segment.name),
        Style::default().fg(palette::TEXT),
    )];
    line_spans.extend(bar_spans);
    line_spans.extend([
        Span::styled(
            format!(" {pct:>5.1}%"),
            Style::default().fg(palette::TEXT_BRIGHT),
        ),
        Span::styled(
            format!(" {pattern_name}"),
            Style::default().fg(palette::DIM),
        ),
        Span::raw(status),
        fail_span,
    ]);

    frame.render_widget(Paragraph::new(Line::from(line_spans)), area);
}

fn render_failure_area(frame: &mut Frame, failures: &[TuiFailure], area: ratatui::layout::Rect) {
    if failures.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no failures detected",
                Style::default().fg(palette::DIM),
            )))
            .block(Block::default().borders(Borders::NONE)),
            area,
        );
        return;
    }

    let header_row = Row::new(vec![
        "Segment", "Address", "Expected", "Actual", "Bit", "Pattern",
    ])
    .style(
        Style::default()
            .fg(palette::LOG_WARN)
            .add_modifier(Modifier::BOLD),
    );

    let severity = palette::failure_severity(failures.len());
    let rows: Vec<Row> = failures
        .iter()
        .rev()
        .take(area.height.saturating_sub(1) as usize)
        .map(|e| {
            Row::new(vec![
                e.segment_name.clone(),
                format!("{:#018x}", e.address),
                format!("{:#018x}", e.expected),
                format!("{:#018x}", e.actual),
                format!("{}", e.flipped_bits),
                e.pattern.clone(),
            ])
            .style(Style::default().fg(severity))
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(20),
        Constraint::Length(20),
        Constraint::Length(20),
        Constraint::Length(4),
        Constraint::Fill(1),
    ];

    frame.render_widget(Table::new(rows, widths).header(header_row), area);
}

fn render_controls(frame: &mut Frame, area: ratatui::layout::Rect) {
    let controls = Line::from(vec![
        Span::styled(" [p]", Style::default().fg(palette::HEADER_CYAN).bold()),
        Span::styled("ause ", Style::default().fg(palette::TEXT)),
        Span::styled("[v]", Style::default().fg(palette::HEADER_CYAN).bold()),
        Span::styled("erbose ", Style::default().fg(palette::TEXT)),
        Span::styled("[q]", Style::default().fg(palette::HEADER_CYAN).bold()),
        Span::styled("uit ", Style::default().fg(palette::TEXT)),
        Span::styled("[^C]", Style::default().fg(palette::HEADER_CYAN).bold()),
        Span::styled("exit", Style::default().fg(palette::TEXT)),
    ]);
    frame.render_widget(Paragraph::new(controls), area);
}

#[cfg(test)]
mod tests {
    use assert2::assert;

    use super::super::FlippedBits;
    use super::*;

    fn make_segment(name: &str, size_bytes: usize) -> Segment {
        Segment::new(
            name.to_string(),
            size_bytes,
            vec!["solid".to_string(), "walk".to_string()],
        )
    }

    mod symbol_set {
        use assert2::{assert, check};
        use rstest::rstest;

        use super::*;

        #[rstest]
        #[case(SymbolSet::Block)]
        #[case(SymbolSet::Braille)]
        #[case(SymbolSet::Eighth)]
        #[case(SymbolSet::Shade)]
        #[case(SymbolSet::Ascii)]
        fn char_for_zero_returns_first(#[case] set: SymbolSet) {
            let ch = set.char_for(0.0);
            check!(
                ch == set.chars()[0],
                "{set:?} char_for(0.0) should be first char"
            );
        }

        #[rstest]
        #[case(SymbolSet::Block)]
        #[case(SymbolSet::Braille)]
        #[case(SymbolSet::Eighth)]
        #[case(SymbolSet::Shade)]
        #[case(SymbolSet::Ascii)]
        fn char_for_one_returns_last(#[case] set: SymbolSet) {
            let ch = set.char_for(1.0);
            let chars = set.chars();
            check!(
                ch == chars[chars.len() - 1],
                "{set:?} char_for(1.0) should be last char"
            );
        }

        #[test]
        fn char_for_clamps_above_one() {
            let ch = SymbolSet::Ascii.char_for(5.0);
            check!(ch == '@'); // last ASCII char
        }

        #[test]
        fn char_for_clamps_below_zero() {
            let ch = SymbolSet::Ascii.char_for(-1.0);
            check!(ch == '.'); // first ASCII char
        }

        #[test]
        fn char_for_midpoint() {
            let ch = SymbolSet::Ascii.char_for(0.5);
            let chars = SymbolSet::Ascii.chars();
            // 0.5 * 7 = 3.5, rounds to 4 -> '+'
            check!(ch == chars[4]);
        }

        #[rstest]
        #[case(SymbolSet::Block)]
        #[case(SymbolSet::Braille)]
        #[case(SymbolSet::Eighth)]
        #[case(SymbolSet::Shade)]
        #[case(SymbolSet::Ascii)]
        fn all_have_nonempty_chars(#[case] set: SymbolSet) {
            assert!(!set.chars().is_empty());
        }

        #[test]
        fn equality() {
            check!(SymbolSet::Braille == SymbolSet::Braille);
            check!(SymbolSet::Block != SymbolSet::Ascii);
        }

        #[test]
        fn clone() {
            let s = SymbolSet::Shade;
            let s2 = s;
            check!(s == s2);
        }
    }

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn test_terminal(w: u16, h: u16) -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(w, h)).unwrap()
    }

    fn buf_text(term: &Terminal<TestBackend>) -> String {
        term.backend()
            .buffer()
            .content()
            .iter()
            .map(|c: &ratatui::buffer::Cell| c.symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn render_header_no_failures_no_verbose() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("r0", 1024);
        let elapsed = Duration::from_secs_f64(1.5);
        term.draw(|frame| {
            render_header(frame, &segment, elapsed, false, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("ferrite"), "header should contain 'ferrite'");
        assert!(text.contains("r0"), "header should show segment name");
        assert!(text.contains("1.5s"), "header should show elapsed time");
        assert!(!text.contains("VERBOSE"));
    }

    #[test]
    fn render_header_with_failures() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("r0", 1024);
        for _ in 0..5 {
            segment.record_failure();
        }
        let elapsed = Duration::from_secs(10);
        term.draw(|frame| {
            render_header(frame, &segment, elapsed, false, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(
            text.contains("5 failures"),
            "header should show failure count"
        );
    }

    #[test]
    fn render_header_verbose_mode() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("r0", 1024);
        let elapsed = Duration::from_secs(0);
        term.draw(|frame| {
            render_header(frame, &segment, elapsed, true, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("VERBOSE"));
    }

    #[test]
    fn render_memory_map_narrow_width_returns_early() {
        let mut term = test_terminal(3, 1);
        let segment = make_segment("r0", 1024);
        let failures: Vec<TuiFailure> = vec![];
        // Should not panic on very narrow width
        term.draw(|frame| {
            render_memory_map(frame, &segment, &failures, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_with_activity() {
        let mut term = test_terminal(40, 1);
        let segment = make_segment("r0", 1024);
        segment.activity.touch(0.5);
        let failures: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_memory_map(frame, &segment, &failures, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_with_failures() {
        let mut term = test_terminal(40, 1);
        let segment = make_segment("r0", 1024);
        let failures = vec![TuiFailure {
            segment_name: "r0".into(),
            address: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            flipped_bits: FlippedBits::Single(0),
            pattern: "solid".into(),
            progress_fraction: 0.5,
        }];
        segment.record_failure();
        term.draw(|frame| {
            render_memory_map(frame, &segment, &failures, frame.area(), SymbolSet::Braille);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_labels_narrow_returns_early() {
        let mut term = test_terminal(3, 1);
        let segment = make_segment("r0", 1024);
        term.draw(|frame| {
            render_memory_map_labels(frame, &segment, frame.area());
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_labels_shows_segment_name() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("64.0 MiB", 64 * 1024 * 1024);
        term.draw(|frame| {
            render_memory_map_labels(frame, &segment, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(
            text.contains("64.0 MiB"),
            "labels should show segment name, got: '{text}'"
        );
    }

    #[test]
    fn render_segment_bar_shows_pattern_and_progress() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("r0", 1024);
        segment.set_progress(1, 2);
        let failures: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_segment_bar(frame, &segment, &failures, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("r0"), "should show segment name");
        assert!(text.contains("50.0%"), "should show progress percentage");
        assert!(text.contains("solid"), "should show pattern name");
        assert!(text.contains("ok"), "should show ok for no failures");
    }

    #[test]
    fn render_segment_bar_shows_failures() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("r0", 1024);
        for _ in 0..3 {
            segment.record_failure();
        }
        let failures: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_segment_bar(frame, &segment, &failures, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("3fail"), "should show failure count");
    }

    #[test]
    fn render_segment_bar_paused() {
        let mut term = test_terminal(80, 1);
        let segment = make_segment("r0", 1024);
        segment.set_paused(true);
        let failures: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_segment_bar(frame, &segment, &failures, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("⏸"), "should show pause indicator");
    }

    #[test]
    fn render_failure_area_empty() {
        let mut term = test_terminal(80, 3);
        let failures: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_failure_area(frame, &failures, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("no failures detected"));
    }

    #[test]
    fn render_failure_area_with_failures() {
        let mut term = test_terminal(120, 5);
        let failures = vec![
            TuiFailure {
                segment_name: "r0".into(),
                address: 0xdead,
                expected: 0xFF,
                actual: 0xFE,
                flipped_bits: FlippedBits::Single(0),
                pattern: "solid".into(),
                progress_fraction: 0.1,
            },
            TuiFailure {
                segment_name: "r0".into(),
                address: 0xbeef,
                expected: 0xAA,
                actual: 0xBB,
                flipped_bits: FlippedBits::Single(4),
                pattern: "walk".into(),
                progress_fraction: 0.5,
            },
        ];
        term.draw(|frame| {
            render_failure_area(frame, &failures, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("Segment"), "should have table header");
        assert!(
            text.contains("r0"),
            "should show segment name in failure rows"
        );
    }

    #[test]
    fn render_controls_shows_keybindings() {
        let mut term = test_terminal(80, 1);
        term.draw(|frame| {
            render_controls(frame, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("ause"), "should show pause control");
        assert!(text.contains("erbose"), "should show verbose control");
        assert!(text.contains("uit"), "should show quit control");
    }

    #[test]
    fn render_heatmap_full_layout() {
        let mut term = test_terminal(80, 15);
        let segment = make_segment("r0", 1024);
        segment.set_progress(3, 10);
        let failures: Vec<TuiFailure> = vec![];
        let elapsed = Duration::from_secs(5);
        term.draw(|frame| {
            render_heatmap(frame, &segment, &failures, elapsed, false, SymbolSet::Ascii);
        })
        .unwrap();
        // Should not panic -- layout fits all sections
    }

    #[test]
    fn render_heatmap_with_failures_full() {
        let mut term = test_terminal(80, 15);
        let segment = make_segment("r0", 1024);
        for _ in 0..3 {
            segment.record_failure();
        }
        let failures = vec![TuiFailure {
            segment_name: "r0".into(),
            address: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            flipped_bits: FlippedBits::Single(0),
            pattern: "solid".into(),
            progress_fraction: 0.3,
        }];
        let elapsed = Duration::from_secs(2);
        term.draw(|frame| {
            render_heatmap(
                frame,
                &segment,
                &failures,
                elapsed,
                true,
                SymbolSet::Braille,
            );
        })
        .unwrap();
    }
}
