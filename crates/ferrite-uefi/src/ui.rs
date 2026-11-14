//! The text-console status screen, for machines without GOP.
//!
//! It shows the current test, pass progress, faults, the address to open and the keys. The
//! framebuffer dashboard is the real interface; this only has to say whether the run is healthy.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use ferrite_net::{NetStatus, PortStatus};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

use crate::sweep::SweepStatus;

/// Everything one frame shows; a frame equal to the last one is not redrawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub sweep: SweepStatus,
    pub net: Option<NetStatus>,
    /// Why there is no network, when setup failed.
    pub net_err: Option<&'static str>,
}

pub fn render(frame: &mut Frame<'_>, view: &View) {
    let outer = Block::default().borders(Borders::ALL).title(" ferrite-uefi memory test ");
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());
    let lines = status_lines(view);
    let [rows, gauge] = Layout::vertical([
        Constraint::Length(u16::try_from(lines.len() + 1).unwrap_or(u16::MAX)),
        Constraint::Length(1),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(lines), rows);

    let sweep = &view.sweep;
    let label = format!("pass {}  {}%", sweep.passes + 1, sweep.percent.min(100));
    let color = if sweep.paused { Color::Yellow } else { Color::Green };
    frame.render_widget(
        Gauge::default().gauge_style(Style::new().fg(color)).percent(sweep.percent.min(100)).label(label),
        gauge,
    );
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
        let reason = view.net_err.unwrap_or("no wired NIC driven by the firmware");
        let message = format!("no network: {reason}, Wi-Fi not up");
        lines.push(row("network", Span::styled(message, Style::new().fg(Color::Red))));
    }
    lines.push(row("keys", Span::raw("p pause/resume   r restart   q quit")));
    lines
}

/// One port's line: its medium and the address to open, or why there is none yet.
fn port_span(port: &PortStatus) -> Span<'static> {
    let link = &port.link;
    let yellow = Style::new().fg(Color::Yellow);
    match (link.address, link.up) {
        (Some(cidr), true) => Span::raw(format!("{} open http://{}/", link.medium, cidr.address())),
        (None, true) => Span::styled(format!("{} waiting for DHCP", link.medium), yellow),
        (_, false) => Span::styled(format!("{} link down", link.medium), yellow),
    }
}

fn row(key: &'static str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{key:>8}  "), Style::new().fg(Color::Cyan)), value])
}
