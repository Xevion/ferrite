//! The memory map: what each stretch of physical address space is, and where the sweep is working.
//!
//! The axis runs from address 0 to the top of the RAM the firmware's memory map reports, fixed when
//! the sweep starts and cut into `N` equal cells. Under UEFI's identity map a pointer is its
//! physical address, so a word the sweep touches lands in the cell its address says. Each cell shows
//! one [`CellState`]; a cell that is partly the sweep's memory counts as the sweep's, and is
//! [`CellState::Done`] only once every byte of it that the sweep holds is behind a front.
//!
//! The pieces: [`MemMap`] is the ground (which cells have RAM), [`FaultCells`] the cells a fault
//! has been seen in (kept by the sweep's tally), and [`MemMap::view`] reads both with a [`Scene`],
//! the run's memory and where each core's test is in it, into a [`View`]. Cores report fronts in
//! words of the run's layout; [`Segment`] translates those to physical extents. Groups of cells
//! labelled by DIMM or channel would be bands of extents handed to the same cell arithmetic.

use ferrite_efi::memory::ranges::Range;
use ferrite_hal::PhysAddr;
use ferrite_mem::suite::{Activity, Front, Span};
use ferrite_mem::{CpuAddr, Order};

const WORD_BYTES: u64 = size_of::<u64>() as u64;

/// What one cell of the map shows, from the least to the most pressing: a cell with several things
/// going on shows the greatest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum CellState {
    /// No RAM answers at these addresses: the gap below 4 GiB, say.
    NoRam,
    /// RAM the sweep does not hold now: the firmware's own, the reserve between rotations, and the
    /// block lent to the firmware during one.
    Firmware,
    /// The sweep's memory, ahead of the front in the current phase.
    Pending,
    /// The sweep's memory, behind the front in the current phase.
    Done,
    /// The sweep's memory, left untouched while a bit-fade wait lets it hold its pattern.
    Holding,
    /// A core is working here: its head is in the cell, or a block-move window is.
    Active,
    /// A fault has been seen at an address in the cell since the run began.
    Fault,
}

impl CellState {
    /// The one character the status document writes for the state: `n` no RAM, `f` firmware,
    /// `p` pending, `t` tested (done), `h` holding, `a` active, `x` fault.
    #[must_use]
    pub const fn code(self) -> char {
        match self {
            Self::NoRam => 'n',
            Self::Firmware => 'f',
            Self::Pending => 'p',
            Self::Done => 't',
            Self::Holding => 'h',
            Self::Active => 'a',
            Self::Fault => 'x',
        }
    }
}

/// A run of physical memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    start: PhysAddr,
    bytes: u64,
}

impl Extent {
    #[must_use]
    pub const fn new(start: PhysAddr, bytes: u64) -> Self {
        Self { start, bytes }
    }

    #[must_use]
    pub const fn start(self) -> PhysAddr {
        self.start
    }

    #[must_use]
    pub const fn bytes(self) -> u64 {
        self.bytes
    }

    /// One past the last byte.
    #[must_use]
    pub const fn end(self) -> u64 {
        self.start.get().saturating_add(self.bytes)
    }

    /// Whether the byte at physical address `address` is in the run.
    #[must_use]
    pub const fn contains(self, address: u64) -> bool {
        address >= self.start.get() && address < self.end()
    }
}

impl From<Range> for Extent {
    fn from(range: Range) -> Self {
        Self::new(range.start, range.bytes())
    }
}

/// Where a stretch of the run's words lies in physical memory: the run's regions are laid end to
/// end in word space, each one a block of consecutive addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    words: Span,
    base: PhysAddr,
}

impl Segment {
    /// The block at `base` holding the run's `words`.
    #[must_use]
    pub const fn new(words: Span, base: PhysAddr) -> Self {
        Self { words, base }
    }

    /// The memory of the words of `span` that this segment holds.
    fn extent(&self, span: Span) -> Option<Extent> {
        let part = span.clip(self.words);
        if part.words == 0 {
            return None;
        }
        let from = self.base.checked_add((part.start - self.words.start) as u64 * WORD_BYTES)?;
        Some(Extent::new(from, part.words as u64 * WORD_BYTES))
    }
}

/// The memory of the words of `span`, one extent for each segment they touch.
fn extents(segments: &[Segment], span: Span) -> impl Iterator<Item = Extent> + '_ {
    segments.iter().filter_map(move |s| s.extent(span))
}

/// Address 0 to `top`, cut into `N` equal cells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Axis<const N: usize> {
    top: u64,
}

impl<const N: usize> Axis<N> {
    #[must_use]
    pub const fn new(top: u64) -> Self {
        Self { top }
    }

    #[must_use]
    pub const fn top(self) -> u64 {
        self.top
    }

    /// Bytes in a cell, rounded down.
    #[must_use]
    pub const fn cell_bytes(self) -> u64 {
        self.top / N as u64
    }

    /// The cell holding `addr`; `None` at or past the top. Cell `i` begins at the first address
    /// `a` with `a * N >= i * top`, so this is exact rather than off by one at a cell's edge.
    fn cell_of(self, addr: u64) -> Option<usize> {
        (addr < self.top).then(|| (u128::from(addr) * N as u128 / u128::from(self.top)) as usize)
    }

    /// First address of cell `i`; the top for the cell after the last.
    fn edge(self, i: usize) -> u64 {
        (u128::from(self.top) * i as u128).div_ceil(N as u128) as u64
    }

    /// The cells `extent` touches, each with the bytes of it inside.
    fn cells(self, extent: Extent) -> impl Iterator<Item = (usize, u64)> {
        let end = extent.end().min(self.top);
        let start = extent.start.get();
        let first_last = || Some((self.cell_of(start)?, self.cell_of(end.checked_sub(1)?)?));
        let touched = if start < end { first_last() } else { None };
        touched
            .into_iter()
            .flat_map(|(first, last)| first..=last)
            .map(move |i| (i, end.min(self.edge(i + 1)) - start.max(self.edge(i))))
    }
}

/// The cells a fault has been seen in, kept for the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultCells<const N: usize> {
    axis: Axis<N>,
    hit: [bool; N],
}

impl<const N: usize> Default for FaultCells<N> {
    fn default() -> Self {
        Self::new(Axis::default())
    }
}

impl<const N: usize> FaultCells<N> {
    #[must_use]
    pub const fn new(axis: Axis<N>) -> Self {
        Self { axis, hit: [false; N] }
    }

    /// Note a fault at `addr`; one outside the axis has no cell to show in.
    pub fn note(&mut self, addr: CpuAddr) {
        if let Some(i) = self.axis.cell_of(addr.get() as u64) {
            self.hit[i] = true;
        }
    }

    /// The same cells with no fault seen, for a restarted run.
    #[must_use]
    pub const fn cleared(&self) -> Self {
        Self::new(self.axis)
    }
}

/// One core's share of the run, and where its test is in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreFront {
    /// The words of the run the core works on.
    pub share: Span,
    /// The phase the core is in; `None` between phases, before the first and after the last.
    pub front: Option<Front>,
    /// Whether the core has done any work in this test.
    pub began: bool,
}

/// What the map shows now.
#[derive(Clone, Copy, Debug)]
pub struct Scene<'a> {
    /// Where the run's words lie in memory.
    pub segments: &'a [Segment],
    /// Each core's front; a test that is not split has one core with the whole run as its share.
    pub cores: &'a [CoreFront],
    /// The sweep's memory that is not in the run now but is finished for the pass: the main memory
    /// while the reserve is under test.
    pub aside: &'a [Extent],
}

/// The cells' states at one moment, with the axis they are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View<const N: usize> {
    /// The top of the axis in bytes; 0 when there is no map.
    pub top: u64,
    pub cells: [CellState; N],
}

impl<const N: usize> View<N> {
    pub const NONE: Self = Self { top: 0, cells: [CellState::NoRam; N] };

    /// Bytes in a cell, rounded down.
    #[must_use]
    pub const fn cell_bytes(&self) -> u64 {
        Axis::<N>::new(self.top).cell_bytes()
    }

    /// The cells' state characters in order, as [`CellState::code`] gives them.
    pub fn codes(&self) -> impl Iterator<Item = char> + '_ {
        self.cells.iter().map(|c| c.code())
    }

    /// Whether any cell is in `state`.
    #[must_use]
    pub fn has(&self, state: CellState) -> bool {
        self.cells.contains(&state)
    }
}

/// The ground of the map: the axis and which cells have RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemMap<const N: usize> {
    axis: Axis<N>,
    ram: [bool; N],
}

impl<const N: usize> MemMap<N> {
    /// The map of the RAM in `ram`, to the top of the highest.
    #[must_use]
    pub fn new(ram: &[Extent]) -> Self {
        let axis = Axis::new(ram.iter().map(|e| e.end()).max().unwrap_or(0));
        let mut cells = [false; N];
        for &extent in ram {
            for (i, _) in axis.cells(extent) {
                cells[i] = true;
            }
        }
        Self { axis, ram: cells }
    }

    #[must_use]
    pub const fn axis(&self) -> Axis<N> {
        self.axis
    }

    /// A set of fault cells on this map's axis.
    #[must_use]
    pub const fn faults(&self) -> FaultCells<N> {
        FaultCells::new(self.axis)
    }

    /// Each cell's state under `scene`, by priority: a fault seen in it, a core's head or window
    /// in it, a bit-fade hold over it, all the sweep's memory in it behind a front, the sweep's
    /// memory in it ahead of the fronts, RAM the sweep does not hold, no RAM.
    ///
    /// Memory the scene sets aside shows as done, and a core with no front shows its whole share as
    /// done once it has begun and as pending before. Faults count only on the map's own axis.
    #[must_use]
    pub fn view(&self, scene: &Scene<'_>, faults: &FaultCells<N>) -> View<N> {
        let axis = self.axis;
        let mut state = self.ram.map(|ram| if ram { CellState::Firmware } else { CellState::NoRam });
        for &extent in scene.aside {
            for (i, _) in axis.cells(extent) {
                state[i] = state[i].max(CellState::Done);
            }
        }
        let mut cover = Cover::<N>::default();
        for segment in scene.segments {
            if let Some(extent) = segment.extent(segment.words) {
                for (i, bytes) in axis.cells(extent) {
                    cover.ours[i] += bytes;
                }
            }
        }
        for core in scene.cores {
            marks(core, |span, mark| {
                for extent in extents(scene.segments, span) {
                    for (i, bytes) in axis.cells(extent) {
                        match mark {
                            Mark::Done => cover.done[i] += bytes,
                            Mark::Active => cover.active[i] = true,
                            Mark::Held => cover.held[i] = true,
                        }
                    }
                }
            });
        }
        let seen = faults.axis == axis;
        let cells = core::array::from_fn(|i| {
            let (ours, done) = (cover.ours[i], cover.done[i]);
            if seen && faults.hit[i] {
                CellState::Fault
            } else if cover.active[i] {
                CellState::Active
            } else if cover.held[i] {
                CellState::Holding
            } else if ours > 0 {
                if done >= ours { CellState::Done } else { CellState::Pending }
            } else {
                state[i]
            }
        });
        View { top: axis.top, cells }
    }
}

/// Per cell, the bytes of the sweep's memory in it and how much of that is behind a front, and
/// whether a core is working or a wait is holding in it.
struct Cover<const N: usize> {
    ours: [u64; N],
    done: [u64; N],
    active: [bool; N],
    held: [bool; N],
}

impl<const N: usize> Default for Cover<N> {
    fn default() -> Self {
        Self { ours: [0; N], done: [0; N], active: [false; N], held: [false; N] }
    }
}

#[derive(Clone, Copy)]
enum Mark {
    Done,
    Active,
    Held,
}

/// What a core's front says about its share, as spans of the run's words with what each is.
fn marks(core: &CoreFront, mut mark: impl FnMut(Span, Mark)) {
    let CoreFront { share, front, began } = *core;
    let Some(front) = front else {
        if began {
            mark(share, Mark::Done);
        }
        return;
    };
    let mut sweep = |front: Front| {
        mark(front.done(), Mark::Done);
        if let Some(head) = front.head() {
            mark(Span { start: head, words: 1 }, Mark::Active);
        }
    };
    match front.activity {
        Activity::Hold => mark(front.span, Mark::Held),
        Activity::Window => {
            mark(front.span, Mark::Active);
            mark(behind(share, front.span, Order::Ascending), Mark::Done);
        },
        Activity::Sweep { order, .. } => {
            sweep(front);
            mark(behind(share, front.span, order), Mark::Done);
        },
        Activity::Bus { .. } => {
            sweep(front);
            front.check_stream().into_iter().for_each(sweep);
        },
    }
}

/// The words of `share` that a walk of `span` in `order` has passed before it gets to its span:
/// windows ascend, so the share's words below one are done and those above are not.
fn behind(share: Span, span: Span, order: Order) -> Span {
    match order {
        Order::Ascending => Span { start: share.start, words: span.start.saturating_sub(share.start).min(share.words) },
        Order::Descending => {
            let from = span.end().clamp(share.start, share.end());
            Span { start: from, words: share.end() - from }
        },
    }
}

#[cfg(test)]
mod tests;
