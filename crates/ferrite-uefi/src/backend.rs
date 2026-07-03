//! A ratatui [`Backend`] that renders to the UEFI Simple Text Output Protocol.
//!
//! Vendored from `ratatuefi` (MIT OR Apache-2.0) and reworked. The upstream
//! `draw` issued three firmware protocol calls per character cell
//! (`set_cursor_position` + `set_color` + `write_str`), which is painfully slow
//! on real firmware where each STOP call is expensive. Here we coalesce runs of
//! contiguous, identically-styled cells on the same row into a single
//! `write_str`, and only re-issue cursor/color calls when they actually change.
//! Combined with ratatui's own frame diffing (only changed cells reach `draw`),
//! this collapses "calls per character" down to "calls per styled run".
//!
//! This is a text-console backend: a 16-colour character grid, no pixel
//! graphics. A GOP framebuffer backend with an embedded font is the upgrade
//! path for a truecolor UI.

use alloc::string::String;

use ratatui_core::{
    backend::{self, Backend, ClearType, WindowSize},
    buffer,
    layout::{Position, Size},
    style::Color,
};
use uefi::proto::console::text::{Color as UefiColor, Output};

pub struct UefiBackend<'a> {
    output: &'a mut Output,
}

impl<'a> UefiBackend<'a> {
    pub fn new(output: &'a mut Output) -> Self {
        Self { output }
    }
}

/// The text console cannot fail from our side; every STOP call we make is
/// infallible-by-construction or best-effort ignored, so the error type is
/// uninhabited.
#[derive(Debug)]
pub enum UefiBackendError {}

impl core::fmt::Display for UefiBackendError {
    fn fmt(&self, _f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {}
    }
}

impl core::error::Error for UefiBackendError {}

/// Map a ratatui foreground colour onto the 16 EFI text colours. Anything we
/// don't have an exact slot for degrades to a sensible neighbour rather than
/// panicking (upstream `panic!`d here).
fn fg_color(color: Color) -> UefiColor {
    match color {
        Color::Reset | Color::White | Color::Gray => UefiColor::White,
        Color::Black => UefiColor::Black,
        Color::Red => UefiColor::Red,
        Color::Green => UefiColor::Green,
        Color::Yellow => UefiColor::Yellow,
        Color::Blue => UefiColor::Blue,
        Color::Magenta => UefiColor::Magenta,
        Color::Cyan => UefiColor::Cyan,
        Color::DarkGray => UefiColor::DarkGray,
        Color::LightRed => UefiColor::LightRed,
        Color::LightGreen => UefiColor::LightGreen,
        Color::LightYellow => UefiColor::Yellow,
        Color::LightBlue => UefiColor::LightBlue,
        Color::LightMagenta => UefiColor::LightMagenta,
        Color::LightCyan => UefiColor::LightCyan,
        Color::Indexed(i) => index_to_fg(i),
        Color::Rgb(r, g, b) => rgb_to_fg(r, g, b),
    }
}

/// Backgrounds are restricted to the first 8 EFI colours by the spec, so we map
/// onto that subset only.
fn bg_color(color: Color) -> UefiColor {
    match color {
        Color::Reset | Color::Black => UefiColor::Black,
        Color::Blue | Color::LightBlue => UefiColor::Blue,
        Color::Green | Color::LightGreen => UefiColor::Green,
        Color::Cyan | Color::LightCyan => UefiColor::Cyan,
        Color::Red | Color::LightRed => UefiColor::Red,
        Color::Magenta | Color::LightMagenta => UefiColor::Magenta,
        Color::Yellow | Color::LightYellow => UefiColor::Brown,
        Color::White | Color::Gray | Color::DarkGray => UefiColor::LightGray,
        Color::Indexed(i) => index_to_bg(i),
        Color::Rgb(r, g, b) => bg_from_luma(r, g, b),
    }
}

fn index_to_fg(i: u8) -> UefiColor {
    // 16-colour ANSI slots map directly; the 216-colour cube and greyscale ramp
    // fall back to a luma-based light/dark grey.
    match i {
        0 => UefiColor::Black,
        1 => UefiColor::Red,
        2 => UefiColor::Green,
        3 => UefiColor::Yellow,
        4 => UefiColor::Blue,
        5 => UefiColor::Magenta,
        6 => UefiColor::Cyan,
        7 => UefiColor::LightGray,
        8 => UefiColor::DarkGray,
        9 => UefiColor::LightRed,
        10 => UefiColor::LightGreen,
        11 => UefiColor::Yellow,
        12 => UefiColor::LightBlue,
        13 => UefiColor::LightMagenta,
        14 => UefiColor::LightCyan,
        15 => UefiColor::White,
        232..=243 => UefiColor::DarkGray,
        _ => UefiColor::LightGray,
    }
}

fn index_to_bg(i: u8) -> UefiColor {
    match i {
        0 | 8 => UefiColor::Black,
        1 | 9 => UefiColor::Red,
        2 | 10 => UefiColor::Green,
        3 | 11 => UefiColor::Brown,
        4 | 12 => UefiColor::Blue,
        5 | 13 => UefiColor::Magenta,
        6 | 14 => UefiColor::Cyan,
        232..=243 => UefiColor::Black,
        _ => UefiColor::LightGray,
    }
}

fn rgb_to_fg(r: u8, g: u8, b: u8) -> UefiColor {
    if u16::from(r) + u16::from(g) + u16::from(b) > 600 {
        UefiColor::White
    } else if u16::from(r) + u16::from(g) + u16::from(b) < 128 {
        UefiColor::DarkGray
    } else {
        UefiColor::LightGray
    }
}

fn bg_from_luma(r: u8, g: u8, b: u8) -> UefiColor {
    if u16::from(r) + u16::from(g) + u16::from(b) > 384 {
        UefiColor::LightGray
    } else {
        UefiColor::Black
    }
}

impl Backend for UefiBackend<'_> {
    type Error = UefiBackendError;

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn window_size(&mut self) -> Result<backend::WindowSize, Self::Error> {
        Ok(WindowSize {
            columns_rows: self.size()?,
            pixels: Size::ZERO,
        })
    }

    fn size(&self) -> Result<Size, Self::Error> {
        let mode = self
            .output
            .current_mode()
            .ok()
            .flatten()
            .expect("text output always has a current mode");
        Ok(Size {
            width: mode.columns() as u16,
            height: mode.rows() as u16,
        })
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        // The text console only exposes a full clear; treat everything as such.
        match clear_type {
            ClearType::All => self.clear(),
            _ => Ok(()),
        }
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        let _ = self.output.clear();
        Ok(())
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let pos = position.into();
        let _ = self
            .output
            .set_cursor_position(pos.x as usize, pos.y as usize);
        Ok(())
    }

    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        let (column, row) = self.output.cursor_position();
        Ok((column as u16, row as u16).into())
    }

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a buffer::Cell)>,
    {
        // A run is a maximal sequence of contiguous cells on one row sharing the
        // same fg/bg. We buffer its glyphs and flush with a single reposition +
        // colour + write, instead of one triple per cell. `uefi`'s `Color` has
        // no `PartialEq`, so continuity is judged on the source ratatui colours.
        let mut run = String::new();
        let mut run_start: Position = Position::ORIGIN;
        let mut next_x: u16 = 0;
        let mut run_row: u16 = 0;
        let mut run_fg = Color::Reset;
        let mut run_bg = Color::Reset;

        for (x, y, cell) in content {
            let contiguous = !run.is_empty()
                && y == run_row
                && x == next_x
                && cell.fg == run_fg
                && cell.bg == run_bg;

            if !contiguous {
                self.flush_run(&run, run_start, run_fg, run_bg);
                run.clear();
                run_start = Position { x, y };
                run_row = y;
                run_fg = cell.fg;
                run_bg = cell.bg;
            }

            run.push_str(cell.symbol());
            next_x = x + 1;
        }

        self.flush_run(&run, run_start, run_fg, run_bg);
        Ok(())
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        let _ = self.output.enable_cursor(false);
        Ok(())
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        let _ = self.output.enable_cursor(true);
        Ok(())
    }
}

impl UefiBackend<'_> {
    fn flush_run(&mut self, run: &str, start: Position, fg: Color, bg: Color) {
        if run.is_empty() {
            return;
        }
        let _ = self
            .output
            .set_cursor_position(start.x as usize, start.y as usize);
        let _ = self.output.set_color(fg_color(fg), bg_color(bg));
        // `Output` implements `core::fmt::Write`; ignore write faults (e.g. a
        // glyph the firmware font lacks) rather than aborting the frame.
        use core::fmt::Write as _;
        let _ = self.output.write_str(run);
    }
}
