//! A ratatui [`Backend`] over the UEFI Simple Text Output Protocol.
//!
//! Derived from `ratatuefi` (MIT OR Apache-2.0). Firmware text calls are slow,
//! so contiguous cells on one row with the same colours are written as a
//! single string, and the cursor position and colour attribute are only set
//! when they differ from what the console already has. Together with ratatui's
//! frame diffing this costs a few calls per styled run rather than per cell.
//!
//! It draws the status screen of machines without GOP, a 16-colour grid; only
//! the named colours are mapped, since that screen uses no others.

use alloc::vec::Vec;

use ratatui_core::backend::{Backend, ClearType, WindowSize};
use ratatui_core::buffer;
use ratatui_core::layout::{Position, Size};
use ratatui_core::style::Color;
use snafu::{ResultExt as _, Snafu};
use uefi::proto::console::text::{Color as UefiColor, Output};
use uefi::{CStr16, Char16, Status};

/// Columns and rows of text mode 0, which every console must support.
const MODE0_SIZE: Size = Size { width: 80, height: 25 };
const REPLACEMENT: u16 = b'?' as u16;

/// The Simple Text Output call that failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleOp {
    QueryMode,
    SetCursorPosition,
    SetColor,
    OutputString,
    Clear,
    EnableCursor,
}

#[derive(Debug, Snafu)]
#[snafu(display("console {op:?} failed: {source}"))]
pub struct BackendError {
    op: ConsoleOp,
    source: uefi::Error,
}

/// Foreground and background of a run, as the console's attribute colours.
#[derive(Clone, Copy, Debug)]
struct Attr {
    fg: UefiColor,
    bg: UefiColor,
}

impl Attr {
    fn of(cell: &buffer::Cell) -> Self {
        Self { fg: fg_color(cell.fg), bg: bg_color(cell.bg) }
    }

    fn same(self, other: Self) -> bool {
        self.fg as usize == other.fg as usize && self.bg as usize == other.bg as usize
    }
}

pub struct UefiBackend<'a> {
    output: &'a mut Output,
    /// Attribute the console is known to have, or `None` after anything that may change it.
    attr: Option<Attr>,
    /// Where the console's cursor is known to be.
    cursor: Option<Position>,
    /// The console rejected `EnableCursor` once; OVMF's does.
    cursor_toggle_unsupported: bool,
    /// Console width, read once: the text mode is not changed while we run.
    width: u16,
    /// UCS-2 text of the current run.
    text: Vec<u16>,
}

impl<'a> UefiBackend<'a> {
    pub fn new(output: &'a mut Output) -> Self {
        let mut backend = Self {
            output,
            attr: None,
            cursor: None,
            cursor_toggle_unsupported: false,
            width: MODE0_SIZE.width,
            text: Vec::new(),
        };
        backend.width = backend.console_size().map_or(MODE0_SIZE.width, |s| s.width);
        backend
    }

    fn console_size(&self) -> Result<Size, BackendError> {
        let mode = self.output.current_mode().context(BackendSnafu { op: ConsoleOp::QueryMode })?;
        Ok(mode.map_or(MODE0_SIZE, |m| Size { width: m.columns() as u16, height: m.rows() as u16 }))
    }

    /// Write the run buffered in `self.text` at `start` in `attr`.
    fn flush_run(&mut self, start: Position, attr: Attr) -> Result<(), BackendError> {
        if self.text.is_empty() {
            return Ok(());
        }
        if self.cursor != Some(start) {
            self.cursor = None;
            self.output
                .set_cursor_position(start.x.into(), start.y.into())
                .context(BackendSnafu { op: ConsoleOp::SetCursorPosition })?;
        }
        if !self.attr.is_some_and(|a| a.same(attr)) {
            self.attr = None;
            self.output.set_color(attr.fg, attr.bg).context(BackendSnafu { op: ConsoleOp::SetColor })?;
            self.attr = Some(attr);
        }
        let advance = self.text.len() as u16;
        self.text.push(0);
        // `push_ucs2` never stores a NUL, so the only one is the terminator.
        let written = match CStr16::from_u16_with_nul(&self.text) {
            Ok(text) => self.output.output_string_lossy(text).context(BackendSnafu { op: ConsoleOp::OutputString }),
            Err(_) => Ok(()),
        };
        self.text.clear();
        self.cursor = None;
        written?;
        // Reaching the last column wraps or scrolls depending on the firmware.
        let end = start.x.saturating_add(advance);
        self.cursor = (end < self.width).then_some(Position { x: end, y: start.y });
        Ok(())
    }

    /// Put the console back as firmware and the next program expect it.
    fn restore(&mut self) -> Result<(), BackendError> {
        self.attr = None;
        self.cursor = None;
        self.output
            .set_color(UefiColor::LightGray, UefiColor::Black)
            .context(BackendSnafu { op: ConsoleOp::SetColor })?;
        self.output.clear().context(BackendSnafu { op: ConsoleOp::Clear })?;
        self.set_cursor_visible(true)
    }

    /// Show or hide the cursor where the console supports it. UNSUPPORTED is
    /// not an error: failing here would abort the frame before ratatui commits it.
    fn set_cursor_visible(&mut self, visible: bool) -> Result<(), BackendError> {
        if self.cursor_toggle_unsupported {
            return Ok(());
        }
        match self.output.enable_cursor(visible) {
            Err(e) if e.status() == Status::UNSUPPORTED => {
                self.cursor_toggle_unsupported = true;
                log::debug!("ui: console cannot show or hide the cursor");
                Ok(())
            },
            result => result.context(BackendSnafu { op: ConsoleOp::EnableCursor }),
        }
    }
}

impl Drop for UefiBackend<'_> {
    fn drop(&mut self) {
        if let Err(e) = self.restore() {
            log::debug!("ui: restoring the console failed: {e}");
        }
    }
}

/// Push `symbol` as UCS-2, replacing what the console cannot represent.
fn push_ucs2(text: &mut Vec<u16>, symbol: &str) {
    text.extend(
        symbol
            .chars()
            .map(|c| Char16::try_from(c).map_or(REPLACEMENT, u16::from))
            .map(|u| if u == 0 { REPLACEMENT } else { u }),
    );
}

/// Map a ratatui foreground colour onto the 16 EFI text colours.
fn fg_color(color: Color) -> UefiColor {
    match color {
        Color::Reset | Color::White | Color::Indexed(_) | Color::Rgb(..) => UefiColor::White,
        Color::Gray => UefiColor::LightGray,
        Color::Black => UefiColor::Black,
        Color::Red => UefiColor::Red,
        Color::Green => UefiColor::Green,
        Color::Yellow | Color::LightYellow => UefiColor::Yellow,
        Color::Blue => UefiColor::Blue,
        Color::Magenta => UefiColor::Magenta,
        Color::Cyan => UefiColor::Cyan,
        Color::DarkGray => UefiColor::DarkGray,
        Color::LightRed => UefiColor::LightRed,
        Color::LightGreen => UefiColor::LightGreen,
        Color::LightBlue => UefiColor::LightBlue,
        Color::LightMagenta => UefiColor::LightMagenta,
        Color::LightCyan => UefiColor::LightCyan,
    }
}

/// Backgrounds are limited to the first 8 EFI colours.
fn bg_color(color: Color) -> UefiColor {
    match color {
        Color::Reset | Color::Black | Color::Indexed(_) | Color::Rgb(..) => UefiColor::Black,
        Color::Blue | Color::LightBlue => UefiColor::Blue,
        Color::Green | Color::LightGreen => UefiColor::Green,
        Color::Cyan | Color::LightCyan => UefiColor::Cyan,
        Color::Red | Color::LightRed => UefiColor::Red,
        Color::Magenta | Color::LightMagenta => UefiColor::Magenta,
        Color::Yellow | Color::LightYellow => UefiColor::Brown,
        Color::White | Color::Gray | Color::DarkGray => UefiColor::LightGray,
    }
}

impl Backend for UefiBackend<'_> {
    type Error = BackendError;

    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }

    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        Ok(WindowSize { columns_rows: self.console_size()?, pixels: Size::ZERO })
    }

    fn size(&self) -> Result<Size, Self::Error> {
        self.console_size()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        // The text console only offers a full clear.
        match clear_type {
            ClearType::All => self.clear(),
            _ => Ok(()),
        }
    }

    fn clear(&mut self) -> Result<(), Self::Error> {
        self.cursor = None;
        self.output.clear().context(BackendSnafu { op: ConsoleOp::Clear })?;
        self.cursor = Some(Position::ORIGIN);
        Ok(())
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        let pos = position.into();
        self.cursor = None;
        self.output
            .set_cursor_position(pos.x.into(), pos.y.into())
            .context(BackendSnafu { op: ConsoleOp::SetCursorPosition })?;
        self.cursor = Some(pos);
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
        let mut start = Position::ORIGIN;
        let mut attr = Attr { fg: UefiColor::White, bg: UefiColor::Black };
        let mut cells: u16 = 0;

        for (x, y, cell) in content {
            let cell_attr = Attr::of(cell);
            let contiguous = cells > 0 && y == start.y && x == start.x + cells && cell_attr.same(attr);
            if !contiguous {
                self.flush_run(start, attr)?;
                start = Position { x, y };
                attr = cell_attr;
                cells = 0;
            }
            push_ucs2(&mut self.text, cell.symbol());
            cells += 1;
        }
        self.flush_run(start, attr)
    }

    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.set_cursor_visible(false)
    }

    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.set_cursor_visible(true)
    }
}
