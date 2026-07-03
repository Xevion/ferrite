use core::time::Duration;

use alloc::string::String;
use alloc::vec::Vec;
use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Wrap};
use uefi::boot;
use uefi::proto::console::text::{Input, Key, Output, ScanCode};

use crate::backend::UefiBackend;
use crate::net::{Net, NetStatus};

/// Wall time we sleep between render/poll iterations.
const TICK_MS: u64 = 40;

pub struct App<'a> {
    quit: bool,
    progress: u16,
    input: &'a mut Input,
    terminal: Terminal<UefiBackend<'a>>,
    net: Option<Net>,
    net_err: Option<&'static str>,
}

impl<'a> App<'a> {
    pub fn new(input: &'a mut Input, output: &'a mut Output) -> Self {
        let terminal = Terminal::new(UefiBackend::new(output)).unwrap();
        let (net, net_err) = match Net::setup() {
            Ok(net) => (Some(net), None),
            Err(e) => (None, Some(status_reason(e.status()))),
        };
        Self {
            quit: false,
            progress: 0,
            input,
            terminal,
            net,
            net_err,
        }
    }

    pub fn run(&mut self) {
        while !self.quit {
            if let Some(net) = self.net.as_mut() {
                net.set_page(http_page(net.status(), self.progress));
                net.poll(TICK_MS);
            }
            self.draw();
            self.poll_input();
            self.progress = (self.progress + 1) % 101;
            boot::stall(Duration::from_millis(TICK_MS));
        }
    }

    fn poll_input(&mut self) {
        // Non-blocking: read_key returns Ok(None) when nothing is queued.
        while let Ok(Some(key)) = self.input.read_key() {
            match key {
                Key::Printable(c) if matches!(char::from(c), 'q' | 'Q') => self.quit = true,
                Key::Special(ScanCode::ESCAPE) => self.quit = true,
                _ => {}
            }
        }
    }

    fn draw(&mut self) {
        let progress = self.progress;
        let status = self.net.as_ref().map(Net::status);
        let net_err = self.net_err;
        self.terminal
            .draw(|frame| render(frame, progress, status, net_err))
            .unwrap();
    }
}

fn render(frame: &mut Frame, progress: u16, status: Option<NetStatus>, net_err: Option<&str>) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Double)
        .border_style(Style::new().fg(Color::Cyan))
        .title(" ferrite-uefi ")
        .title_alignment(Alignment::Center);
    let inner = outer.inner(frame.area());
    frame.render_widget(outer, frame.area());

    let rows = Layout::vertical([
        Constraint::Length(2), // banner
        Constraint::Length(7), // network
        Constraint::Length(3), // gauge
        Constraint::Min(0),    // spacer
        Constraint::Length(1), // footer
    ])
    .split(inner);

    let banner = Paragraph::new(vec![
        Line::from(Span::styled(
            "bare-metal memory tester - spike",
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "ratatui on UEFI, no OS underneath",
            Style::new().fg(Color::DarkGray),
        )),
    ])
    .alignment(Alignment::Center);
    frame.render_widget(banner, rows[0]);

    frame.render_widget(network_panel(status, net_err), rows[1]);

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(" sweep "))
        .gauge_style(Style::new().fg(Color::Green).bg(Color::Black))
        .percent(progress);
    frame.render_widget(gauge, rows[2]);

    let footer = Paragraph::new(Line::from(vec![
        Span::styled("q", Style::new().fg(Color::Yellow)),
        Span::raw(" / "),
        Span::styled("Esc", Style::new().fg(Color::Yellow)),
        Span::raw(" to exit"),
    ]))
    .alignment(Alignment::Center);
    frame.render_widget(footer, rows[4]);
}

fn network_panel(status: Option<NetStatus>, net_err: Option<&str>) -> Paragraph<'static> {
    let mut lines = Vec::new();

    match status {
        None => {
            let reason = net_err.unwrap_or("no NIC found");
            lines.push(info_line("nic", reason, Color::Red));
            lines.push(info_line(
                "hint",
                "OVMF may lack a network stack; add -device e1000",
                Color::DarkGray,
            ));
        }
        Some(s) => {
            let mac = format!(
                "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                s.mac[0], s.mac[1], s.mac[2], s.mac[3], s.mac[4], s.mac[5]
            );
            let (link, link_color) = if s.link_up {
                ("up", Color::Green)
            } else {
                ("down", Color::Red)
            };
            lines.push(info_line("mac", &mac, Color::White));
            lines.push(info_line("link", link, link_color));

            match s.address {
                Some(cidr) => lines.push(info_line("ip", &format!("{cidr}"), Color::LightGreen)),
                None => lines.push(info_line("ip", "requesting (DHCP)...", Color::Yellow)),
            }
            lines.push(info_line("gateway", &opt_addr(s.gateway), Color::White));
            lines.push(info_line("dns", &opt_addr(s.dns), Color::White));
        }
    }

    Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" network "))
        .wrap(Wrap { trim: true })
}

fn opt_addr(addr: Option<smoltcp::wire::Ipv4Address>) -> String {
    addr.map_or_else(|| String::from("-"), |a| format!("{a}"))
}

fn http_page(status: NetStatus, progress: u16) -> String {
    let ip = status
        .address
        .map_or_else(|| String::from("(none)"), |c| format!("{c}"));
    let mac = format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        status.mac[0], status.mac[1], status.mac[2], status.mac[3], status.mac[4], status.mac[5]
    );
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <title>ferrite-uefi</title></head><body style=\"font-family:monospace\">\
         <h1>ferrite-uefi</h1>\
         <p>bare-metal memory tester &mdash; spike, no OS underneath</p>\
         <ul><li>ip: {ip}</li><li>mac: {mac}</li><li>sweep: {progress}%</li></ul>\
         </body></html>"
    )
}

const fn status_reason(status: uefi::Status) -> &'static str {
    match status {
        uefi::Status::NOT_FOUND => "no SNP device (no NIC / OVMF lacks net stack)",
        uefi::Status::ACCESS_DENIED => "SNP busy (held by firmware net stack)",
        uefi::Status::UNSUPPORTED => "SNP unsupported by firmware",
        _ => "SNP setup failed",
    }
}

fn info_line(key: &str, value: &str, value_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:>8}  "),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(String::from(value), Style::new().fg(value_color)),
    ])
}
