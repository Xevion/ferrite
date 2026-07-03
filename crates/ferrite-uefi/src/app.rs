use core::time::Duration;

use ratatui::prelude::*;
use ratatui::widgets::{Block, BorderType, Borders, Gauge, Paragraph, Wrap};
use uefi::boot;
use uefi::proto::console::text::{Input, Key, Output, ScanCode};

use crate::backend::UefiBackend;

pub struct App<'a> {
    quit: bool,
    progress: u16,
    input: &'a mut Input,
    terminal: Terminal<UefiBackend<'a>>,
}

impl<'a> App<'a> {
    pub fn new(input: &'a mut Input, output: &'a mut Output) -> Self {
        let terminal = Terminal::new(UefiBackend::new(output)).unwrap();
        Self {
            quit: false,
            progress: 0,
            input,
            terminal,
        }
    }

    pub fn run(&mut self) {
        while !self.quit {
            self.draw();
            self.poll_input();
            // No real work yet: this animates a placeholder sweep so we can see
            // the render loop, ratatui's diffing, and the run-coalescing backend
            // actually driving the firmware console.
            self.progress = (self.progress + 1) % 101;
            boot::stall(Duration::from_millis(40));
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
        self.terminal
            .draw(|frame| render(frame, progress))
            .unwrap();
    }
}

fn render(frame: &mut Frame, progress: u16) {
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
        Constraint::Length(5), // info
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

    let info = Paragraph::new(vec![
        info_line("target", "x86_64-unknown-uefi"),
        info_line("region", "not yet allocated"),
        info_line("network", "wired only (mDNS/HTTP planned)"),
        info_line("status", "scaffold, render loop live"),
    ])
    .block(Block::default().borders(Borders::ALL).title(" system "))
    .wrap(Wrap { trim: true });
    frame.render_widget(info, rows[1]);

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

fn info_line(key: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:>8}  "),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{value}"), Style::new().fg(Color::White)),
    ])
}
