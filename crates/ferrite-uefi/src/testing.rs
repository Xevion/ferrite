//! Test support shared by the modules: the test binary's allocator, counting the heap allocations
//! (and reallocations) of each thread.

pub use ferrite_testing::counting::{allocations, count};

#[global_allocator]
static ALLOCATOR: ferrite_testing::counting::Counting = ferrite_testing::counting::Counting;

/// The memory map of a machine with 18 GiB of RAM part way through a test: the firmware's RAM at the
/// ends and a gap for devices below 4 GiB, the sweep's `done` cells tested, the next one active and
/// the rest pending.
pub fn sample_map(done: usize) -> crate::sweep::MapView {
    use crate::memmap::CellState::{Active, Done, Firmware, NoRam, Pending};

    let mut cells = [Pending; crate::dash::CELLS];
    cells[0] = Firmware;
    cells[24..29].fill(NoRam);
    cells[crate::dash::CELLS - 8..].fill(Firmware);
    let mut left = done;
    for cell in cells.iter_mut().filter(|c| **c == Pending) {
        if left == 0 {
            *cell = Active;
            break;
        }
        *cell = Done;
        left -= 1;
    }
    crate::sweep::MapView { top: 18 << 30, cells }
}
