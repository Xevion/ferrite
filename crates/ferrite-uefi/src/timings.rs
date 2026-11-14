//! What each test took on this machine, kept on the boot volume so the next run knows its pace from
//! the first second.
//!
//! The file is `\EFI\ferrite\timings.dat`, text, and small: a header with the format version, one
//! line per (machine, preset, memory size, processors, kernel tier, test) with the milliseconds the
//! test took, and a closing line with the record count and a checksum of everything before it. A
//! file that is cut short, edited, from a newer format or not a timings file at all is refused as a
//! whole, with the reason, and the run starts without priors; the next finished test writes a fresh one.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write as _};
use core::time::Duration;

use ferrite_efi::bootlog;
use ferrite_efi::volume::{self, ReadError};
use ferrite_hal::Instant;
use snafu::Snafu;

/// Where the timings live on the boot volume.
pub const PATH: &str = "\\EFI\\ferrite\\timings.dat";

const HEADER: &str = "ferrite-timings";
const VERSION: u32 = 1;
const END: &str = "end";
/// Records kept; the least recently updated go first. A machine has a handful of setups, each with
/// at most a dozen tests, so this is room for dozens of them in a few tens of KiB.
const MAX_RECORDS: usize = 512;
/// The longest a test is taken to have run: longer is a corrupt number, not a slow machine.
const MAX_MS: u64 = 30 * 24 * 3600 * 1000;
/// The file is rewritten at most this often, and when the run ends.
pub const SAVE_EVERY: Duration = Duration::from_secs(10);
/// Bytes of a file past which it is not read at all.
const MAX_FILE: usize = 128 << 10;

/// The setup a timing is valid for: tests take as long as they do on this machine, with this
/// preset's counts, over about this much memory, on this many processors, with this kernel tier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    /// A hash of what identifies the machine (see [`crate::machine`]).
    pub machine: u64,
    pub preset: String,
    /// The memory under test, in units of [`Key::MEM_UNIT`], rounded to the nearest.
    pub mem: u64,
    pub cpus: u32,
    pub tier: String,
}

impl Key {
    pub const MEM_UNIT: usize = 256 << 20;

    #[must_use]
    pub fn new(machine: u64, preset: &str, bytes: usize, cpus: usize, tier: &str) -> Self {
        Self {
            machine,
            preset: preset.into(),
            mem: ((bytes + Self::MEM_UNIT / 2) / Self::MEM_UNIT) as u64,
            cpus: u32::try_from(cpus).unwrap_or(u32::MAX),
            tier: tier.into(),
        }
    }
}

/// What is wrong with a timings file.
#[derive(Debug, PartialEq, Eq, Snafu)]
pub enum ParseError {
    #[snafu(display("larger than {MAX_FILE} bytes"))]
    TooLarge,
    #[snafu(display("not valid UTF-8"))]
    Encoding,
    #[snafu(display("not a timings file (no {HEADER} header)"))]
    Header,
    #[snafu(display("format version {found}, this build reads {VERSION}"))]
    Version { found: String },
    #[snafu(display("line {line}: bad {field}"))]
    Record { line: usize, field: &'static str },
    #[snafu(display("cut short: no {END} line"))]
    Truncated,
    #[snafu(display("the {END} line does not match the {records} records before it"))]
    Count { records: usize },
    #[snafu(display("the checksum does not match what precedes it"))]
    Checksum,
    #[snafu(display("more than {MAX_RECORDS} records"))]
    TooMany,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    key: Key,
    test: String,
    millis: u64,
}

/// The timings, oldest update first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Timings {
    records: Vec<Record>,
}

/// FNV-1a over `bytes`: enough to notice a damaged file, not a defence against anyone.
#[must_use]
pub fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

impl Timings {
    /// How long `test` took under `key` last time, if known.
    #[must_use]
    pub fn get(&self, key: &Key, test: &str) -> Option<Duration> {
        self.records.iter().find(|r| &r.key == key && r.test == test).map(|r| Duration::from_millis(r.millis))
    }

    /// How many timings there are.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Note that `test` took `took` under `key`, replacing what it took before.
    pub fn record(&mut self, key: &Key, test: &str, took: Duration) {
        let millis = u64::try_from(took.as_millis()).unwrap_or(MAX_MS).min(MAX_MS);
        if let Some(at) = self.records.iter().position(|r| &r.key == key && r.test == test) {
            self.records.remove(at);
        }
        self.records.push(Record { key: key.clone(), test: test.into(), millis });
        if self.records.len() > MAX_RECORDS {
            self.records.remove(0);
        }
    }

    /// The file's text.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut out = String::with_capacity(64 + self.records.len() * 80);
        // Writing to a String does not fail.
        let _ = writeln!(out, "{HEADER} {VERSION}");
        for r in &self.records {
            let Key { machine, preset, mem, cpus, tier } = &r.key;
            let _ = writeln!(out, "{machine:016x} {preset} {mem} {cpus} {tier} {} {}", r.millis, r.test);
        }
        let _ = writeln!(out, "{END} {} {:016x}", self.records.len(), fnv1a(out.as_bytes()));
        out
    }

    /// The timings in `bytes`.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() > MAX_FILE {
            return Err(ParseError::TooLarge);
        }
        let text = core::str::from_utf8(bytes).map_err(|_| ParseError::Encoding)?;
        let mut lines = text.split_inclusive('\n');
        let head = lines.next().ok_or(ParseError::Header)?.trim_end();
        let version = head.strip_prefix(HEADER).and_then(|v| v.strip_prefix(' ')).ok_or(ParseError::Header)?;
        if version != "1" {
            return Err(ParseError::Version { found: version.chars().take(8).collect() });
        }
        let mut records = Vec::new();
        let mut consumed = head.len() + 1;
        for (n, line) in lines.enumerate() {
            let line_no = n + 2;
            let body = line.strip_suffix('\n').ok_or(ParseError::Truncated)?;
            if let Some(tail) = body.strip_prefix(END).and_then(|t| t.strip_prefix(' ')) {
                let (count, sum) = tail.split_once(' ').ok_or(ParseError::Record { line: line_no, field: "end" })?;
                let count: usize = count.parse().map_err(|_| ParseError::Record { line: line_no, field: "count" })?;
                let sum = u64::from_str_radix(sum, 16)
                    .map_err(|_| ParseError::Record { line: line_no, field: "checksum" })?;
                if count != records.len() {
                    return Err(ParseError::Count { records: records.len() });
                }
                if sum != fnv1a(&bytes[..consumed]) {
                    return Err(ParseError::Checksum);
                }
                return Ok(Self { records });
            }
            if records.len() == MAX_RECORDS {
                return Err(ParseError::TooMany);
            }
            records.push(Record::parse(body, line_no)?);
            consumed += line.len();
        }
        Err(ParseError::Truncated)
    }
}

impl Record {
    fn parse(line: &str, line_no: usize) -> Result<Self, ParseError> {
        let bad = |field| ParseError::Record { line: line_no, field };
        let mut fields = line.splitn(7, ' ');
        let mut next = |field| fields.next().filter(|f| !f.is_empty()).ok_or_else(|| bad(field));
        let machine = u64::from_str_radix(next("machine")?, 16).map_err(|_| bad("machine"))?;
        let preset = next("preset")?;
        let mem = next("memory")?.parse().map_err(|_| bad("memory"))?;
        let cpus = next("processors")?.parse().map_err(|_| bad("processors"))?;
        let tier = next("tier")?;
        let millis: u64 = next("milliseconds")?.parse().map_err(|_| bad("milliseconds"))?;
        let test = next("test")?;
        if millis > MAX_MS || test.chars().any(char::is_control) {
            return Err(bad(if millis > MAX_MS { "milliseconds" } else { "test" }));
        }
        Ok(Self {
            key: Key { machine, preset: preset.into(), mem, cpus, tier: tier.into() },
            test: test.into(),
            millis,
        })
    }
}

/// The timings with the bookkeeping of when to write them out.
#[derive(Debug, Default)]
pub struct Store {
    timings: Timings,
    dirty: bool,
    last_save: Option<Instant>,
}

impl Store {
    #[must_use]
    pub fn new(timings: Timings) -> Self {
        Self { timings, dirty: false, last_save: None }
    }

    #[must_use]
    pub const fn timings(&self) -> &Timings {
        &self.timings
    }

    pub fn record(&mut self, key: &Key, test: &str, took: Duration) {
        self.timings.record(key, test, took);
        self.dirty = true;
    }

    /// The file to write now, if there is a change to write and either `force` or [`SAVE_EVERY`] has
    /// passed since the last write. Taking it counts it as written; [`Store::unsaved`] undoes that.
    pub fn take_save(&mut self, now: Instant, force: bool) -> Option<String> {
        let due = self.last_save.is_none_or(|at| now - at >= SAVE_EVERY);
        (self.dirty && (force || due)).then(|| {
            self.dirty = false;
            self.last_save = Some(now);
            self.timings.encode()
        })
    }

    /// The write of the file last taken failed, so there is still something to write.
    pub fn unsaved(&mut self) {
        self.dirty = true;
    }
}

/// Where the timings are written.
pub trait Disk {
    fn write(&mut self, bytes: &[u8]) -> Result<(), uefi::Error>;
}

/// The boot volume.
pub struct BootVolume;

impl Disk for BootVolume {
    fn write(&mut self, bytes: &[u8]) -> Result<(), uefi::Error> {
        bootlog::replace_file(PATH, bytes)
    }
}

/// The timings on the boot volume; none, with a line saying why, when there is no usable file.
#[must_use]
pub fn load() -> Timings {
    from_file(volume::read_file(PATH))
}

fn from_file(file: Result<Vec<u8>, ReadError>) -> Timings {
    match file {
        Ok(bytes) => match Timings::parse(&bytes) {
            Ok(timings) => {
                log::info!("timings: {} records from {PATH}", timings.len());
                timings
            },
            Err(e) => {
                log::warn!("timings: ignoring {PATH}: {e}");
                Timings::default()
            },
        },
        Err(ReadError::NotFound { .. }) => {
            log::info!("timings: no {PATH}, the first pass learns them");
            Timings::default()
        },
        Err(e) => {
            log::warn!("timings: {e}, the first pass learns them");
            Timings::default()
        },
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:016x} {} {} MiB {} cpus {}",
            self.machine,
            self.preset,
            self.mem * (Self::MEM_UNIT as u64 >> 20),
            self.cpus,
            self.tier
        )
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::{fixture, rstest};

    use super::*;

    fn key(preset: &str) -> Key {
        Key::new(0xfeed_beef_0123_4567, preset, 15_942_901_760, 8, "avx2")
    }

    #[fixture]
    fn two() -> Timings {
        let mut t = Timings::default();
        t.record(&key("standard"), "bus stress, r/w turnaround", Duration::from_millis(70_809));
        t.record(&key("memtest86-plus"), "bit fade", Duration::from_millis(485_968));
        t
    }

    #[rstest]
    fn a_file_reads_back_as_it_was_written(two: Timings) {
        let text = two.encode();
        check!(
            text.starts_with(
                "ferrite-timings 1\nfeedbeef01234567 standard 59 8 avx2 70809 bus stress, r/w turnaround\n"
            )
        );
        assert!(let Ok(back) = Timings::parse(text.as_bytes()));
        check!(back == two);
        check!(back.get(&key("memtest86-plus"), "bit fade") == Some(Duration::from_millis(485_968)));
        check!(back.get(&key("standard"), "bit fade").is_none());
    }

    #[test]
    fn a_setup_is_told_apart_by_every_part_of_its_key() {
        let mut t = Timings::default();
        t.record(&key("standard"), "x", Duration::from_secs(1));
        for other in [
            Key::new(1, "standard", 15_942_901_760, 8, "avx2"),
            Key::new(0xfeed_beef_0123_4567, "standard", 8 << 30, 8, "avx2"),
            Key::new(0xfeed_beef_0123_4567, "standard", 15_942_901_760, 4, "avx2"),
            Key::new(0xfeed_beef_0123_4567, "standard", 15_942_901_760, 8, "sse2"),
        ] {
            check!(t.get(&other, "x").is_none(), "{other}");
        }
        // The same memory to within a few percent is the same setup.
        let nearly = Key::new(0xfeed_beef_0123_4567, "standard", 15_900_000_000, 8, "avx2");
        check!(t.get(&nearly, "x") == Some(Duration::from_secs(1)));
    }

    #[test]
    fn recording_again_replaces_and_the_oldest_go_first() {
        let mut t = Timings::default();
        t.record(&key("standard"), "a", Duration::from_secs(1));
        t.record(&key("standard"), "b", Duration::from_secs(2));
        t.record(&key("standard"), "a", Duration::from_secs(3));
        check!(t.len() == 2);
        check!(t.get(&key("standard"), "a") == Some(Duration::from_secs(3)));
        for n in 0..MAX_RECORDS {
            t.record(&key("standard"), &alloc::format!("t{n}"), Duration::from_secs(1));
        }
        check!(t.len() == MAX_RECORDS);
        check!(t.get(&key("standard"), "a").is_none() && t.get(&key("standard"), "b").is_none());
        check!(t.get(&key("standard"), "t0").is_some());
    }

    #[rstest]
    #[case::empty(b"".as_slice(), ParseError::Header)]
    #[case::not_ours(b"hello\n", ParseError::Header)]
    #[case::binary(b"\xff\xfe\x00", ParseError::Encoding)]
    #[case::newer(b"ferrite-timings 2\n", ParseError::Version { found: "2".into() })]
    #[case::no_records_no_end(b"ferrite-timings 1\n", ParseError::Truncated)]
    #[case::end_without_newline(b"ferrite-timings 1\nend 0 0000000000000000", ParseError::Truncated)]
    fn files_that_are_not_ours_or_not_whole_are_refused(#[case] bytes: &[u8], #[case] why: ParseError) {
        check!(Timings::parse(bytes) == Err(why));
    }

    #[rstest]
    fn damage_is_found_wherever_it_is(two: Timings) {
        let text = two.encode();
        // A power cut mid-write: every proper prefix is refused.
        for cut in 0..text.len() {
            check!(Timings::parse(&text.as_bytes()[..cut]).is_err(), "prefix of {cut} bytes");
        }
        // A flipped digit in a time or in the key, and the checksum notices.
        let flipped = text.replacen("70809", "70808", 1);
        check!(Timings::parse(flipped.as_bytes()) == Err(ParseError::Checksum));
        let swapped = text.replacen(" 59 ", " 60 ", 1);
        check!(Timings::parse(swapped.as_bytes()) == Err(ParseError::Checksum));
        // A count that disagrees with the records.
        let lines: Vec<&str> = text.lines().collect();
        let miscount = alloc::format!("{}\n{}\n{}\nend 1 0\n", lines[0], lines[1], lines[2]);
        check!(Timings::parse(miscount.as_bytes()) == Err(ParseError::Count { records: 2 }));
    }

    #[rstest]
    #[case::short_line("machine")]
    #[case::bad_hex("zz standard 62 8 avx2 5 name")]
    #[case::bad_number("feedbeef01234567 standard x 8 avx2 5 name")]
    #[case::huge_time("feedbeef01234567 standard 62 8 avx2 99999999999999999999 name")]
    #[case::beyond_a_month("feedbeef01234567 standard 62 8 avx2 99999999999 name")]
    #[case::no_name("feedbeef01234567 standard 62 8 avx2 5 ")]
    fn a_bad_record_is_named_by_its_line(#[case] record: &str) {
        let mut body = alloc::format!("{HEADER} 1\n{record}\n");
        let sum = fnv1a(body.as_bytes());
        let _ = writeln!(body, "end 1 {sum:016x}");
        check!(matches!(Timings::parse(body.as_bytes()), Err(ParseError::Record { line: 2, .. })));
    }

    #[test]
    fn a_file_of_nothing_but_noise_is_refused_without_panicking() {
        // A cheap stand-in for a fuzzer: every byte pattern of a few shapes.
        for seed in 0..2000_u32 {
            let bytes: Vec<u8> =
                (0..seed % 97).map(|i| (seed.wrapping_mul(2_654_435_761).rotate_left(i) >> 8) as u8).collect();
            check!(Timings::parse(&bytes).is_err());
        }
        check!(Timings::parse(&alloc::vec![b'a'; MAX_FILE + 1]) == Err(ParseError::TooLarge));
    }

    #[test]
    fn writes_are_rate_limited_but_a_forced_one_goes_at_once() {
        let at = |s| Instant::from_epoch(Duration::from_secs(s));
        let mut store = Store::default();
        check!(store.take_save(at(0), false).is_none(), "nothing to write");
        store.record(&key("standard"), "a", Duration::from_secs(1));
        check!(store.take_save(at(1), false).is_some(), "the first is never early");
        store.record(&key("standard"), "b", Duration::from_secs(1));
        check!(store.take_save(at(5), false).is_none());
        check!(store.take_save(at(11), false).is_some());
        store.record(&key("standard"), "c", Duration::from_secs(1));
        check!(store.take_save(at(12), true).is_some(), "the end of the run does not wait");
        store.unsaved();
        check!(store.take_save(at(40), false).is_some(), "a failed write is tried again");
    }
}
