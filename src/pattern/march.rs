//! March C- test: a systematic read/write sequence from the academic March
//! test family. Unlike single-cell patterns (solid bits, walking ones), a
//! march applies each element's operations to every cell *in address order*,
//! completing one cell's element before advancing. This ordering is what lets
//! it detect coupling faults (`CFin`, `CFid`) and address decoder faults (AF)
//! that bulk fill-and-verify cannot.
//!
//! March C- (10N):
//! ```text
//! M0: ↕(w0)      M1: ↑(r0,w1)   M2: ↑(r1,w0)
//! M3: ↓(r0,w1)   M4: ↓(r1,w0)   M5: ↕(r0)
//! ```
//!
//! **Ordering caveat:** coupling coverage assumes ascending/descending
//! traversal visits physically adjacent cells. With scattered 4 KiB pages that
//! holds only within a page; SAF/TF/AF detection is unaffected by layout.
//! Hugepages (2 MiB) restore physical contiguity within each page. This
//! mirrors what `MemTest86`+ does with Moving Inversions -- a practical
//! approximation, not a formal guarantee.

use std::ptr;

use crate::Failure;
use crate::shutdown;

/// One operation applied to each cell as a march element sweeps the buffer.
#[derive(Clone, Copy)]
enum MarchOp {
    /// Read the cell, expecting all-zero (`false`) or all-one (`true`) words.
    Read(bool),
    /// Write all-zero (`false`) or all-one (`true`) to the cell.
    Write(bool),
}

/// Address traversal order for a march element.
#[derive(Clone, Copy, PartialEq)]
enum Direction {
    Ascending,
    Descending,
}

/// A march element: a traversal direction plus the operations applied to each
/// cell in that direction before advancing to the next cell.
struct MarchElement {
    dir: Direction,
    ops: &'static [MarchOp],
}

use Direction::{Ascending, Descending};
use MarchOp::{Read, Write};

/// The canonical March C- element sequence (M0–M5). `↕` elements use a fixed
/// direction since a single-operation sweep is direction-agnostic.
const MARCH_C_MINUS: &[MarchElement] = &[
    MarchElement {
        dir: Ascending,
        ops: &[Write(false)],
    }, // M0: ↕(w0)
    MarchElement {
        dir: Ascending,
        ops: &[Read(false), Write(true)],
    }, // M1: ↑(r0,w1)
    MarchElement {
        dir: Ascending,
        ops: &[Read(true), Write(false)],
    }, // M2: ↑(r1,w0)
    MarchElement {
        dir: Descending,
        ops: &[Read(false), Write(true)],
    }, // M3: ↓(r0,w1)
    MarchElement {
        dir: Descending,
        ops: &[Read(true), Write(false)],
    }, // M4: ↓(r1,w0)
    MarchElement {
        dir: Ascending,
        ops: &[Read(false)],
    }, // M5: ↕(r0)
];

/// The all-zero or all-one word a `false`/`true` cell state maps to.
#[inline]
const fn word(bit: bool) -> u64 {
    if bit { u64::MAX } else { 0 }
}

/// Report activity roughly every this many cells, and check for cancellation
/// at the same cadence so a long element bails out promptly on quit.
const REPORT_CHUNK: usize = 64 * 1024;

/// Apply one march element to `buf` in its traversal direction, pushing a
/// [`Failure`] for every read whose observed word differs from its expected
/// state. Returns `true` if a quit was requested mid-sweep.
fn run_element(
    buf: &mut [u64],
    element: &MarchElement,
    base_addr: usize,
    failures: &mut Vec<Failure>,
    on_activity: &(dyn Fn(f64) + Sync),
) -> bool {
    let len = buf.len();
    if len == 0 {
        return false;
    }
    let base = buf.as_mut_ptr();

    let apply = |i: usize, failures: &mut Vec<Failure>| {
        // SAFETY: i < len, so base.add(i) is in bounds and aligned. All buffer
        // access is volatile to preserve the observable read/write ordering the
        // march depends on.
        let cell = unsafe { base.add(i) };
        for op in element.ops {
            match op {
                Read(bit) => {
                    let expected = word(*bit);
                    let actual = unsafe { ptr::read_volatile(cell) };
                    if actual != expected {
                        failures.push(Failure {
                            addr: base_addr + i * 8,
                            expected,
                            actual,
                            word_index: i,
                            phys_addr: None,
                        });
                    }
                }
                Write(bit) => unsafe { ptr::write_volatile(cell, word(*bit)) },
            }
        }
    };

    match element.dir {
        Ascending => {
            for i in 0..len {
                apply(i, failures);
                if i % REPORT_CHUNK == 0 {
                    on_activity(i as f64 / len as f64);
                    if shutdown::quit_requested() {
                        return true;
                    }
                }
            }
        }
        Descending => {
            for i in (0..len).rev() {
                apply(i, failures);
                if i % REPORT_CHUNK == 0 {
                    on_activity(i as f64 / len as f64);
                    if shutdown::quit_requested() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Run the full March C- sequence over `buf`.
///
/// The march is strictly sequential: each element must complete cell-by-cell
/// in address order for its coupling/address-decoder coverage to hold, so the
/// `parallel` flag is accepted for signature parity but does not split the
/// buffer. `on_subpass` fires once per completed element (6 total), matching
/// [`Pattern::sub_passes`](crate::pattern::Pattern::sub_passes).
pub(super) fn run(
    buf: &mut [u64],
    _parallel: bool,
    on_subpass: &mut impl FnMut(),
    on_activity: &(dyn Fn(f64) + Sync),
) -> Vec<Failure> {
    let base_addr = buf.as_ptr() as usize;
    let mut failures = Vec::new();
    for element in MARCH_C_MINUS {
        let quit = run_element(buf, element, base_addr, &mut failures, on_activity);
        on_subpass();
        if quit || shutdown::quit_requested() {
            break;
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;

    static NOOP_ACTIVITY: fn(f64) = |_| {};

    mod element {
        use assert2::{assert, check};

        use super::*;

        fn read0(dir: Direction) -> MarchElement {
            MarchElement {
                dir,
                ops: &[Read(false)],
            }
        }

        #[test]
        fn read_reports_every_mismatch_ascending() {
            let mut buf = vec![u64::MAX; 8]; // all cells hold 1, element expects 0
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            let quit = run_element(
                &mut buf,
                &read0(Ascending),
                base,
                &mut failures,
                &NOOP_ACTIVITY,
            );
            check!(!quit);
            assert!(failures.len() == 8);
            // Ascending traversal reports low indices first.
            let indices: Vec<usize> = failures.iter().map(|f| f.word_index).collect();
            check!(indices == vec![0, 1, 2, 3, 4, 5, 6, 7]);
            check!(failures[0].expected == 0);
            check!(failures[0].actual == u64::MAX);
            check!(failures[3].addr == base + 3 * 8);
        }

        #[test]
        fn read_reports_descending_order() {
            let mut buf = vec![u64::MAX; 5];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            run_element(
                &mut buf,
                &read0(Descending),
                base,
                &mut failures,
                &NOOP_ACTIVITY,
            );
            let indices: Vec<usize> = failures.iter().map(|f| f.word_index).collect();
            check!(indices == vec![4, 3, 2, 1, 0]);
        }

        #[test]
        fn write_then_read_is_clean() {
            let mut buf = vec![0u64; 16];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            let w1 = MarchElement {
                dir: Ascending,
                ops: &[Write(true)],
            };
            let r1 = MarchElement {
                dir: Ascending,
                ops: &[Read(true)],
            };
            run_element(&mut buf, &w1, base, &mut failures, &NOOP_ACTIVITY);
            run_element(&mut buf, &r1, base, &mut failures, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn corruption_between_write_and_read_is_caught() {
            let mut buf = vec![0u64; 32];
            let base = buf.as_ptr() as usize;
            let mut failures = Vec::new();
            let w1 = MarchElement {
                dir: Ascending,
                ops: &[Write(true)],
            };
            run_element(&mut buf, &w1, base, &mut failures, &NOOP_ACTIVITY);
            // Simulate a cell that fails to hold the written 1 (stuck-low bit).
            buf[17] = u64::MAX ^ (1 << 2);
            let r1 = MarchElement {
                dir: Ascending,
                ops: &[Read(true)],
            };
            run_element(&mut buf, &r1, base, &mut failures, &NOOP_ACTIVITY);
            assert!(failures.len() == 1);
            check!(failures[0].word_index == 17);
            check!(failures[0].flipped_bits() == 1);
        }

        #[test]
        fn empty_buffer_is_a_noop() {
            let mut buf: Vec<u64> = vec![];
            let mut failures = Vec::new();
            let quit = run_element(
                &mut buf,
                &read0(Ascending),
                0,
                &mut failures,
                &NOOP_ACTIVITY,
            );
            check!(!quit);
            assert!(failures.is_empty());
        }
    }

    mod full_sequence {
        use assert2::{assert, check};
        use serial_test::serial;

        use super::*;
        use crate::shutdown::{self, QuitReason};

        #[test]
        fn clean_memory_has_no_failures() {
            let mut buf = vec![0u64; 4096];
            let failures = run(&mut buf, false, &mut || {}, &NOOP_ACTIVITY);
            assert!(failures.is_empty());
        }

        #[test]
        fn fires_one_subpass_per_element() {
            let mut buf = vec![0u64; 256];
            let mut count = 0u32;
            run(&mut buf, false, &mut || count += 1, &NOOP_ACTIVITY);
            check!(count == 6);
        }

        #[test]
        #[serial]
        fn quit_stops_the_sequence_early() {
            shutdown::reset();
            let mut buf = vec![0u64; 256];
            let mut count = 0u32;
            run(
                &mut buf,
                false,
                &mut || {
                    count += 1;
                    if count == 2 {
                        shutdown::request_quit(QuitReason::UserQuit);
                    }
                },
                &NOOP_ACTIVITY,
            );
            shutdown::reset();
            check!(count == 2);
        }
    }
}
