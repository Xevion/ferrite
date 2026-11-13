//! Pixel rectangles, the cuts the dashboard's layout is made of, and the set of rectangles a frame
//! changed.

use alloc::vec::Vec;

/// A rectangle of whole pixels; empty when either side is zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

#[expect(clippy::return_self_not_must_use, reason = "a cut is also made to skip a gap, dropping the piece")]
impl Rect {
    #[must_use]
    pub const fn new(x: u32, y: u32, w: u32, h: u32) -> Self {
        Self { x, y, w, h }
    }

    #[must_use]
    pub const fn right(self) -> u32 {
        self.x + self.w
    }

    #[must_use]
    pub const fn bottom(self) -> u32 {
        self.y + self.h
    }

    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    #[must_use]
    pub const fn area(self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// The smallest rectangle holding both; an empty side does not count.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let (x, y) = (self.x.min(other.x), self.y.min(other.y));
        Self::new(x, y, self.right().max(other.right()) - x, self.bottom().max(other.bottom()) - y)
    }

    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        let (x, y) = (self.x.max(other.x), self.y.max(other.y));
        let (right, bottom) = (self.right().min(other.right()), self.bottom().min(other.bottom()));
        if right <= x || bottom <= y { Self::default() } else { Self::new(x, y, right - x, bottom - y) }
    }

    /// Shrunk by `dx` on the left and right and `dy` on the top and bottom, down to nothing.
    #[must_use]
    pub fn inset(self, dx: u32, dy: u32) -> Self {
        let w = self.w.saturating_sub(dx.saturating_mul(2));
        let h = self.h.saturating_sub(dy.saturating_mul(2));
        Self::new(self.x + dx.min(self.w / 2), self.y + dy.min(self.h / 2), w, h)
    }

    /// Cut `h` pixels off the top (all of it when shorter) and return them.
    pub fn take_top(&mut self, h: u32) -> Self {
        let h = h.min(self.h);
        let top = Self::new(self.x, self.y, self.w, h);
        self.y += h;
        self.h -= h;
        top
    }

    pub fn take_bottom(&mut self, h: u32) -> Self {
        let h = h.min(self.h);
        self.h -= h;
        Self::new(self.x, self.y + self.h, self.w, h)
    }

    pub fn take_left(&mut self, w: u32) -> Self {
        let w = w.min(self.w);
        let left = Self::new(self.x, self.y, w, self.h);
        self.x += w;
        self.w -= w;
        left
    }

    pub fn take_right(&mut self, w: u32) -> Self {
        let w = w.min(self.w);
        self.w -= w;
        Self::new(self.x + self.w, self.y, w, self.h)
    }
}

/// More rectangles than this in one frame are pushed as their bounding box.
const MAX_RECTS: usize = 16;

/// The parts of the screen one frame changed, merged where pushing their union costs little more
/// than pushing each.
#[derive(Debug)]
pub struct DirtyRects {
    bounds: Rect,
    rects: Vec<Rect>,
}

impl DirtyRects {
    #[must_use]
    pub const fn new(bounds: Rect) -> Self {
        Self { bounds, rects: Vec::new() }
    }

    /// Mark `rect`, clipped to the screen.
    pub fn add(&mut self, rect: Rect) {
        let mut rect = rect.intersect(self.bounds);
        if rect.is_empty() {
            return;
        }
        while let Some(i) = self.rects.iter().position(|&other| worth_merging(other, rect)) {
            rect = rect.union(self.rects.swap_remove(i));
        }
        self.rects.push(rect);
        if self.rects.len() > MAX_RECTS {
            let all = self.rects.drain(..).fold(Rect::default(), Rect::union);
            self.rects.push(all);
        }
    }

    /// Mark the pixels of `rect` that differ between `before`, its previous rows packed, and
    /// `after`, the whole canvas in rows of `stride` pixels: one bounding rectangle for each
    /// vertical band of `band` pixels with a change.
    pub fn add_changes(&mut self, before: &[u8], after: &[u8], stride: u32, rect: Rect, band: u32) {
        let (w, band) = (rect.w as usize, band.max(1) as usize);
        let bands = w.div_ceil(band);
        // Per band: leftmost and rightmost changed column, first and last changed row.
        let mut seen: Vec<Option<(usize, usize, u32, u32)>> = alloc::vec![None; bands];
        for (y, old) in (rect.y..rect.bottom()).zip(before.chunks_exact(w * 4)) {
            let start = (y as usize * stride as usize + rect.x as usize) * 4;
            let new = &after[start..start + w * 4];
            if old == new {
                continue;
            }
            for (b, slot) in seen.iter_mut().enumerate() {
                let (lo, hi) = (b * band, ((b + 1) * band).min(w));
                let (old, new) = (&old[lo * 4..hi * 4], &new[lo * 4..hi * 4]);
                let differs = |(o, n): (&[u8; 4], &[u8; 4])| o != n;
                let pairs = || old.as_chunks::<4>().0.iter().zip(new.as_chunks::<4>().0);
                let Some(first) = pairs().position(differs) else { continue };
                let last = hi - lo - 1 - pairs().rev().position(differs).unwrap_or(0);
                let (l, r) = (lo + first, lo + last);
                *slot = Some(slot.map_or((l, r, y, y), |(l0, r0, top, _)| (l0.min(l), r0.max(r), top, y)));
            }
        }
        for (left, right, top, bottom) in seen.into_iter().flatten() {
            let x = rect.x + u32::try_from(left).unwrap_or(0);
            let w = u32::try_from(right - left + 1).unwrap_or(0);
            self.add(Rect::new(x, top, w, bottom - top + 1));
        }
    }

    pub fn add_all(&mut self) {
        self.rects.clear();
        self.rects.push(self.bounds);
    }

    pub fn clear(&mut self) {
        self.rects.clear();
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Rect] {
        &self.rects
    }
}

/// Whether the union of `a` and `b` wastes at most an eighth of itself on pixels neither covers.
fn worth_merging(a: Rect, b: Rect) -> bool {
    let union = a.union(b).area();
    let covered = a.area() + b.area() - a.intersect(b).area();
    (union - covered) * 8 <= union
}

#[cfg(test)]
mod tests;
