//! Pixel rectangles, the cuts the dashboard's layout is made of, and the set of rectangles a frame
//! changed.

/// A pixel count as a coordinate.
#[expect(clippy::cast_precision_loss, reason = "screen sizes are far below 2^24")]
#[must_use]
pub const fn px(v: u32) -> f32 {
    v as f32
}

/// A coordinate as a pixel count, rounded down, and zero for anything negative.
#[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, reason = "clamped to zero first")]
#[must_use]
pub fn whole_px(v: f32) -> u32 {
    v.max(0.0) as u32
}

/// A coordinate as a pixel count, rounded up, and zero for anything negative.
#[must_use]
pub fn ceil_px(v: f32) -> u32 {
    let down = whole_px(v);
    if px(down) < v { down + 1 } else { down }
}

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

/// More rectangles than this in one frame are merged, the pair that wastes least first.
const MAX_RECTS: usize = 16;

/// The parts of the screen one frame changed, merged where pushing their union costs little more
/// than pushing each.
#[derive(Debug)]
pub struct DirtyRects {
    bounds: Rect,
    rects: heapless::Vec<Rect, { MAX_RECTS + 1 }>,
}

impl DirtyRects {
    #[must_use]
    pub const fn new(bounds: Rect) -> Self {
        Self { bounds, rects: heapless::Vec::new() }
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
        // Room for one past the limit, which is then merged down.
        self.rects.push(rect).ok();
        while self.rects.len() > MAX_RECTS {
            let n = self.rects.len();
            let pairs = (0..n).flat_map(|i| (i + 1..n).map(move |j| (i, j)));
            let Some((i, j)) = pairs.min_by_key(|&(i, j)| waste(self.rects[i], self.rects[j])) else { break };
            let other = self.rects.swap_remove(j);
            self.rects[i] = self.rects[i].union(other);
        }
    }

    pub fn add_all(&mut self) {
        self.rects.clear();
        self.rects.push(self.bounds).ok();
    }

    pub fn clear(&mut self) {
        self.rects.clear();
    }

    #[must_use]
    pub fn as_slice(&self) -> &[Rect] {
        &self.rects
    }
}

/// Most bands [`changed_bands`] reports on; wider rectangles get wider bands.
pub const MAX_BANDS: usize = 64;

/// A view of pixel rows: BGRA bytes from the first pixel of a rectangle, `stride` bytes apart.
#[derive(Clone, Copy)]
pub struct Plane<'a> {
    pub data: &'a [u8],
    pub stride: usize,
}

impl Plane<'_> {
    fn row(&self, y: usize, bytes: usize) -> &[u8] {
        &self.data[y * self.stride..][..bytes]
    }
}

/// The pixels of `rect` that differ between the planes `old` and `new`, whose first pixels are
/// `rect`'s: one bounding rectangle for each vertical band of `band` pixels with a change.
#[must_use]
pub fn changed_bands(old: Plane<'_>, new: Plane<'_>, rect: Rect, band: u32) -> heapless::Vec<Rect, MAX_BANDS> {
    let w = rect.w as usize;
    let band = (band.max(1) as usize).max(w.div_ceil(MAX_BANDS));
    let bands = w.div_ceil(band);
    // Per band: leftmost and rightmost changed column, first and last changed row.
    let mut seen = [None::<(usize, usize, u32, u32)>; MAX_BANDS];
    for (row, y) in (0..rect.h as usize).zip(rect.y..) {
        let (old, new) = (old.row(row, w * 4), new.row(row, w * 4));
        if same(old, new) {
            continue;
        }
        for (b, slot) in seen.iter_mut().take(bands).enumerate() {
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
    let mut found = heapless::Vec::new();
    for (left, right, top, bottom) in seen.into_iter().take(bands).flatten() {
        let x = rect.x + u32::try_from(left).unwrap_or(0);
        let w = u32::try_from(right - left + 1).unwrap_or(0);
        // `bands` is at most the capacity.
        found.push(Rect::new(x, top, w, bottom - top + 1)).ok();
    }
    found
}

/// Whether two rows of pixels are equal. The slice `==` is a `memcmp`, which the freestanding
/// builds implement as a byte loop; this compares 64 bytes at a time.
fn same(a: &[u8], b: &[u8]) -> bool {
    let (a_blocks, a_tail) = a.as_chunks::<64>();
    let (b_blocks, b_tail) = b.as_chunks::<64>();
    let block = |x: &[u8; 64], y: &[u8; 64]| {
        let (x, y) = (x.as_chunks::<16>().0, y.as_chunks::<16>().0);
        x.iter().zip(y).fold(0u128, |diff, (x, y)| diff | (u128::from_ne_bytes(*x) ^ u128::from_ne_bytes(*y))) == 0
    };
    a_blocks.iter().zip(b_blocks).all(|(x, y)| block(x, y)) && a_tail.iter().zip(b_tail).all(|(x, y)| x == y)
}

/// Pixels of the union of `a` and `b` that neither covers.
fn waste(a: Rect, b: Rect) -> u64 {
    a.union(b).area() - (a.area() + b.area() - a.intersect(b).area())
}

/// Whether the union of `a` and `b` wastes at most an eighth of itself.
fn worth_merging(a: Rect, b: Rect) -> bool {
    waste(a, b) * 8 <= a.union(b).area()
}

#[cfg(test)]
mod tests;
