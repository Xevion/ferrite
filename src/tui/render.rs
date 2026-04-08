use std::sync::Arc;
use std::sync::atomic::Ordering;
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SymbolSet {
    Block,
    Braille,
    Eighth,
    Shade,
    Ascii,
}

impl SymbolSet {
    fn chars(self) -> &'static [char] {
        match self {
            SymbolSet::Block => &['░', '▒', '▓', '█'],
            SymbolSet::Braille => &['⠂', '⠆', '⠖', '⠶', '⡶', '⣶', '⣾', '⣿'],
            SymbolSet::Eighth => &['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'],
            SymbolSet::Shade => &['·', '∘', '○', '●', '◉'],
            SymbolSet::Ascii => &['.', ':', '-', '=', '+', '*', '#', '@'],
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

/// Compute region-to-column mappings for the memory map display.
fn region_columns(regions: &[Arc<Segment>], usable_width: usize) -> Vec<(usize, usize)> {
    let total_bytes: usize = regions.iter().map(|r| r.size_bytes).sum();
    if total_bytes == 0 {
        return vec![(0, 1); regions.len()];
    }
    let mut result = Vec::with_capacity(regions.len());
    let mut col = 0;
    for (i, region) in regions.iter().enumerate() {
        let w = if i == regions.len() - 1 {
            usable_width - col
        } else {
            ((region.size_bytes as f64 / total_bytes as f64) * usable_width as f64).round() as usize
        }
        .max(1);
        result.push((col, w));
        col += w;
    }
    result
}

/// Top-level renderer: draws all TUI sections into the frame.
pub fn render_heatmap(
    frame: &mut Frame,
    regions: &[Arc<Segment>],
    errors: &[TuiFailure],
    elapsed: Duration,
    verbose: bool,
    symbols: SymbolSet,
) {
    let area = frame.area();

    let mut constraints = vec![
        Constraint::Length(1), // header
        Constraint::Length(1), // memory map
        Constraint::Length(1), // labels
    ];
    for _ in regions {
        constraints.push(Constraint::Length(1));
    }
    constraints.push(Constraint::Length(1)); // separator
    constraints.push(Constraint::Min(3)); // errors
    constraints.push(Constraint::Length(1)); // controls

    let chunks = ratatui::layout::Layout::vertical(constraints).split(area);

    render_header(frame, regions, elapsed, verbose, chunks[0]);
    render_memory_map(frame, regions, errors, chunks[1], symbols);
    render_memory_map_labels(frame, regions, chunks[2]);

    for (i, region) in regions.iter().enumerate() {
        render_heatmap_region(frame, region, errors, i, chunks[i + 3], symbols);
    }

    let sep_idx = regions.len() + 3;

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(chunks[sep_idx].width as usize),
            Style::default().fg(palette::SEPARATOR),
        ))),
        chunks[sep_idx],
    );

    render_error_area(frame, errors, chunks[sep_idx + 1]);
    render_controls(frame, chunks[sep_idx + 2]);
}

fn render_header(
    frame: &mut Frame,
    regions: &[Arc<Segment>],
    elapsed: Duration,
    verbose: bool,
    area: ratatui::layout::Rect,
) {
    let total_failures: usize = regions
        .iter()
        .map(|r| r.failure_count.load(Ordering::Relaxed))
        .sum();

    let mut spans = vec![
        Span::styled(
            " ferrite ",
            Style::default().fg(palette::HEADER_CYAN).bold(),
        ),
        Span::styled("│", Style::default().fg(palette::SEPARATOR)),
        Span::styled(
            format!(" {} regions ", regions.len()),
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
                .fg(palette::error_severity(total_failures))
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

/// Continuous memory map bar: bg=error severity, fg=activity brightness.
fn render_memory_map(
    frame: &mut Frame,
    regions: &[Arc<Segment>],
    errors: &[TuiFailure],
    area: ratatui::layout::Rect,
    symbols: SymbolSet,
) {
    if area.width < 4 {
        return;
    }
    let usable_width = (area.width - 2) as usize;
    let boundaries = region_columns(regions, usable_width);

    // Build per-column error counts
    let mut col_errors: Vec<usize> = vec![0; usable_width];
    for err in errors {
        if err.region_idx < boundaries.len() {
            let (start, width) = boundaries[err.region_idx];
            let err_col = start + (err.progress_fraction * (width as f64 - 1.0)).round() as usize;
            if err_col < usable_width {
                col_errors[err_col] += 1;
            }
        }
    }

    let mut spans = vec![Span::raw(" ")];
    for (i, region) in regions.iter().enumerate() {
        let (start, width) = boundaries[i];
        let err_age = region.last_error_age_secs();

        for c in 0..width {
            let col_idx = start + c;
            let cell_frac = c as f64 / width as f64;
            let cell_idx =
                (cell_frac * ACTIVITY_CELLS as f64).min(ACTIVITY_CELLS as f64 - 1.0) as usize;
            let brightness = region.activity.brightness(cell_idx);

            let ch = symbols.char_for(brightness);
            let fg = palette::activity_color(brightness);

            let local_errs = col_errors.get(col_idx).copied().unwrap_or(0);
            let bg = if local_errs > 0 {
                palette::error_bg(local_errs, err_age)
            } else {
                None
            };

            let mut style = Style::default().fg(fg);
            if let Some(bg_color) = bg {
                style = style.bg(bg_color);
            }
            spans.push(Span::styled(ch.to_string(), style));
        }
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_memory_map_labels(
    frame: &mut Frame,
    regions: &[Arc<Segment>],
    area: ratatui::layout::Rect,
) {
    if area.width < 4 {
        return;
    }
    let usable_width = (area.width - 2) as usize;
    let boundaries = region_columns(regions, usable_width);
    let mut label_chars: Vec<(char, Color)> = vec![(' ', palette::DIM); usable_width];

    for (i, region) in regions.iter().enumerate() {
        let (col, w) = boundaries[i];
        let mb = region.size_bytes / (1024 * 1024);
        let label = format!("r{i}:{mb}M");
        let label_len = label.len().min(w);
        let offset = w.saturating_sub(label_len) / 2;
        for (j, ch) in label.chars().take(label_len).enumerate() {
            let idx = col + offset + j;
            if idx < usable_width {
                label_chars[idx] = (ch, palette::DIM);
            }
        }
    }

    let mut spans = vec![Span::raw(" ")];
    for (ch, color) in &label_chars {
        spans.push(Span::styled(ch.to_string(), Style::default().fg(*color)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_heatmap_region(
    frame: &mut Frame,
    region: &Segment,
    errors: &[TuiFailure],
    region_idx: usize,
    area: ratatui::layout::Rect,
    symbols: SymbolSet,
) {
    let pattern_name = region.current_pattern();
    let progress_bp = region.progress_bp.load(Ordering::Relaxed);
    let pct = progress_bp as f64 / 100.0;
    let errs = region.failure_count.load(Ordering::Relaxed);
    let paused = region.paused.load(Ordering::Relaxed);

    let bar_chars = 20;
    let region_errors: Vec<f64> = errors
        .iter()
        .filter(|e| e.region_idx == region_idx)
        .map(|e| e.progress_fraction)
        .collect();

    let err_age = region.last_error_age_secs();

    let mut bar_spans: Vec<Span> = Vec::with_capacity(bar_chars);
    for c in 0..bar_chars {
        let col_frac = (c as f64 + 0.5) / bar_chars as f64;
        let cell_idx = (col_frac * ACTIVITY_CELLS as f64).min(ACTIVITY_CELLS as f64 - 1.0) as usize;
        let brightness = region.activity.brightness(cell_idx);

        let col_frac_start = c as f64 / bar_chars as f64;
        let col_frac_end = (c + 1) as f64 / bar_chars as f64;
        let errors_here = region_errors
            .iter()
            .filter(|&&f| f >= col_frac_start && f < col_frac_end)
            .count();

        let ch = symbols.char_for(brightness);
        let fg = if errors_here > 0 {
            palette::error_severity(errors_here)
        } else if paused {
            palette::PROGRESS_PAUSED
        } else {
            palette::activity_color(brightness)
        };

        let bg = if errors_here > 0 {
            palette::error_bg(errors_here, err_age)
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
    let err_span = if errs > 0 {
        Span::styled(
            format!(" {errs}err"),
            Style::default().fg(palette::error_severity(errs)).bold(),
        )
    } else {
        Span::styled(" ok", Style::default().fg(palette::DIM))
    };

    let mut line_spans = vec![Span::styled(
        format!(" {:<10}", region.name),
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
        err_span,
    ]);

    frame.render_widget(Paragraph::new(Line::from(line_spans)), area);
}

fn render_error_area(frame: &mut Frame, errors: &[TuiFailure], area: ratatui::layout::Rect) {
    if errors.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no errors detected",
                Style::default().fg(palette::DIM),
            )))
            .block(Block::default().borders(Borders::NONE)),
            area,
        );
        return;
    }

    let header_row = Row::new(vec![
        "Region", "Address", "Expected", "Actual", "Bit", "Pattern",
    ])
    .style(
        Style::default()
            .fg(palette::LOG_WARN)
            .add_modifier(Modifier::BOLD),
    );

    let rows: Vec<Row> = errors
        .iter()
        .rev()
        .take(area.height.saturating_sub(1) as usize)
        .map(|e| {
            let region_errs = errors
                .iter()
                .filter(|o| o.region_idx == e.region_idx)
                .count();
            Row::new(vec![
                e.region_name.clone(),
                format!("{:#018x}", e.address),
                format!("{:#018x}", e.expected),
                format!("{:#018x}", e.actual),
                format!("{}", e.flipped_bits),
                e.pattern.clone(),
            ])
            .style(Style::default().fg(palette::error_severity(region_errs)))
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
        Span::styled("[s]", Style::default().fg(palette::HEADER_CYAN).bold()),
        Span::styled("kip ", Style::default().fg(palette::TEXT)),
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
    use assert2::{assert, check};

    use super::super::FlippedBits;
    use super::*;

    fn make_region(name: &str, size_bytes: usize) -> Arc<Segment> {
        Arc::new(Segment::new(
            name.to_string(),
            size_bytes,
            vec!["solid".to_string(), "walk".to_string()],
        ))
    }

    #[test]
    fn symbol_set_char_for_zero_returns_first() {
        for set in [
            SymbolSet::Block,
            SymbolSet::Braille,
            SymbolSet::Eighth,
            SymbolSet::Shade,
            SymbolSet::Ascii,
        ] {
            let ch = set.char_for(0.0);
            check!(
                ch == set.chars()[0],
                "{set:?} char_for(0.0) should be first char"
            );
        }
    }

    #[test]
    fn symbol_set_char_for_one_returns_last() {
        for set in [
            SymbolSet::Block,
            SymbolSet::Braille,
            SymbolSet::Eighth,
            SymbolSet::Shade,
            SymbolSet::Ascii,
        ] {
            let ch = set.char_for(1.0);
            let chars = set.chars();
            check!(
                ch == chars[chars.len() - 1],
                "{set:?} char_for(1.0) should be last char"
            );
        }
    }

    #[test]
    fn symbol_set_char_for_clamps_above_one() {
        let ch = SymbolSet::Ascii.char_for(5.0);
        check!(ch == '@'); // last ASCII char
    }

    #[test]
    fn symbol_set_char_for_clamps_below_zero() {
        let ch = SymbolSet::Ascii.char_for(-1.0);
        check!(ch == '.'); // first ASCII char
    }

    #[test]
    fn symbol_set_char_for_midpoint() {
        let ch = SymbolSet::Ascii.char_for(0.5);
        let chars = SymbolSet::Ascii.chars();
        // 0.5 * 7 = 3.5, rounds to 4 -> '+'
        check!(ch == chars[4]);
    }

    #[test]
    fn symbol_set_all_have_nonempty_chars() {
        for set in [
            SymbolSet::Block,
            SymbolSet::Braille,
            SymbolSet::Eighth,
            SymbolSet::Shade,
            SymbolSet::Ascii,
        ] {
            assert!(!set.chars().is_empty());
        }
    }

    #[test]
    fn symbol_set_equality() {
        check!(SymbolSet::Braille == SymbolSet::Braille);
        check!(SymbolSet::Block != SymbolSet::Ascii);
    }

    #[test]
    fn symbol_set_clone() {
        let s = SymbolSet::Shade;
        let s2 = s;
        check!(s == s2);
    }

    #[test]
    fn region_columns_single_region_gets_full_width() {
        let regions = vec![make_region("r0", 1024)];
        let cols = region_columns(&regions, 80);
        check!(cols == vec![(0, 80)]);
    }

    #[test]
    fn region_columns_two_equal_regions() {
        let regions = vec![make_region("r0", 1024), make_region("r1", 1024)];
        let cols = region_columns(&regions, 80);
        // Both should be ~40 cols, last gets remainder
        check!(cols.len() == 2);
        let total_width: usize = cols.iter().map(|(_, w)| w).sum();
        check!(total_width == 80);
    }

    #[test]
    fn region_columns_different_sizes() {
        let regions = vec![make_region("r0", 3000), make_region("r1", 1000)];
        let cols = region_columns(&regions, 100);
        check!(cols.len() == 2);
        // r0 should be ~75 cols, r1 ~25 cols
        let (_, w0) = cols[0];
        assert!(w0 > 60, "larger region should get more columns, got {w0}");
        let total: usize = cols.iter().map(|(_, w)| w).sum();
        check!(total == 100);
    }

    #[test]
    fn region_columns_zero_total_bytes() {
        let regions = vec![make_region("r0", 0), make_region("r1", 0)];
        let cols = region_columns(&regions, 80);
        check!(cols == vec![(0, 1), (0, 1)]);
    }

    #[test]
    fn region_columns_minimum_width_is_one() {
        // One tiny region and one huge -- tiny should still get at least 1 col
        let regions = vec![make_region("r0", 1), make_region("r1", 1_000_000)];
        let cols = region_columns(&regions, 80);
        for (_, w) in &cols {
            assert!(*w >= 1, "every region should get at least 1 column");
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
    fn render_header_no_errors_no_verbose() {
        let mut term = test_terminal(80, 1);
        let regions = vec![make_region("r0", 1024)];
        let elapsed = Duration::from_secs_f64(1.5);
        term.draw(|frame| {
            render_header(frame, &regions, elapsed, false, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("ferrite"), "header should contain 'ferrite'");
        assert!(
            text.contains("1 regions"),
            "header should show region count"
        );
        assert!(text.contains("1.5s"), "header should show elapsed time");
        assert!(!text.contains("VERBOSE"));
    }

    #[test]
    fn render_header_with_errors() {
        let mut term = test_terminal(80, 1);
        let regions = vec![make_region("r0", 1024)];
        regions[0].failure_count.store(5, Ordering::Relaxed);
        let elapsed = Duration::from_secs(10);
        term.draw(|frame| {
            render_header(frame, &regions, elapsed, false, frame.area());
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
        let regions = vec![make_region("r0", 1024)];
        let elapsed = Duration::from_secs(0);
        term.draw(|frame| {
            render_header(frame, &regions, elapsed, true, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("VERBOSE"));
    }

    #[test]
    fn render_memory_map_narrow_width_returns_early() {
        let mut term = test_terminal(3, 1);
        let regions = vec![make_region("r0", 1024)];
        let errors: Vec<TuiFailure> = vec![];
        // Should not panic on very narrow width
        term.draw(|frame| {
            render_memory_map(frame, &regions, &errors, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_with_activity() {
        let mut term = test_terminal(40, 1);
        let regions = vec![make_region("r0", 1024)];
        regions[0].activity.touch(0.5);
        let errors: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_memory_map(frame, &regions, &errors, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_with_errors() {
        let mut term = test_terminal(40, 1);
        let regions = vec![make_region("r0", 1024)];
        let errors = vec![TuiFailure {
            region_idx: 0,
            region_name: "r0".into(),
            address: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            flipped_bits: FlippedBits::Single(0),
            pattern: "solid".into(),
            progress_fraction: 0.5,
        }];
        regions[0].record_failure();
        term.draw(|frame| {
            render_memory_map(frame, &regions, &errors, frame.area(), SymbolSet::Braille);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_labels_narrow_returns_early() {
        let mut term = test_terminal(3, 1);
        let regions = vec![make_region("r0", 1024)];
        term.draw(|frame| {
            render_memory_map_labels(frame, &regions, frame.area());
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_labels_shows_region_info() {
        let mut term = test_terminal(80, 1);
        let mb = 64 * 1024 * 1024;
        let regions = vec![make_region("r0", mb)];
        term.draw(|frame| {
            render_memory_map_labels(frame, &regions, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(
            text.contains("r0:64M"),
            "labels should show region size in MB, got: '{text}'"
        );
    }

    #[test]
    fn render_heatmap_region_shows_pattern_and_progress() {
        let mut term = test_terminal(80, 1);
        let regions = [make_region("r0", 1024)];
        regions[0].progress_bp.store(5000, Ordering::Relaxed);
        let errors: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_heatmap_region(
                frame,
                &regions[0],
                &errors,
                0,
                frame.area(),
                SymbolSet::Ascii,
            );
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("r0"), "should show region name");
        assert!(text.contains("50.0%"), "should show progress percentage");
        assert!(text.contains("solid"), "should show pattern name");
        assert!(text.contains("ok"), "should show ok for no errors");
    }

    #[test]
    fn render_heatmap_region_shows_errors() {
        let mut term = test_terminal(80, 1);
        let regions = [make_region("r0", 1024)];
        regions[0].failure_count.store(3, Ordering::Relaxed);
        let errors: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_heatmap_region(
                frame,
                &regions[0],
                &errors,
                0,
                frame.area(),
                SymbolSet::Ascii,
            );
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("3err"), "should show error count");
    }

    #[test]
    fn render_heatmap_region_paused() {
        let mut term = test_terminal(80, 1);
        let regions = [make_region("r0", 1024)];
        regions[0].paused.store(true, Ordering::Relaxed);
        let errors: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_heatmap_region(
                frame,
                &regions[0],
                &errors,
                0,
                frame.area(),
                SymbolSet::Ascii,
            );
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("⏸"), "should show pause indicator");
    }

    #[test]
    fn render_error_area_empty() {
        let mut term = test_terminal(80, 3);
        let errors: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_error_area(frame, &errors, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("no errors detected"));
    }

    #[test]
    fn render_error_area_with_errors() {
        let mut term = test_terminal(120, 5);
        let errors = vec![
            TuiFailure {
                region_idx: 0,
                region_name: "r0".into(),
                address: 0xdead,
                expected: 0xFF,
                actual: 0xFE,
                flipped_bits: FlippedBits::Single(0),
                pattern: "solid".into(),
                progress_fraction: 0.1,
            },
            TuiFailure {
                region_idx: 0,
                region_name: "r0".into(),
                address: 0xbeef,
                expected: 0xAA,
                actual: 0xBB,
                flipped_bits: FlippedBits::Single(4),
                pattern: "walk".into(),
                progress_fraction: 0.5,
            },
        ];
        term.draw(|frame| {
            render_error_area(frame, &errors, frame.area());
        })
        .unwrap();
        let text = buf_text(&term);
        assert!(text.contains("Region"), "should have table header");
        assert!(text.contains("r0"), "should show region name in error rows");
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
        assert!(text.contains("kip"), "should show skip control");
        assert!(text.contains("uit"), "should show quit control");
    }

    #[test]
    fn render_heatmap_full_layout() {
        let mut term = test_terminal(80, 15);
        let regions = vec![make_region("r0", 1024), make_region("r1", 2048)];
        regions[0].progress_bp.store(3000, Ordering::Relaxed);
        regions[1].progress_bp.store(7500, Ordering::Relaxed);
        let errors: Vec<TuiFailure> = vec![];
        let elapsed = Duration::from_secs(5);
        term.draw(|frame| {
            render_heatmap(frame, &regions, &errors, elapsed, false, SymbolSet::Ascii);
        })
        .unwrap();
        // Should not panic -- layout fits all sections
    }

    #[test]
    fn render_heatmap_with_errors_full() {
        let mut term = test_terminal(80, 15);
        let regions = vec![make_region("r0", 1024)];
        regions[0].failure_count.store(2, Ordering::Relaxed);
        regions[0].record_failure();
        let errors = vec![TuiFailure {
            region_idx: 0,
            region_name: "r0".into(),
            address: 0x1000,
            expected: 0xFF,
            actual: 0xFE,
            flipped_bits: FlippedBits::Single(0),
            pattern: "solid".into(),
            progress_fraction: 0.3,
        }];
        let elapsed = Duration::from_secs(2);
        term.draw(|frame| {
            render_heatmap(frame, &regions, &errors, elapsed, true, SymbolSet::Braille);
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_multiple_regions() {
        let mut term = test_terminal(80, 1);
        let regions = vec![
            make_region("r0", 1024),
            make_region("r1", 2048),
            make_region("r2", 512),
        ];
        for r in &regions {
            r.activity.touch(0.3);
        }
        let errors: Vec<TuiFailure> = vec![];
        term.draw(|frame| {
            render_memory_map(frame, &regions, &errors, frame.area(), SymbolSet::Shade);
        })
        .unwrap();
    }

    #[test]
    fn render_heatmap_region_with_error_overlays() {
        let mut term = test_terminal(80, 1);
        let regions = [make_region("r0", 1024)];
        regions[0].record_failure();
        let errors = vec![
            TuiFailure {
                region_idx: 0,
                region_name: "r0".into(),
                address: 0x100,
                expected: 0xFF,
                actual: 0x00,
                flipped_bits: FlippedBits::Single(0),
                pattern: "solid".into(),
                progress_fraction: 0.25,
            },
            TuiFailure {
                region_idx: 0,
                region_name: "r0".into(),
                address: 0x200,
                expected: 0xFF,
                actual: 0x00,
                flipped_bits: FlippedBits::Single(0),
                pattern: "solid".into(),
                progress_fraction: 0.25, // same column as first
            },
        ];
        term.draw(|frame| {
            render_heatmap_region(
                frame,
                &regions[0],
                &errors,
                0,
                frame.area(),
                SymbolSet::Eighth,
            );
        })
        .unwrap();
    }

    #[test]
    fn render_memory_map_error_out_of_bounds_region() {
        let mut term = test_terminal(40, 1);
        let regions = vec![make_region("r0", 1024)];
        // Error referencing non-existent region
        let errors = vec![TuiFailure {
            region_idx: 99,
            region_name: "r99".into(),
            address: 0x1000,
            expected: 0,
            actual: 1,
            flipped_bits: FlippedBits::Single(0),
            pattern: "solid".into(),
            progress_fraction: 0.5,
        }];
        term.draw(|frame| {
            render_memory_map(frame, &regions, &errors, frame.area(), SymbolSet::Ascii);
        })
        .unwrap();
    }
}
