//! The GOP screen the dashboard is pushed to.
//!
//! Frames go by `Blt` or by writing the framebuffer directly, whichever measured faster at startup.
//! Firmware tends to leave the aperture uncacheable, where every store is one bus transaction, so
//! direct writes are also tried with the framebuffer remapped write-combining for the app's
//! lifetime. Only changed rectangles are pushed after the first frame.

use alloc::vec::Vec;
use core::arch::x86_64::{__m128i, _mm_loadu_si128, _mm_storeu_si128};
use core::fmt;
use core::mem::ManuallyDrop;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_efi::paging::{self, MtrrView, Pat, WriteCombining};
use ferrite_hal::{Clock as _, PhysAddr};
use snafu::{ResultExt as _, Snafu};
use uefi::Handle;
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams, ScopedProtocol};
use uefi::proto::console::gop::{BltOp, BltPixel, BltRegion, GraphicsOutput, Mode, ModeInfo, PixelFormat};
use uefi::proto::unsafe_protocol;

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

/// The ways of pushing a frame that calibration compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    Blt,
    Direct,
    /// Direct writes with the framebuffer remapped write-combining.
    Combined,
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
    /// The framebuffer remapped write-combining, while direct writes use it. Dropped after the
    /// screen is cleared and the mode restored.
    wc: Option<WriteCombining>,
}

impl Display {
    /// Take the first GOP exclusively, which stops the firmware's graphics console from drawing
    /// over it, and switch to the display's native mode: firmware often leaves a laptop panel in
    /// 640x480 for its text console.
    pub fn open() -> Result<Self, DisplayError> {
        let handle = boot::get_handle_for_protocol::<GraphicsOutput>().context(LocateSnafu)?;
        let mut gop = boot::open_protocol_exclusive::<GraphicsOutput>(handle).context(OpenSnafu { handle })?;
        let restore = switch_to_native(&mut gop, handle);
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
            wc: None,
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
            // `w * 4` bytes.
            unsafe { copy_row(base.add(offset), bytes, w * 4) };
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
        let layout = self.direct_layout();
        let direct = layout.and_then(|layout| timed(self, Path::Direct(layout)));
        let (fb_base, fb_size) = {
            let mut fb = self.gop.frame_buffer();
            (PhysAddr::new(fb.as_mut_ptr() as u64), fb.size() as u64)
        };
        let before = describe(fb_base);
        let combined = layout.and_then(|layout| match WriteCombining::map(fb_base, fb_size) {
            Ok(wc) => {
                self.wc = Some(wc);
                timed(self, Path::Direct(layout))
            },
            Err(e) => {
                log::info!("gop: framebuffer not remapped write-combining: {e}");
                None
            },
        });
        let choice = [(Choice::Blt, blt), (Choice::Direct, direct), (Choice::Combined, combined)]
            .into_iter()
            .filter_map(|(choice, took)| Some((choice, took?)))
            .min_by_key(|&(_, took)| took)
            .map_or(Choice::Blt, |(choice, _)| choice);
        self.path = match (choice, layout) {
            (Choice::Direct | Choice::Combined, Some(layout)) => Path::Direct(layout),
            _ => Path::Blt,
        };
        if choice != Choice::Combined {
            self.wc = None;
        }
        log::info!(
            "gop: {}x{} {:?} stride {}, framebuffer {:#x}+{fb_size:#x} ({before}); {bytes} bytes by blt {}, \
             direct {}, direct write-combining {}; pushing by {:?}{}",
            self.width,
            self.height,
            self.format,
            self.stride,
            fb_base.get(),
            Rate(bytes, blt),
            Rate(bytes, direct),
            Rate(bytes, combined),
            self.path,
            if self.wc.is_some() { " write-combining" } else { "" }
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

/// `EFI_EDID_ACTIVE_PROTOCOL`: the EDID of the display the GOP drives.
#[repr(C)]
#[unsafe_protocol("bd8c1056-9f36-44ec-92a8-a6337f817986")]
struct EdidActive {
    size: u32,
    edid: *const u8,
}

/// The display's preferred resolution, from the first detailed timing of its EDID.
fn native_resolution(handle: Handle) -> Option<(usize, usize)> {
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: read-only use of a protocol the GOP's driver installed; nothing else opens it exclusively.
    let edid = unsafe { boot::open_protocol::<EdidActive>(params, OpenProtocolAttributes::GetProtocol) }.ok()?;
    if edid.edid.is_null() {
        return None;
    }
    // SAFETY: the protocol says `size` bytes live at `edid` while the handle stays open.
    let bytes = unsafe { core::slice::from_raw_parts(edid.edid, edid.size as usize) };
    preferred_timing(bytes)
}

/// The active size of the first detailed timing descriptor of an EDID base block.
fn preferred_timing(edid: &[u8]) -> Option<(usize, usize)> {
    const HEADER: [u8; 8] = [0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0];
    let d = edid.get(54..72)?;
    // A zero pixel clock marks a display descriptor, not a timing.
    if !edid.starts_with(&HEADER) || (d[0] == 0 && d[1] == 0) {
        return None;
    }
    let width = usize::from(d[2]) | usize::from(d[4] >> 4) << 8;
    let height = usize::from(d[5]) | usize::from(d[7] >> 4) << 8;
    (width > 0 && height > 0).then_some((width, height))
}

/// Switch to the display's native mode: the EDID's preferred timing when a mode matches it, else
/// mode 0 when it is larger than the current one, since firmware lists the panel's own mode first.
/// Returns the mode that was current when it changed.
fn switch_to_native(gop: &mut GraphicsOutput, handle: Handle) -> Option<Mode> {
    let current = gop.current_mode_info();
    let modes: Vec<Mode> = gop.modes().collect();
    let native = native_resolution(handle);
    let resolutions: Vec<(usize, usize)> = modes.iter().map(|m| m.info().resolution()).collect();
    log::debug!("gop: modes {resolutions:?}, current {:?}, EDID preferred {native:?}", current.resolution());
    let area = |info: &ModeInfo| info.resolution().0 * info.resolution().1;
    let target = native
        .and_then(|res| {
            let matching = modes.iter().filter(|m| m.info().resolution() == res);
            matching.max_by_key(|m| matches!(m.info().pixel_format(), PixelFormat::Bgr | PixelFormat::Rgb))
        })
        .or_else(|| modes.first().filter(|m| area(m.info()) > area(&current)))?;
    if *target.info() == current {
        return None;
    }
    let original = modes.iter().find(|m| *m.info() == current).copied();
    match gop.set_mode(target) {
        Ok(()) => {
            log::info!("gop: mode {:?} -> {:?}", current.resolution(), target.info().resolution());
            original
        },
        Err(e) => {
            let (want, have) = (target.info().resolution(), current.resolution());
            log::warn!("gop: setting mode {want:?} failed, staying at {have:?}: {e}");
            None
        },
    }
}

/// The memory type an address has now, for the log: its page, the PAT entry it uses, the MTRRs.
fn describe(addr: PhysAddr) -> alloc::string::String {
    let pat = Pat::read();
    let mtrr = MtrrView::at(addr);
    paging::leaf(addr).map_or_else(
        || alloc::format!("not found in the page tables, {mtrr}"),
        |leaf| {
            alloc::format!(
                "{} KiB page, PAT {} ({}), {mtrr}",
                leaf.size >> 10,
                leaf.pat_index,
                pat.entry(leaf.pat_index)
            )
        },
    )
}

/// Copy `len` bytes to the framebuffer in 16-byte stores. `memcpy` here is `rep movsb`, which an
/// uncacheable aperture turns into one bus transaction per byte.
///
/// # Safety
/// `dst` and `src` are valid for `len` bytes and do not overlap; `len` is a multiple of 4.
#[expect(clippy::cast_ptr_alignment, reason = "the loadu and storeu intrinsics take unaligned pointers")]
unsafe fn copy_row(dst: *mut u8, src: *const u8, len: usize) {
    let mut i = 0;
    while i + 16 <= len {
        // SAFETY: both ranges hold 16 bytes at `i`; the unaligned forms need no alignment.
        unsafe { _mm_storeu_si128(dst.add(i).cast::<__m128i>(), _mm_loadu_si128(src.add(i).cast::<__m128i>())) };
        i += 16;
    }
    while i < len {
        // SAFETY: 4 bytes remain at `i`, since `len` is a multiple of 4.
        unsafe { dst.add(i).cast::<u32>().write_unaligned(src.add(i).cast::<u32>().read_unaligned()) };
        i += 4;
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

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::preferred_timing;

    #[test]
    fn the_preferred_timing_gives_the_native_size() {
        let mut edid = [0_u8; 128];
        edid[..8].copy_from_slice(&[0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0]);
        // 1920x1080 at 148.5 MHz: active sizes split over a low byte and a high nibble.
        edid[54..62].copy_from_slice(&[0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40]);
        check!(preferred_timing(&edid) == Some((1920, 1080)));

        edid[54..56].copy_from_slice(&[0, 0]);
        check!(preferred_timing(&edid) == None, "a display descriptor, not a timing");
        check!(preferred_timing(&edid[..60]) == None);
        check!(preferred_timing(&[0; 128]) == None, "no EDID header");
    }
}
