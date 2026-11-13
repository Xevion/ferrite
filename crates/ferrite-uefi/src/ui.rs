//! The text-console status screen, for machines without GOP.
//!
//! It shows the current test, pass progress, faults, the address to open and the keys. The
//! framebuffer dashboard is the real interface; this only has to say whether the run is healthy.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};

use crate::net::NetStatus;
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
    let [lines, gauge] = Layout::vertical([Constraint::Length(5), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(status_lines(view)), lines);

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
            format!("{n} ({}; first in {} on cpu {})", sweep.analysis, sweep.first_fault_test, sweep.first_fault_cpu),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
    };
    let network = match (view.net, view.net_err) {
        (Some(net), _) => match net.link.address {
            Some(cidr) if net.link.up => Span::raw(format!("open http://{}/", cidr.address())),
            Some(_) => Span::styled("link down", Style::new().fg(Color::Yellow)),
            None => Span::styled("waiting for DHCP", Style::new().fg(Color::Yellow)),
        },
        (None, err) => Span::styled(String::from(err.unwrap_or("no NIC")), Style::new().fg(Color::Red)),
    };
    vec![
        row("test", Span::raw(test)),
        row("faults", faults),
        row("network", network),
        row("keys", Span::raw("p pause/resume   r restart   q quit")),
    ]
}

fn row(key: &'static str, value: Span<'static>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{key:>8}  "), Style::new().fg(Color::Cyan)), value])
}
