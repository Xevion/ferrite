//! The ratatui screen: banner, network panel, sweep gauge and footer.

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Wrap};

use crate::net::{NetStatus, OptAddr};
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
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" ferrite-uefi ")
        .title_alignment(Alignment::Center);
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let [banner, network, gauge, summary, _, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(7),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);

    frame.render_widget(banner_widget(), banner);
    frame.render_widget(network_panel(view.net.as_ref(), view.net_err), network);
    frame.render_widget(sweep_gauge(&view.sweep), gauge);
    frame.render_widget(sweep_summary(&view.sweep), summary);
    frame.render_widget(footer_widget(), footer);
}

fn banner_widget() -> Paragraph<'static> {
    Paragraph::new(vec![
        Line::from(Span::styled(
            "bare-metal memory tester - spike",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled("ratatui on UEFI, no OS underneath", Style::new().fg(Color::DarkGray))),
    ])
    .alignment(Alignment::Center)
}

fn footer_widget() -> Paragraph<'static> {
    Paragraph::new(Line::from(vec![
        Span::styled("q", Style::new().fg(Color::Yellow)),
        Span::raw(" / "),
        Span::styled("Esc", Style::new().fg(Color::Yellow)),
        Span::raw(" to exit"),
    ]))
    .alignment(Alignment::Center)
}

fn sweep_gauge(sweep: &SweepStatus) -> Gauge<'static> {
    Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" memory sweep "))
        .gauge_style(Style::new().fg(Color::Green).bg(Color::Black))
        .percent(sweep.percent.min(100))
        .label(format!(
            "{} ({}/{}) {}% on {} cpus, pass {}%",
            sweep.test, sweep.position.0, sweep.position.1, sweep.test_percent, sweep.cpus, sweep.percent
        ))
}

fn sweep_summary(sweep: &SweepStatus) -> Paragraph<'static> {
    let mut spans = vec![
        Span::styled(format!(" {} MiB ", sweep.bytes >> 20), Style::new().fg(Color::DarkGray)),
        Span::styled(format!("{}  ", sweep.tier), Style::new().fg(Color::DarkGray)),
        Span::styled(format!("pass {}  ", sweep.passes), Style::new().fg(Color::White)),
    ];
    match sweep.first_fault {
        None => spans.push(Span::styled("no faults", Style::new().fg(Color::Green))),
        Some(f) => spans.push(Span::styled(
            format!(
                "{} FAULTS (first @ {} in {} on cpu{}, bits {:#x}; {})",
                sweep.faults,
                f.addr,
                sweep.first_fault_test,
                sweep.first_fault_cpu,
                f.expected ^ f.actual,
                sweep.analysis
            ),
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        )),
    }
    Paragraph::new(Line::from(spans))
}

fn network_panel(status: Option<&NetStatus>, net_err: Option<&str>) -> Paragraph<'static> {
    let mut lines = Vec::new();
    match status {
        None => {
            lines.push(info_line("nic", net_err.unwrap_or("no NIC found"), Color::Red));
            lines.push(info_line("hint", "OVMF may lack a network stack; add -device e1000", Color::DarkGray));
        },
        Some(s) => {
            let link = &s.link;
            let (state, state_color) = if link.up { ("up", Color::Green) } else { ("down", Color::Red) };
            lines.push(info_line("mac", &format!("{}", link.mac), Color::White));
            lines.push(info_line("link", state, state_color));
            match link.address {
                Some(cidr) => lines.push(info_line("ip", &format!("{cidr}"), Color::LightGreen)),
                None => lines.push(info_line("ip", "requesting (DHCP)...", Color::Yellow)),
            }
            lines.push(info_line("gateway", &format!("{}", OptAddr(link.gateway)), Color::White));
            lines.push(info_line("dns", &format!("{}", OptAddr(link.dns)), Color::White));
        },
    }
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" network ")).wrap(Wrap { trim: true })
}

fn info_line(key: &str, value: &str, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:>8}  "), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(String::from(value), Style::new().fg(value_color)),
    ])
}
