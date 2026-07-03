//! Shared helpers for the wall-clock benches. In a subdirectory so cargo's
//! bench autodiscovery doesn't treat it as a standalone target.

use std::fmt;

/// A byte size that renders as a binary unit (`4 MiB`, `2 GiB`) in divan's
/// argument labels instead of a raw byte count.
#[derive(Clone, Copy)]
pub struct Size(pub usize);

impl Size {
    #[inline]
    pub const fn bytes(self) -> usize {
        self.0
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const KIB: usize = 1 << 10;
        const MIB: usize = 1 << 20;
        const GIB: usize = 1 << 30;
        let b = self.0;
        if b >= GIB && b.is_multiple_of(GIB) {
            write!(f, "{} GiB", b / GIB)
        } else if b >= MIB && b.is_multiple_of(MIB) {
            write!(f, "{} MiB", b / MIB)
        } else if b >= KIB && b.is_multiple_of(KIB) {
            write!(f, "{} KiB", b / KIB)
        } else {
            write!(f, "{b} B")
        }
    }
}
