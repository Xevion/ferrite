//! Drawing for the framebuffer dashboard: pixel rectangles and the dirty set, the RGBA canvas with
//! its shapes and text, and conversion to the framebuffer's pixel layouts.

mod canvas;
pub mod pixel;
pub mod rect;
pub mod text;

pub use canvas::{Canvas, Fill, Recording, Rgb};
pub use rect::{DirtyRects, Rect, ceil_px, px, whole_px};
