//! The text-console status screen, for machines without GOP.
//!
//! It shows the current test, pass progress, faults, the address to open and the keys, in the
//! framebuffer dashboard's firmware-setup look as far as 16 console colours allow: a blue title bar
//! and key bar over a grey page with blue labels. The dashboard is the real interface; this only
//! has to say whether the run is healthy.

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec;
use alloc::vec::Vec;

use ferrite_net::{NetStatus, PortStatus};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Gauge, Paragraph};

use ferrite_mem::eta::{BasisPoints, Confidence, Position};

use crate::dash::format;
use crate::net::NoNetwork;
use crate::sweep::SweepStatus;
use crate::wall::{Ends, TimeLine};

/// Everything one frame shows; a frame equal to the last one is not redrawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub sweep: SweepStatus,
    pub net: Option<NetStatus>,
    /// Why there is no network, when no port is up.
    pub net_err: Option<NoNetwork>,
    /// Whole seconds since the run started.
    pub elapsed: u64,
    /// The time of day and when the times left end, when the clock has a source.
    pub time: Option<TimeLine>,
}

/// The page: dark ink on grey.
const PAGE: Style = Style::new().fg(Color::Black).bg(Color::Gray);
/// The title and key bars: white on blue.
const BAR: Style = Style::new().fg(Color::White).bg(Color::Blue);
const LABEL: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);
/// A time left that is only a guess, against the page's dark ink.
const GUESS: Style = Style::new().fg(Color::DarkGray);

pub fn render(frame: &mut Frame<'_>, view: &View) {
    let area = frame.area();
    frame.render_widget(Block::new().style(PAGE), area);
    let lines = status_lines(view);
    let [title, rows, gauge, _, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(u16::try_from(lines.len() + 1).unwrap_or(u16::MAX)),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let name = Style::new().add_modifier(Modifier::BOLD);
    let heading = Line::from(vec![
        Span::styled(" FERRITE", name),
        Span::raw("  Memory Diagnostic  "),
        Span::raw(concat!("v", env!("CARGO_PKG_VERSION"))),
    ]);
    let heading_width = heading.width();
    frame.render_widget(Paragraph::new(heading).style(BAR), title);
    if let Some(time) = view.time {
        let now = format!("{} ", time.now.text(time.format));
        if usize::from(title.width) > heading_width + now.len() {
            frame.render_widget(Paragraph::new(now).alignment(Alignment::Right).style(BAR), title);
        }
    }
    frame.render_widget(Paragraph::new(lines), rows);

    let sweep = &view.sweep;
    let progress = sweep.outlook.pass_progress;
    let label = format!("pass {}  {progress}", sweep.passes + 1);
    let color = if sweep.paused { Color::DarkGray } else { Color::LightBlue };
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(color).bg(Color::Black))
            .use_unicode(false)
            .ratio(f64::from(progress.get()) / f64::from(BasisPoints::WHOLE))
            .label(label),
        gauge,
    );
    frame.render_widget(Paragraph::new(key_hints(sweep.paused)).style(BAR), keys);
}

fn key_hints(paused: bool) -> Line<'static> {
    let cap = Style::new().fg(Color::Blue).bg(Color::Gray).add_modifier(Modifier::BOLD);
    let mut spans = vec![Span::raw(" ")];
    for (key, what) in [("P", if paused { "resume" } else { "pause" }), ("R", "restart"), ("Q", "quit")] {
        spans.push(Span::styled(format!(" {key} "), cap));
        spans.push(Span::raw(format!(" {what}  ")));
    }
    Line::from(spans)
}

/// A time left, and when it ends, in the ink that says how far to trust it.
fn left_span(eta: ferrite_mem::eta::Eta, ends: Option<(Ends, &TimeLine)>) -> Span<'static> {
    let mut text = format::left(eta.remaining).to_string();
    if let Some((ends, time)) = ends {
        // The console's character set is plain, so the separator is too.
        text = format!("{text}, ends {}", ends.text(time.format));
    }
    match eta.confidence {
        Confidence::Measured => Span::raw(text),
        Confidence::Estimated => Span::styled(text, GUESS),
    }
}

fn status_lines(view: &View) -> Vec<Line<'static>> {
    let sweep = &view.sweep;
    let o = &sweep.outlook;
    let (i, n) = sweep.position;
    let state = if sweep.paused { " (paused)" } else { "" };
    let phase = match o.position {
        Position::Wait { nth, waits, left } => format!(", wait {nth} of {waits}, {} left", format::clock(left)),
        Position::Work { .. } => String::new(),
    };
    let test = format!("{} ({i}/{n}) {}{phase}{state}", sweep.test, o.test_progress);
    let time = view.time.as_ref();
    let pass = o.pass.map_or_else(
        || vec![Span::raw(format!("{}", o.pass_progress))],
        |eta| {
            let ends = time.and_then(|time| time.pass.map(|ends| (ends, time)));
            vec![Span::raw(format!("{}  ", o.pass_progress)), left_span(eta, ends)]
        },
    );
    let faults = match sweep.faults {
        0 => Span::styled("none", Style::new().fg(Color::Green)),
        n => Span::styled(
            format!(
                "{n} ({}{})",
                sweep.analysis,
                sweep.first_fault.map_or_else(String::new, |f| format!("; first in {} on cpu {}", f.test, f.cpu))
            ),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let now = match (o.position, o.rate) {
        (Position::Work { .. }, Some(rate)) if !sweep.paused => format::rate(rate).to_string(),
        _ => String::from("-"),
    };
    let average = match (o.test_avg, o.test_delta) {
        // The console's character set has no minus sign, so this one is ASCII.
        (Some(avg), Some(delta)) => format!("{} {:+}%", format::rate(avg), delta.percent()),
        (Some(avg), None) => format::rate(avg).to_string(),
        (None, _) => String::from("-"),
    };
    let pace = o.pace_percent.map_or_else(|| String::from("-"), |p| format!("{p}%"));
    let rate = format!("now {now}, test avg {average}, pace {pace}");
    let overall = o.overall(core::time::Duration::from_secs(view.elapsed));
    let overall = match (overall.fill, overall.left) {
        (Some(fill), Some(left)) => {
            let ends = time.and_then(|time| time.overall.map(|ends| (ends, time)));
            vec![Span::raw(format!("{fill}, ")), left_span(left, ends), Span::raw(format!(" (goal: {})", o.goal))]
        },
        (Some(fill), None) => vec![Span::raw(format!("{fill} (goal: {})", o.goal))],
        (None, _) => vec![Span::raw(format!("no goal, running {}", format::clock(overall.elapsed)))],
    };
    let mut lines = vec![
        row("test", Span::raw(test)),
        row_of("pass", pass),
        row("rate", Span::raw(rate)),
        row_of("overall", overall),
        row("faults", faults),
    ];
    if let Some(net) = view.net {
        for (n, port) in net.ports().iter().enumerate() {
            lines.push(row(if n == 0 { "network" } else { "" }, port_span(port)));
        }
    } else {
        let message = view.net_err.map_or_else(|| String::from("no network"), |why| format!("no network: {why}"));
        lines.push(row("network", Span::styled(message, Style::new().fg(Color::Red))));
    }
    lines
}

/// One port's line: its medium and the address to open, or why there is none yet.
fn port_span(port: &PortStatus) -> Span<'static> {
    let link = &port.link;
    let degraded = Style::new().fg(Color::Magenta);
    match (link.address, link.up) {
        (Some(cidr), true) => Span::raw(format!("{} open http://{}/", link.medium, cidr.address())),
        (None, true) => Span::styled(format!("{} waiting for DHCP", link.medium), degraded),
        (_, false) => Span::styled(format!("{} link down", link.medium), degraded),
    }
}

fn row(key: &'static str, value: Span<'static>) -> Line<'static> {
    row_of(key, vec![value])
}

fn row_of(key: &'static str, mut value: Vec<Span<'static>>) -> Line<'static> {
    value.insert(0, Span::styled(format!("{:<8}  ", key.to_ascii_uppercase()), LABEL));
    Line::from(value)
}

#[cfg(test)]
mod tests {
    use alloc::string::String;
    use core::num::NonZeroU64;
    use core::time::Duration;

    use assert2::check;
    use ferrite_mem::eta::{BasisPoints, Confidence, Eta, Position};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;

    use ferrite_config::ClockFormat;

    use super::*;
    use crate::goal::Goal;
    use crate::net::testing::idle_status;
    use crate::pace::{Delta, Outlook};
    use crate::wall::{Day, TimeOfDay};

    fn screen(sweep: &SweepStatus, elapsed: u64) -> String {
        screen_at(sweep, elapsed, None)
    }

    fn screen_at(sweep: &SweepStatus, elapsed: u64, time: Option<TimeLine>) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("a test terminal");
        let view = View { sweep: *sweep, net: None, net_err: None, elapsed, time };
        terminal.draw(|frame| render(frame, &view)).expect("draws");
        let buffer = terminal.backend().buffer().clone();
        let width = usize::from(buffer.area.width);
        buffer
            .content()
            .chunks(width)
            .map(|row| row.iter().map(Cell::symbol).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_text_screen_says_when_there_is_no_goal_and_what_a_bit_fade_wait_has_left() {
        let shown = screen(&idle_status(), 125);
        check!(shown.contains("no goal, running 2:05"), "{shown}");
        check!(!shown.contains("left"), "no ETA is made up: {shown}");

        let outlook = Outlook {
            goal: Goal { passes: NonZeroU64::new(4), time: None, stop_on_fault: false },
            settled: 1,
            position: Position::Wait { nth: 2, waits: 4, left: Duration::from_secs(192) },
            rate: Some(8_000_000_000),
            test_avg: Some(7_900_000_000),
            test_delta: Delta::between(7_900_000_000, 8_400_000_000),
            pass: Some(Eta { remaining: Duration::from_mins(10), confidence: Confidence::Estimated }),
            pass_progress: BasisPoints::new(7234).unwrap(),
            test_progress: BasisPoints::new(4512).unwrap(),
            cycle: Some(Duration::from_mins(40)),
            cycle_left: Some(Eta { remaining: Duration::from_mins(15), confidence: Confidence::Estimated }),
            pace_percent: Some(98),
            ..Outlook::NONE
        };
        let shown = screen(&SweepStatus { outlook, ..idle_status() }, 125);
        check!(shown.contains("wait 2 of 4, 3:12 left"), "{shown}");
        check!(shown.contains("pass 1  72.34%"), "{shown}");
        check!(shown.contains("PASS      72.34%  10 min left"), "{shown}");
        check!(shown.contains("45.12%"), "{shown}");
        check!(shown.contains("now -, test avg 7.90 GB/s -6%, pace 98%"), "{shown}");
        check!(shown.contains("(goal: 4 passes)"), "{shown}");
        check!(!shown.contains("ends") && !shown.contains(" AM"), "no clock, no times: {shown}");
    }

    #[test]
    fn the_text_screen_shows_the_time_of_day_and_when_the_times_left_end() {
        let outlook = Outlook {
            goal: Goal { passes: NonZeroU64::new(4), time: None, stop_on_fault: false },
            settled: 1,
            pass: Some(Eta { remaining: Duration::from_mins(10), confidence: Confidence::Estimated }),
            pass_progress: BasisPoints::new(7234).unwrap(),
            cycle: Some(Duration::from_mins(40)),
            cycle_left: Some(Eta { remaining: Duration::from_mins(15), confidence: Confidence::Measured }),
            ..Outlook::NONE
        };
        let ends = |day| Some(Ends { day, time: TimeOfDay { hour: 3, minute: 14, second: 0 } });
        let time = TimeLine {
            now: TimeOfDay { hour: 1, minute: 27, second: 43 },
            format: ClockFormat::Twelve,
            overall: ends(Day::Weekday(jiff::civil::Weekday::Saturday)),
            pass: ends(Day::Today),
        };
        let shown = screen_at(&SweepStatus { outlook, ..idle_status() }, 125, Some(time));
        check!(shown.lines().next().unwrap().trim_end().ends_with("01:27:43 AM"), "{shown}");
        check!(shown.contains("PASS      72.34%  10 min left, ends 03:14 AM"), "{shown}");
        check!(shown.contains("OVERALL"), "{shown}");
    }
}
