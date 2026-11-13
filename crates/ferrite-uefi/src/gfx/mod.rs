//! Drawing for the framebuffer dashboard: pixel rectangles and the dirty set, the RGBA canvas with
//! its shapes and text, and conversion to the framebuffer's pixel layouts.

mod canvas;
pub mod pixel;
mod rect;
pub mod text;

pub use canvas::{Canvas, Fill, Rgb};
pub use rect::{DirtyRects, Rect};
