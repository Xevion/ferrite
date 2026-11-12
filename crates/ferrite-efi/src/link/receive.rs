//! Receiving a pushed image: the size limit, in-order chunks and the final checks.

use alloc::vec::Vec;

use ferrite_wire::link::{PushBegin, PushKind};
use snafu::{Snafu, ensure};

/// Largest image the loader accepts.
pub const MAX_IMAGE: u64 = 64 * 1024 * 1024;

/// Why a push ended. Its `Display` is the reason sent to the desktop.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Snafu)]
pub enum Reject {
    #[snafu(display("watchdog_secs must not be 0"))]
    NoWatchdog,
    #[snafu(display("image size {size} is outside 1..={MAX_IMAGE}"))]
    Size { size: u64 },
    #[snafu(display("unknown image kind {raw}"))]
    Kind { raw: u8 },
    #[snafu(display("cannot reserve {size} bytes for the image"))]
    Memory { size: u64 },
    #[snafu(display("chunk at offset {got}, expected {expected}"))]
    Offset { expected: u64, got: u64 },
    #[snafu(display("chunk of {len} bytes at offset {offset} runs past the declared size {size}"))]
    Overrun { offset: u64, len: usize, size: u64 },
    #[snafu(display("push ended after {have} of {size} bytes"))]
    Short { have: u64, size: u64 },
    #[snafu(display("image crc32 is {actual:#010x}, expected {expected:#010x}"))]
    Crc { expected: u32, actual: u32 },
}

/// A complete image whose length and CRC match what was announced.
#[derive(Debug)]
pub struct Image {
    pub begin: PushBegin,
    pub bytes: Vec<u8>,
}

/// An image being received: chunks must arrive in order from offset 0.
#[derive(Debug)]
pub struct Receiver {
    begin: PushBegin,
    buf: Vec<u8>,
    hasher: crc32fast::Hasher,
}

impl Receiver {
    /// Validate the announcement and reserve the whole image up front.
    pub fn start(begin: PushBegin) -> Result<Self, Reject> {
        ensure!(begin.watchdog_secs != 0, NoWatchdogSnafu);
        ensure!(begin.size >= 1 && begin.size <= MAX_IMAGE, SizeSnafu { size: begin.size });
        if let PushKind::Unknown(raw) = begin.kind {
            return KindSnafu { raw }.fail();
        }
        let mut buf = Vec::new();
        let size = usize::try_from(begin.size).map_err(|_| Reject::Memory { size: begin.size })?;
        buf.try_reserve_exact(size).map_err(|_| Reject::Memory { size: begin.size })?;
        Ok(Self { begin, buf, hasher: crc32fast::Hasher::new() })
    }

    #[must_use]
    pub const fn begin(&self) -> &PushBegin {
        &self.begin
    }

    /// Bytes received so far.
    #[must_use]
    pub fn received(&self) -> u64 {
        self.buf.len() as u64
    }

    /// Append the chunk that starts at `offset`, which must be where the last one ended.
    pub fn accept(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Reject> {
        let have = self.received();
        ensure!(offset == have, OffsetSnafu { expected: have, got: offset });
        let fits = have.checked_add(bytes.len() as u64).is_some_and(|end| end <= self.begin.size);
        ensure!(fits, OverrunSnafu { offset, len: bytes.len(), size: self.begin.size });
        self.hasher.update(bytes);
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    /// Check the length and CRC and hand over the image.
    pub fn finish(self) -> Result<Image, Reject> {
        let have = self.received();
        ensure!(have == self.begin.size, ShortSnafu { have, size: self.begin.size });
        let actual = self.hasher.finalize();
        ensure!(actual == self.begin.crc32, CrcSnafu { expected: self.begin.crc32, actual });
        Ok(Image { begin: self.begin, bytes: self.buf })
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use ferrite_wire::link::ShortStr;
    use rstest::rstest;

    use super::*;

    const IMAGE: &[u8] = b"MZ pretend image bytes";

    fn begin_for(image: &[u8]) -> PushBegin {
        PushBegin {
            kind: PushKind::App,
            size: image.len() as u64,
            crc32: crc32fast::hash(image),
            watchdog_secs: 300,
            name: ShortStr::new("app.efi").expect("short"),
        }
    }

    #[test]
    fn chunks_in_order_make_an_image() {
        let mut rx = Receiver::start(begin_for(IMAGE)).expect("accepted");
        for (i, chunk) in IMAGE.chunks(5).enumerate() {
            assert!(let Ok(()) = rx.accept((i * 5) as u64, chunk));
        }
        assert!(let Ok(image) = rx.finish());
        check!(image.bytes == IMAGE);
        check!(image.begin.kind == PushKind::App);
    }

    #[rstest]
    #[case::no_watchdog(PushBegin { watchdog_secs: 0, ..begin_for(IMAGE) }, Reject::NoWatchdog)]
    #[case::empty(PushBegin { size: 0, ..begin_for(IMAGE) }, Reject::Size { size: 0 })]
    #[case::too_big(PushBegin { size: MAX_IMAGE + 1, ..begin_for(IMAGE) }, Reject::Size { size: MAX_IMAGE + 1 })]
    #[case::unknown_kind(PushBegin { kind: PushKind::Unknown(7), ..begin_for(IMAGE) }, Reject::Kind { raw: 7 })]
    fn a_bad_announcement_is_refused_before_anything_is_reserved(#[case] begin: PushBegin, #[case] want: Reject) {
        assert!(let Err(got) = Receiver::start(begin));
        check!(got == want);
    }

    #[test]
    fn the_largest_allowed_size_passes_the_announcement_checks() {
        let begin = PushBegin { size: MAX_IMAGE, ..begin_for(IMAGE) };
        check!(Receiver::start(begin).is_ok());
    }

    #[test]
    fn an_out_of_order_or_repeated_chunk_is_refused() {
        let mut rx = Receiver::start(begin_for(IMAGE)).expect("accepted");
        assert!(let Ok(()) = rx.accept(0, &IMAGE[..4]));
        check!(rx.accept(8, &IMAGE[8..12]) == Err(Reject::Offset { expected: 4, got: 8 }));
        check!(rx.accept(0, &IMAGE[..4]) == Err(Reject::Offset { expected: 4, got: 0 }));
        check!(rx.received() == 4, "a refused chunk adds nothing");
    }

    #[test]
    fn data_past_the_declared_size_is_refused() {
        let mut rx = Receiver::start(begin_for(IMAGE)).expect("accepted");
        let long = [0u8; 64];
        check!(rx.accept(0, &long) == Err(Reject::Overrun { offset: 0, len: 64, size: IMAGE.len() as u64 }));
    }

    #[test]
    fn finishing_early_or_with_the_wrong_crc_is_refused() {
        let mut rx = Receiver::start(begin_for(IMAGE)).expect("accepted");
        assert!(let Ok(()) = rx.accept(0, &IMAGE[..10]));
        check!(rx.finish().err() == Some(Reject::Short { have: 10, size: IMAGE.len() as u64 }));

        let mut corrupt = IMAGE.to_vec();
        corrupt[3] ^= 1;
        let mut rx = Receiver::start(begin_for(IMAGE)).expect("accepted");
        assert!(let Ok(()) = rx.accept(0, &corrupt));
        assert!(let Err(Reject::Crc { expected, actual }) = rx.finish());
        check!(expected == crc32fast::hash(IMAGE) && actual == crc32fast::hash(&corrupt));
    }
}
