use std::vec;
use std::vec::Vec;

use assert2::{assert, check};
use rstest::rstest;

use super::*;
use crate::kernels::{Stride, fill_stride, verify_fill_stride, verify_stride};
use crate::ops::fill_except;

mod bench;

/// Every kernel the host can run; tiers it lacks are skipped.
pub fn kernels() -> Vec<Kernel> {
    // SAFETY: the flags come from the host's own feature detection.
    let caps = unsafe { Caps::new(std::is_x86_feature_detected!("avx2"), std::is_x86_feature_detected!("avx512f")) };
    caps.kernels().collect()
}

/// A 64-byte-aligned host buffer of `words` words.
pub struct Buf {
    backing: Vec<u64>,
    offset: usize,
    words: usize,
}

impl Buf {
    pub fn new(words: usize) -> Self {
        let backing = vec![0; words + BLOCK_WORDS];
        let offset = backing.as_ptr().align_offset(BLOCK_BYTES);
        Self { backing, offset, words }
    }

    pub fn region(&mut self) -> Region<'_> {
        // SAFETY: the buffer is ours, and no reference into it is used while the region lives.
        unsafe { Region::from_raw(self.backing.as_mut_ptr().add(self.offset), self.words) }.unwrap()
    }

    fn words(&self) -> &[u64] {
        &self.backing[self.offset..self.offset + self.words]
    }

    fn addr(&self, word: usize) -> CpuAddr {
        CpuAddr::of(&raw const self.backing[self.offset + word])
    }

    fn set(&mut self, word: usize, value: u64) {
        self.backing[self.offset + word] = value;
    }
}

/// The fills the tests drive, each with its reference word.
#[derive(Clone, Copy, Debug)]
enum Pattern {
    Const(u64),
    Address(u64),
    Prsg { seed: u64, key: u64 },
}

impl Pattern {
    const ALL: [Self; 5] = [
        Self::Const(0x5555_aaaa_0f0f_f0f0),
        Self::Address(0),
        Self::Address(!0),
        Self::Prsg { seed: 1, key: 0 },
        Self::Prsg { seed: 0xdead_beef, key: !0 },
    ];

    fn expected(self, buf: &Buf, word: usize) -> u64 {
        match self {
            Self::Const(v) => v,
            Self::Address(key) => buf.addr(word).get() as u64 ^ key,
            Self::Prsg { seed, key } => Prsg::word(seed, word as u64) ^ key,
        }
    }

    /// Fill `buf` chunk by chunk with `kernel`.
    fn fill(self, kernel: Kernel, buf: &mut Buf, chunk_words: usize) {
        let mut region = buf.region();
        let mut prsg = self.prsg();
        for mut chunk in region.chunks(chunk_words) {
            match self {
                Self::Const(v) => fill(kernel, &mut chunk, v),
                Self::Address(key) => fill_address(kernel, &mut chunk, key),
                Self::Prsg { key, .. } => fill_prsg(kernel, &mut chunk, &mut prsg, key),
            }
        }
    }

    /// Verify `buf` chunk by chunk with `kernel`, returning the faults reported and the count seen.
    fn verify(self, kernel: Kernel, buf: &mut Buf, chunk_words: usize, budget: u64) -> (Vec<Fault>, u64) {
        let mut reported = Vec::new();
        let mut sink = |f| reported.push(f);
        let mut faults = Faults::new(budget, &mut sink);
        let mut region = buf.region();
        let mut prsg = self.prsg();
        for chunk in region.chunks(chunk_words) {
            match self {
                Self::Const(v) => verify(kernel, &chunk, v, &mut faults),
                Self::Address(key) => verify_address(kernel, &chunk, key, &mut faults),
                Self::Prsg { key, .. } => verify_prsg(kernel, &chunk, &mut prsg, key, &mut faults),
            }
        }
        let seen = faults.seen();
        (reported, seen)
    }

    fn prsg(self) -> Prsg {
        match self {
            Self::Prsg { seed, .. } => Prsg::new(seed),
            _ => Prsg::new(0),
        }
    }
}

const WORDS: usize = 40 * BLOCK_WORDS;
/// Not a divisor of `WORDS`, so the last chunk is short.
const CHUNK: usize = 7 * BLOCK_WORDS;

#[test]
fn every_tier_fills_the_reference_words() {
    for kernel in kernels() {
        for pattern in Pattern::ALL {
            let mut buf = Buf::new(WORDS);
            pattern.fill(kernel, &mut buf, CHUNK);
            let want: Vec<u64> = (0..WORDS).map(|w| pattern.expected(&buf, w)).collect();
            check!(buf.words() == want.as_slice(), "{kernel} {pattern:?}");
        }
    }
}

#[test]
fn any_tier_verifies_any_tiers_fill() {
    for filler in kernels() {
        for checker in kernels() {
            for pattern in Pattern::ALL {
                let mut buf = Buf::new(WORDS);
                pattern.fill(filler, &mut buf, CHUNK);
                let (faults, seen) = pattern.verify(checker, &mut buf, 3 * BLOCK_WORDS, 16);
                check!(faults.is_empty() && seen == 0, "fill {filler}, verify {checker}, {pattern:?}");
            }
        }
    }
}

#[test]
fn injected_faults_are_reported_exactly() {
    // The first and last words, both sides of a chunk edge, and two words in one block.
    let flips = [(0, 1), (CHUNK - 1, 1 << 63), (CHUNK, 0xff00), (CHUNK + 3, 4), (CHUNK + 5, !0), (WORDS - 1, 1 << 17)];
    for kernel in kernels() {
        for pattern in Pattern::ALL {
            let mut buf = Buf::new(WORDS);
            pattern.fill(Kernel::SCALAR, &mut buf, WORDS);
            let want: Vec<Fault> = flips
                .iter()
                .map(|&(word, flip)| {
                    let expected = pattern.expected(&buf, word);
                    Fault { addr: buf.addr(word), expected, actual: expected ^ flip }
                })
                .collect();
            for fault in &want {
                let word = (fault.addr.get() - buf.addr(0).get()) / 8;
                buf.set(word, fault.actual);
            }
            let (faults, seen) = pattern.verify(kernel, &mut buf, CHUNK, 64);
            check!(faults == want, "{kernel} {pattern:?}");
            check!(seen == want.len() as u64);
        }
    }
}

#[test]
fn the_budget_caps_reports_but_not_the_count() {
    for kernel in kernels() {
        let mut buf = Buf::new(WORDS);
        let pattern = Pattern::Const(0);
        pattern.fill(kernel, &mut buf, CHUNK);
        for word in (0..WORDS).step_by(9) {
            buf.set(word, 1);
        }
        let (faults, seen) = pattern.verify(kernel, &mut buf, CHUNK, 5);
        check!(faults.len() == 5, "{kernel}");
        check!(seen == WORDS.div_ceil(9) as u64);
        check!(faults[0].addr == buf.addr(0));
    }
}

#[rstest]
#[case::ascending(Order::Ascending)]
#[case::descending(Order::Descending)]
fn an_inversion_step_checks_then_rewrites_in_order(#[case] order: Order) {
    let (old, new) = (0x0123_4567_89ab_cdef, !0x0123_4567_89ab_cdef);
    let (low, high) = (BLOCK_WORDS + 2, WORDS - 3);
    for kernel in kernels() {
        let mut buf = Buf::new(WORDS);
        Pattern::Const(old).fill(kernel, &mut buf, WORDS);
        buf.set(low, 0);
        buf.set(high, 0);
        let mut reported = Vec::new();
        let mut sink = |f: Fault| reported.push(f.addr);
        let mut faults = Faults::new(8, &mut sink);
        let mut region = buf.region();
        let step = Inversion { expect: old, write: new, order };
        let mut chunks: Vec<Region<'_>> = region.chunks(CHUNK).collect();
        if order == Order::Descending {
            chunks.reverse();
        }
        for chunk in &mut chunks {
            invert(kernel, chunk, step, &mut faults);
        }
        let want = match order {
            Order::Ascending => [buf.addr(low), buf.addr(high)],
            Order::Descending => [buf.addr(high), buf.addr(low)],
        };
        check!(reported == want, "{kernel}");
        check!(buf.words().iter().all(|&w| w == new), "{kernel}");
    }
}

#[test]
fn every_tier_copies_a_region() {
    for kernel in kernels() {
        let mut buf = Buf::new(2 * WORDS);
        let mut region = buf.region();
        let (mut src, mut dst) = region.split_at(WORDS);
        fill_prsg(Kernel::SCALAR, &mut src, &mut Prsg::new(7), 0);
        copy(kernel, &src, &mut dst).unwrap();
        let mut seen = 0;
        let mut sink = |_| seen += 1;
        let mut faults = Faults::new(0, &mut sink);
        verify_prsg(Kernel::SCALAR, &dst, &mut Prsg::new(7), 0, &mut faults);
        check!(faults.seen() == 0, "{kernel}");
    }
}

#[test]
fn a_region_rejects_misaligned_bases_and_partial_blocks() {
    let mut buf = Buf::new(WORDS);
    let base = buf.region().ptr();
    // SAFETY: never used to touch memory; only the checks run.
    let misaligned = unsafe { Region::from_raw(base.add(1), BLOCK_WORDS) };
    assert!(misaligned.unwrap_err() == RegionError::Misaligned { addr: buf.addr(1) });
    // SAFETY: as above.
    let partial = unsafe { Region::from_raw(base, BLOCK_WORDS + 1) };
    check!(partial.unwrap_err() == RegionError::Length { words: BLOCK_WORDS + 1 });
}

/// The strides the modulo test and its neighbours use: every offset and chunk phase of a few periods.
fn strides() -> impl Iterator<Item = Stride> {
    [8, 9, 16, 20, 24, 37]
        .into_iter()
        .flat_map(|n| (0..n).flat_map(move |offset| (0..n).map(move |first| Stride { n, offset, first })))
}

/// Whether word `w` of the chunk is one `stride` selects.
fn selected(stride: Stride, w: usize) -> bool {
    (stride.first + w) % stride.n == stride.offset
}

fn sentinel(w: usize) -> u64 {
    0xdead_0000_0000_0000 | (w as u64 * 0x0101)
}

const STRIDE_VALUE: u64 = 0x0123_4567_89ab_cdef;

#[test]
fn fill_except_stores_the_value_everywhere_but_the_selected_words() {
    for kernel in kernels() {
        for stride in strides() {
            let mut buf = Buf::new(WORDS);
            (0..WORDS).for_each(|w| buf.set(w, sentinel(w)));
            fill_except(kernel, &mut buf.region(), stride, STRIDE_VALUE);
            let want: Vec<u64> =
                (0..WORDS).map(|w| if selected(stride, w) { sentinel(w) } else { STRIDE_VALUE }).collect();
            check!(buf.words() == want.as_slice(), "{kernel} {stride:?}");
        }
    }
}

#[test]
fn fill_except_in_chunks_matches_one_pass_over_the_region() {
    for kernel in kernels() {
        for stride in strides().filter(|s| s.first == 0) {
            let mut buf = Buf::new(WORDS);
            (0..WORDS).for_each(|w| buf.set(w, sentinel(w)));
            let mut region = buf.region();
            for (i, mut chunk) in region.chunks(3 * BLOCK_WORDS).enumerate() {
                let first = i * 3 * BLOCK_WORDS % stride.n;
                fill_except(kernel, &mut chunk, Stride { first, ..stride }, STRIDE_VALUE);
            }
            let want: Vec<u64> =
                (0..WORDS).map(|w| if selected(stride, w) { sentinel(w) } else { STRIDE_VALUE }).collect();
            check!(buf.words() == want.as_slice(), "{kernel} {stride:?}");
        }
    }
}

#[test]
fn fill_stride_stores_the_value_in_the_selected_words_only() {
    for stride in strides() {
        let mut buf = Buf::new(WORDS);
        (0..WORDS).for_each(|w| buf.set(w, sentinel(w)));
        // SAFETY: the buffer's region is ours.
        unsafe { fill_stride(buf.region().ptr(), WORDS, stride, STRIDE_VALUE) };
        let want: Vec<u64> = (0..WORDS).map(|w| if selected(stride, w) { STRIDE_VALUE } else { sentinel(w) }).collect();
        check!(buf.words() == want.as_slice(), "{stride:?}");
    }
}

#[test]
fn verify_stride_reports_the_selected_words_that_differ_in_address_order() {
    let flips = [0, 1, 7, 8, 19, 20, 40, 41, 100, 159, 160, WORDS - 1];
    for stride in strides() {
        let mut buf = Buf::new(WORDS);
        (0..WORDS).for_each(|w| buf.set(w, if selected(stride, w) { STRIDE_VALUE } else { sentinel(w) }));
        for &w in &flips {
            buf.set(w, buf.words()[w] ^ (1 << (w % 64)));
        }
        let want: Vec<Fault> = flips
            .iter()
            .filter(|&&w| selected(stride, w))
            .map(|&w| Fault { addr: buf.addr(w), expected: STRIDE_VALUE, actual: STRIDE_VALUE ^ (1 << (w % 64)) })
            .collect();
        let mut reported = Vec::new();
        let mut sink = |f| reported.push(f);
        let mut faults = Faults::new(64, &mut sink);
        // SAFETY: the buffer's region is ours.
        unsafe { verify_stride(buf.region().ptr(), WORDS, stride, STRIDE_VALUE, &mut faults) };
        let seen = faults.seen();
        check!(reported == want, "{stride:?}");
        check!(seen == want.len() as u64, "{stride:?}");
    }
}

#[test]
fn verify_fill_stride_does_what_verify_stride_then_fill_stride_do() {
    let flips = [0, 5, 7, 8, 19, 20, 21, 40, 41, 100, 159, 160, WORDS - 1];
    for n in [8, 20, 37] {
        for first in 0..n {
            for check in (0..n).step_by(3) {
                for fill in [check, (check + 1) % n, (check + 7) % n] {
                    let at = |offset| Stride { n, offset, first };
                    let setup = |buf: &mut Buf| {
                        (0..WORDS)
                            .for_each(|w| buf.set(w, if selected(at(check), w) { STRIDE_VALUE } else { sentinel(w) }));
                        for &w in &flips {
                            buf.set(w, buf.words()[w] ^ (1 << (w % 64)));
                        }
                    };
                    let run = |fused: bool| {
                        let mut buf = Buf::new(WORDS);
                        setup(&mut buf);
                        let mut reported = Vec::new();
                        let mut sink = |f| reported.push(f);
                        let mut faults = Faults::new(64, &mut sink);
                        let ptr = buf.region().ptr();
                        let value = !STRIDE_VALUE;
                        // SAFETY: the buffer's region is ours.
                        unsafe {
                            if fused {
                                verify_fill_stride(ptr, WORDS, at(check), STRIDE_VALUE, at(fill), value, &mut faults);
                            } else {
                                verify_stride(ptr, WORDS, at(check), STRIDE_VALUE, &mut faults);
                                fill_stride(ptr, WORDS, at(fill), value);
                            }
                        }
                        let seen = faults.seen();
                        (buf.words().to_vec(), reported, seen)
                    };
                    check!(run(true) == run(false), "n {n} first {first} check {check} fill {fill}");
                }
            }
        }
    }
}
