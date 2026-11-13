//! The GOP screen the dashboard is pushed to.
//!
//! Frames go by `Blt` or by writing the framebuffer directly, whichever measured faster at startup.
//! Only changed rectangles are pushed after the first frame: a framebuffer aperture may be
//! uncached, and a whole 4K frame can then take tens of milliseconds.

use alloc::vec::Vec;
use core::fmt;
use core::mem::ManuallyDrop;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_hal::Clock as _;
use snafu::{ResultExt as _, Snafu};
use uefi::Handle;
use uefi::boot::{self, ScopedProtocol};
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput, Mode, ModeInfo, PixelFormat};

use crate::gfx::pixel::{self, Layout};
use crate::gfx::{Canvas, Rect};

#[derive(Debug, Snafu)]
pub enum DisplayError {
    #[snafu(display("locating GOP: {source}"))]
    Locate { source: uefi::Error },
    #[snafu(display("opening GOP {handle:?} exclusively: {source}"))]
    Open { handle: Handle, source: uefi::Error },
    #[snafu(display("GOP Blt of {rect:?} failed: {source}"))]
    Blt { rect: Rect, source: uefi::Error },
}

/// How frames reach the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Blt,
    /// Rows copied into the framebuffer, converted to its layout.
    Direct(Layout),
}

/// Rows pushed by each path to time it.
const CALIBRATION_ROWS: u32 = 64;

pub struct Display {
    /// Dropped by hand before the firmware's console is reconnected.
    gop: ManuallyDrop<ScopedProtocol<GraphicsOutput>>,
    handle: Handle,
    /// The mode the firmware had, put back on drop when another was set.
    restore: Option<Mode>,
    width: u32,
    height: u32,
    /// Framebuffer pixels per scan line.
    stride: usize,
    format: PixelFormat,
    path: Path,
    /// One converted row, for framebuffers in RGB order.
    row: Vec<u32>,
}

impl Display {
    /// Take the first GOP exclusively, which stops the firmware's graphics console from drawing
    /// over it, and switch to its largest mode: firmware often leaves a laptop panel in 640x480
    /// for its text console.
    pub fn open() -> Result<Self, DisplayError> {
        let handle = boot::get_handle_for_protocol::<GraphicsOutput>().context(LocateSnafu)?;
        let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).context(OpenSnafu { handle })?;
        let restore = switch_to_largest(&mut gop);
        let info = gop.current_mode_info();
        let (width, height) = info.resolution();
        Ok(Self {
            gop: ManuallyDrop::new(gop),
            handle,
            restore,
            width: u32::try_from(width).unwrap_or(0),
            height: u32::try_from(height).unwrap_or(0),
            stride: info.stride(),
            format: info.pixel_format(),
            path: Path::Blt,
            row: Vec::new(),
        })
    }

    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Framebuffer layout for direct writes, when the mode has one this code writes.
    const fn direct_layout(&self) -> Option<Layout> {
        match self.format {
            PixelFormat::Bgr => Some(Layout::Bgrx),
            PixelFormat::Rgb => Some(Layout::Rgbx),
            PixelFormat::Bitmask | PixelFormat::BltOnly => None,
        }
    }

    /// Push `rects` of `canvas`, which is the screen's size.
    pub fn present(&mut self, canvas: &Canvas, rects: &[Rect]) -> Result<(), DisplayError> {
        for &rect in rects {
            let rect = rect.intersect(canvas.bounds()).intersect(Rect::new(0, 0, self.width, self.height));
            if rect.is_empty() {
                continue;
            }
            match self.path {
                Path::Blt => self.blt(canvas, rect)?,
                Path::Direct(layout) => self.write(canvas, rect, layout),
            }
        }
        Ok(())
    }

    fn blt(&mut self, canvas: &Canvas, rect: Rect) -> Result<(), DisplayError> {
        let (x, y, w, h) = (rect.x as usize, rect.y as usize, rect.w as usize, rect.h as usize);
        self.gop
            .blt(BltOp::BufferToVideo {
                buffer: pixel::as_blt(canvas.bgra()),
                src: BltRegion::SubRectangle { coords: (x, y), px_stride: canvas.bounds().w as usize },
                dest: (x, y),
                dims: (w, h),
            })
            .context(BltSnafu { rect })
    }

    fn write(&mut self, canvas: &Canvas, rect: Rect, layout: Layout) {
        let (x, w) = (rect.x as usize, rect.w as usize);
        let src_stride = canvas.bounds().w as usize * 4;
        let mut fb = self.gop.frame_buffer();
        let (base, size) = (fb.as_mut_ptr(), fb.size());
        self.row.resize(w, 0);
        for y in rect.y as usize..rect.bottom() as usize {
            let offset = (y * self.stride + x) * 4;
            if offset + w * 4 > size {
                break;
            }
            let src = &canvas.bgra()[y * src_stride + x * 4..][..w * 4];
            let bytes = match layout {
                // The canvas is already in this order; its opaque alpha lands in the padding byte.
                Layout::Bgrx => src.as_ptr(),
                Layout::Rgbx => {
                    pixel::convert(src, &mut self.row, layout);
                    self.row.as_ptr().cast::<u8>()
                },
            };
            // SAFETY: the row is inside the framebuffer (checked against its size) and the source has
            // `w * 4` bytes. Plain stores suffice: nothing reads the framebuffer back.
            unsafe { core::ptr::copy_nonoverlapping(bytes, base.add(offset), w * 4) };
        }
    }

    /// Time a push of the top rows by each path the mode allows, log the rates, and keep the
    /// faster. A strip rather than a frame, since an uncached aperture writes at a few MB/s.
    pub fn calibrate(&mut self, canvas: &Canvas, clock: &Clock) {
        let bounds = canvas.bounds();
        let full = [Rect::new(bounds.x, bounds.y, bounds.w, bounds.h.min(CALIBRATION_ROWS))];
        let bytes = full[0].area() * 4;
        let timed = |display: &mut Self, path| {
            display.path = path;
            let start = clock.now();
            match display.present(canvas, &full) {
                Ok(()) => Some(clock.now() - start),
                Err(e) => {
                    log::warn!("gop: a full frame by {path:?} failed: {e}");
                    None
                },
            }
        };
        let blt = timed(self, Path::Blt);
        let direct = self.direct_layout().and_then(|layout| timed(self, Path::Direct(layout)));
        self.path = match (blt, direct, self.direct_layout()) {
            (Some(b), Some(d), Some(layout)) if d < b => Path::Direct(layout),
            (None, Some(_), Some(layout)) => Path::Direct(layout),
            _ => Path::Blt,
        };
        log::info!(
            "gop: {}x{} {:?} stride {}, {bytes} bytes by blt {}, direct {}; pushing by {:?}",
            self.width,
            self.height,
            self.format,
            self.stride,
            Rate(bytes, blt),
            Rate(bytes, direct),
            self.path
        );
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let fill = BltOp::VideoFill {
            color: BltPixel::new(0, 0, 0),
            dest: (0, 0),
            dims: (self.width as usize, self.height as usize),
        };
        if let Err(e) = self.gop.blt(fill) {
            log::debug!("gop: clearing the screen failed: {e}");
        }
        if let Some(mode) = self.restore
            && let Err(e) = self.gop.set_mode(&mode)
        {
            log::warn!("gop: restoring the firmware's mode {:?} failed: {e}", mode.info().resolution());
        }
        // SAFETY: `gop` is not touched again.
        unsafe { ManuallyDrop::drop(&mut self.gop) };
        if let Err(e) = boot::connect_controller(self.handle, &[], None, true) {
            log::debug!("gop: reconnecting the firmware console to {:?} failed: {e}", self.handle);
        }
    }
}

/// Set the mode with the most pixels, preferring one the framebuffer can be written in directly.
/// Returns the mode that was current when it changed.
fn switch_to_largest(gop: &mut GraphicsOutput) -> Option<Mode> {
    let current = gop.current_mode_info();
    let modes: Vec<Mode> = gop.modes().collect();
    let rank = |info: &ModeInfo| {
        let (w, h) = info.resolution();
        (w * h, matches!(info.pixel_format(), PixelFormat::Bgr | PixelFormat::Rgb))
    };
    let resolutions: Vec<(usize, usize)> = modes.iter().map(|m| m.info().resolution()).collect();
    log::debug!("gop: modes {resolutions:?}, current {:?}", current.resolution());
    let original = modes.iter().find(|m| *m.info() == current).copied();
    let best = modes.iter().max_by_key(|m| rank(m.info()))?;
    if rank(best.info()) <= rank(&current) {
        return None;
    }
    match gop.set_mode(best) {
        Ok(()) => {
            log::info!("gop: mode {:?} -> {:?}", current.resolution(), best.info().resolution());
            original
        },
        Err(e) => {
            log::warn!(
                "gop: setting mode {:?} failed, staying at {:?}: {e}",
                best.info().resolution(),
                current.resolution()
            );
            None
        },
    }
}

/// A push's time and rate, or that the path was not available.
struct Rate(u64, Option<Duration>);

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(bytes, Some(took)) = *self else { return f.write_str("n/a") };
        let micros = took.as_micros().max(1);
        // Bytes per microsecond is MB/s.
        write!(f, "{}.{:02} ms ({} MB/s)", micros / 1000, micros % 1000 / 10, u128::from(bytes) / micros)
    }
}
