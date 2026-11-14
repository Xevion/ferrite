use std::vec::Vec;

use assert2::check;
use ferrite_mem::CpuAddr;
use rstest::rstest;

use super::CellState::{Active, Done, Firmware, Holding, NoRam, Pending};
use super::*;

const CELL: u64 = 1 << 20;
/// Words in a cell.
const CW: usize = (CELL / WORD_BYTES) as usize;
const N: usize = 8;

fn mib(n: u64) -> PhysAddr {
    PhysAddr::new(n * CELL)
}

/// Eight cells of a MiB: RAM in cells 0 to 2 and 4 to 7, a hole at 3.
fn map() -> MemMap<N> {
    MemMap::new(&[Extent::new(mib(0), 3 * CELL), Extent::new(mib(4), 4 * CELL)])
}

/// The run: a block at cells 1 and 2, then a block at cells 4 to 6, as `5 * CW` words.
fn run() -> [Segment; 2] {
    [
        Segment::new(Span { start: 0, words: 2 * CW }, mib(1)),
        Segment::new(Span { start: 2 * CW, words: 3 * CW }, mib(4)),
    ]
}

fn words(start: usize, words: usize) -> Span {
    Span { start, words }
}

fn core(share: Span, activity: Option<(Span, Activity)>) -> CoreFront {
    let front = activity.map(|(span, activity)| Front { span, activity });
    CoreFront { share, front, began: front.is_some() }
}

fn up(done: usize) -> Activity {
    Activity::Sweep { order: Order::Ascending, done }
}

fn show(cores: &[CoreFront], faults: &FaultCells<N>) -> [CellState; N] {
    let scene = Scene { segments: &run(), cores, aside: &[] };
    map().view(&scene, faults).cells
}

fn shown(cores: &[CoreFront]) -> [CellState; N] {
    show(cores, &map().faults())
}

const ALL: Span = Span { start: 0, words: 5 * CW };

#[test]
fn memory_is_pending_firmware_or_no_ram_before_anything_runs() {
    check!(shown(&[core(ALL, None)]) == [Firmware, Pending, Pending, NoRam, Pending, Pending, Pending, Firmware]);
    let m = map();
    check!(m.axis().top() == 8 * CELL && m.axis().cell_bytes() == CELL);
}

#[test]
fn a_cell_partly_the_sweeps_counts_as_the_sweeps() {
    let half = [Segment::new(words(0, CW), PhysAddr::new(CELL + CELL / 2))];
    let scene = Scene { segments: &half, cores: &[core(words(0, CW), None)], aside: &[] };
    let view = map().view(&scene, &map().faults());
    check!(view.cells == [Firmware, Pending, Pending, NoRam, Firmware, Firmware, Firmware, Firmware]);
}

#[test]
fn an_ascending_front_marks_what_is_behind_it_done_and_its_head_active() {
    let cores = [core(ALL, Some((ALL, up(CW + CW / 2))))];
    check!(shown(&cores) == [Firmware, Done, Active, NoRam, Pending, Pending, Pending, Firmware]);
}

#[test]
fn a_descending_front_works_down_from_the_end() {
    let down = Activity::Sweep { order: Order::Descending, done: CW + CW / 2 };
    check!(
        shown(&[core(ALL, Some((ALL, down)))]) == [Firmware, Pending, Pending, NoRam, Pending, Active, Done, Firmware]
    );
}

#[test]
fn every_cores_front_shows_in_the_cell_it_is_in() {
    let (a, b) = (words(0, 2 * CW), words(2 * CW, 3 * CW));
    let cores = [core(a, Some((a, up(CW / 2)))), core(b, Some((b, up(CW + CW / 2))))];
    check!(shown(&cores) == [Firmware, Active, Pending, NoRam, Done, Active, Pending, Firmware]);
}

#[test]
fn a_front_crossing_into_the_next_block_is_translated_through_both() {
    let cores = [core(ALL, Some((ALL, up(2 * CW + CW / 2))))];
    check!(shown(&cores) == [Firmware, Done, Done, NoRam, Active, Pending, Pending, Firmware]);

    // Words 1.5 cells in, a cell long, are the back half of the first block's cell and the front half
    // of the second block's.
    let across = extents(&run(), words(CW + CW / 2, CW)).collect::<Vec<_>>();
    check!(across == [Extent::new(PhysAddr::new(2 * CELL + CELL / 2), CELL / 2), Extent::new(mib(4), CELL / 2)]);
}

#[test]
fn a_hold_shows_over_everything_the_wait_leaves_alone() {
    let cores = [core(ALL, Some((ALL, Activity::Hold)))];
    check!(shown(&cores) == [Firmware, Holding, Holding, NoRam, Holding, Holding, Holding, Firmware]);
}

#[test]
fn a_window_is_active_and_the_windows_below_it_are_done() {
    let window = words(3 * CW, CW / 2);
    check!(
        shown(&[core(ALL, Some((window, Activity::Window)))])
            == [Firmware, Done, Done, NoRam, Done, Active, Pending, Firmware]
    );
}

#[test]
fn bus_stress_shows_both_streams() {
    let (low, high) = (words(0, 2 * CW), words(2 * CW, 3 * CW));
    let bus = Activity::Bus { done: CW / 2, check: low, checked: CW + CW / 2 };
    check!(
        shown(&[core(ALL, Some((high, bus)))]) == [Firmware, Done, Active, NoRam, Active, Pending, Pending, Firmware]
    );
}

#[test]
fn a_cell_is_done_only_when_all_of_the_sweeps_memory_in_it_is() {
    // One core is done with its share, which ends in the middle of the second cell of the first
    // block; the next share has not begun.
    let cores = [
        CoreFront { share: words(0, CW + CW / 2), front: None, began: true },
        CoreFront { share: words(CW + CW / 2, 5 * CW - CW - CW / 2), front: None, began: false },
    ];
    check!(shown(&cores) == [Firmware, Done, Pending, NoRam, Pending, Pending, Pending, Firmware]);
}

#[test]
fn a_fault_outranks_the_rest_wherever_it_lies_and_one_off_the_axis_shows_nowhere() {
    let cores = [core(ALL, Some((ALL, up(CW + CW / 2))))];
    let mut faults = map().faults();
    faults.note(CpuAddr::of(core::ptr::without_provenance::<u8>((2 * CELL + 8) as usize)));
    faults.note(CpuAddr::of(core::ptr::without_provenance::<u8>((7 * CELL + 8) as usize)));
    faults.note(CpuAddr::of(core::ptr::without_provenance::<u8>((9 * CELL) as usize)));
    let red = CellState::Fault;
    check!(show(&cores, &faults) == [Firmware, Done, red, NoRam, Pending, Pending, Pending, red]);
    check!(show(&cores, &faults.cleared()) == shown(&cores));
}

#[test]
fn a_reserve_run_shows_the_main_memory_done_and_the_block_lent_to_the_firmware_as_firmware() {
    // The reserve is the first and last cell; the main memory less the lent first cell of its
    // first block is set aside.
    let reserve = [Segment::new(words(0, CW), mib(0)), Segment::new(words(CW, CW), mib(7))];
    let aside = [Extent::new(mib(2), CELL), Extent::new(mib(4), 3 * CELL)];
    let share = words(0, 2 * CW);
    let cores = [core(share, Some((share, up(CW + CW / 2))))];
    let view = map().view(&Scene { segments: &reserve, cores: &cores, aside: &aside }, &map().faults());
    check!(view.cells == [Done, Firmware, Done, NoRam, Done, Done, Done, Active]);
}

#[rstest]
#[case::even(8 * 64, 5)]
#[case::uneven(10, 7)]
#[case::one_byte_a_cell(8, 3)]
fn every_byte_is_in_the_one_cell_that_cell_of_names(#[case] top: u64, #[case] len: u64) {
    let axis = Axis::<N>::new(top);
    for start in 0..top {
        let extent = Extent::new(PhysAddr::new(start), len);
        let cells: Vec<(usize, u64)> = axis.cells(extent).collect();
        check!(cells.iter().map(|c| c.1).sum::<u64>() == extent.end().min(top) - start, "{start}");
        check!(cells.first().map(|c| c.0) == axis.cell_of(start), "{start}");
        let inside = |c: usize, a: u64| axis.cell_of(a) == Some(c);
        for (cell, bytes) in cells {
            check!((start..extent.end().min(top)).filter(|&a| inside(cell, a)).count() as u64 == bytes, "{start}");
        }
    }
    check!(axis.cell_of(top).is_none());
}

#[test]
fn a_machine_with_no_ram_to_map_shows_nothing() {
    let scene = Scene { segments: &[], cores: &[], aside: &[] };
    check!(MemMap::<N>::new(&[]).view(&scene, &FaultCells::default()) == View::NONE);
}
