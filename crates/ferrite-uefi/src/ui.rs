//! The text-console status screen, for machines without GOP.
//!
//! It shows the current test, pass progress, faults, the address to open and the keys, in the
//! framebuffer dashboard's firmware-setup look as far as 16 console colours allow: a blue title bar
//! and key bar over a grey page with blue labels. The dashboard is the real interface; this only
//! has to say whether the run is healthy.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use ferrite_net::{NetStatus, PortStatus};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Gauge, Paragraph};

use crate::net::NoNetwork;
use crate::sweep::SweepStatus;

/// Everything one frame shows; a frame equal to the last one is not redrawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub sweep: SweepStatus,
    pub net: Option<NetStatus>,
    /// Why there is no network, when no port is up.
    pub net_err: Option<NoNetwork>,
}

/// The page: dark ink on grey.
const PAGE: Style = Style::new().fg(Color::Black).bg(Color::Gray);
/// The title and key bars: white on blue.
const BAR: Style = Style::new().fg(Color::White).bg(Color::Blue);
const LABEL: Style = Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD);

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
    frame.render_widget(Paragraph::new(heading).style(BAR), title);
    frame.render_widget(Paragraph::new(lines), rows);

    let sweep = &view.sweep;
    let label = format!("pass {}  {}%", sweep.passes + 1, sweep.percent.min(100));
    let color = if sweep.paused { Color::DarkGray } else { Color::LightBlue };
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(color).bg(Color::Black))
            .use_unicode(false)
            .percent(sweep.percent.min(100))
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

fn status_lines(view: &View) -> Vec<Line<'static>> {
    let sweep = &view.sweep;
    let (i, n) = sweep.position;
    let state = if sweep.paused { " (paused)" } else { "" };
    let test = format!("{} ({i}/{n}) {}%{state}", sweep.test, sweep.test_percent.min(100));
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
    let mut lines = vec![row("test", Span::raw(test)), row("faults", faults)];
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
    Line::from(vec![Span::styled(format!("{:<8}  ", key.to_ascii_uppercase()), LABEL), value])
}
