//! The canvas's pixels in the layouts a GOP framebuffer or `Blt` takes.
//!
//! The canvas is kept in BGRA byte order, which is already the `Blt` pixel and the common BGR
//! framebuffer layout, so only an RGB framebuffer needs a conversion per pixel.

use uefi::proto::console::gop::BltPixel;

/// A 32-bit framebuffer pixel's byte order in memory, the padding byte last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    Bgrx,
    Rgbx,
}

/// Convert a row of canvas pixels into `dst` as `layout`.
pub fn convert(src: &[u8], dst: &mut [u32], layout: Layout) {
    let pixels = src.as_chunks::<4>().0.iter().zip(dst);
    match layout {
        Layout::Bgrx => pixels.for_each(|(p, out)| *out = u32::from_le_bytes([p[0], p[1], p[2], 0])),
        Layout::Rgbx => pixels.for_each(|(p, out)| *out = u32::from_le_bytes([p[2], p[1], p[0], 0])),
    }
}

/// Canvas pixels as `Blt` pixels, without copying; a trailing partial pixel is left out.
#[must_use]
pub fn as_blt(src: &[u8]) -> &[BltPixel] {
    const { assert!(size_of::<BltPixel>() == 4 && align_of::<BltPixel>() == 1) };
    // SAFETY: `BltPixel` is `repr(C)` of four `u8`s (blue, green, red, reserved), so any four bytes
    // are a valid one at any alignment, and the length stays inside `src`.
    unsafe { core::slice::from_raw_parts(src.as_ptr().cast::<BltPixel>(), src.len() / 4) }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    /// Two pixels in canvas order: blue, green, red, alpha.
    const ROW: [u8; 8] = [0x33, 0x22, 0x11, 0xff, 0xc0, 0xb0, 0xa0, 0xff];

    #[test]
    fn rgbx_swaps_red_and_blue_and_clears_padding() {
        let mut out = [0u32; 2];
        convert(&ROW, &mut out, Layout::Rgbx);
        check!(out.map(u32::to_le_bytes) == [[0x11, 0x22, 0x33, 0], [0xa0, 0xb0, 0xc0, 0]]);
        convert(&ROW, &mut out, Layout::Bgrx);
        check!(out.map(u32::to_le_bytes) == [[0x33, 0x22, 0x11, 0], [0xc0, 0xb0, 0xa0, 0]]);
    }

    #[test]
    fn blt_view_reads_canvas_order() {
        let blt = as_blt(&ROW[..7]);
        check!(blt.len() == 1);
        check!((blt[0].red, blt[0].green, blt[0].blue) == (0x11, 0x22, 0x33));
    }
}
