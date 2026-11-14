//! The app's side of push notifications.
//!
//! Reads the settings from the boot volume, notices in the sweep's history what is worth telling, and
//! words it. The queue, the retries and the HTTP are `ferrite_net::notify`'s.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::time::Duration;

use ferrite_efi::volume::{self, ReadError};
use ferrite_hal::Instant;
use ferrite_net::mdns::HOST;
use ferrite_net::notify::{Config, Event, Notification, Notifier};

use crate::report::{FaultRecord, PassRecord, Snapshot};
use crate::sweep::SweepStatus;

/// Where the settings live on the boot volume.
const CONFIG_PATH: &str = "\\EFI\\ferrite\\notify.env";

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

/// The notifier the settings on the boot volume describe; `None`, with a log line saying why, when
/// there is no file or it is not usable.
pub fn load() -> Option<Notifier> {
    notifier(volume::read_file(CONFIG_PATH))
}

fn notifier(file: Result<Vec<u8>, ReadError>) -> Option<Notifier> {
    let text = match file {
        Ok(text) => text,
        Err(ReadError::NotFound { .. }) => {
            log::info!("notify: no {CONFIG_PATH}, notifications are off");
            return None;
        },
        Err(e) => {
            log::warn!("notify: {e}, notifications are off");
            return None;
        },
    };
    match Config::from_env(&text) {
        Ok(config) => {
            log::info!(
                "notify: on, {:?} to {} port {} for {}",
                config.format,
                config.endpoint.host,
                config.endpoint.port,
                config.events
            );
            Some(Notifier::new(config))
        },
        Err(e) => {
            log::warn!("notify: {CONFIG_PATH}: {e}, notifications are off");
            None
        },
    }
}

/// Notices events in the sweep's snapshots, one call at a time.
#[derive(Debug, Default)]
pub struct Watch {
    /// The newest pass reported, until a restart empties the history.
    pass: Option<u64>,
    /// The first fault was reported, until a restart clears it.
    fault: bool,
}

impl Watch {
    #[must_use]
    pub const fn new() -> Self {
        Self { pass: None, fault: false }
    }

    /// Hand `send` a notification for each pass that finished and for the run's first fault since
    /// the last call. Costs a comparison or two when nothing happened.
    pub fn observe(&mut self, now: Instant, snapshot: &Snapshot<'_>, send: &mut dyn FnMut(Notification)) {
        let history = snapshot.history;
        match history.passes.back() {
            None => self.pass = None,
            Some(newest) if self.pass != Some(newest.pass) => {
                let elapsed = history.run_started.map_or(Duration::ZERO, |started| now - started);
                for record in history.passes.iter().filter(|r| self.pass.is_none_or(|seen| r.pass > seen)) {
                    send(pass_done(record, &snapshot.status, elapsed));
                }
                self.pass = Some(newest.pass);
            },
            Some(_) => {},
        }
        match snapshot.status.first_fault {
            Some(first) if !self.fault => {
                self.fault = true;
                send(first_fault(&first));
            },
            None => self.fault = false,
            Some(_) => {},
        }
    }
}

fn pass_done(record: &PassRecord, status: &SweepStatus, elapsed: Duration) -> Notification {
    Notification::new(
        Event::Pass,
        format!("ferrite: pass {} done", record.pass + 1),
        format!(
            "ferrite: pass {} done on {HOST}, {}, {}, {}",
            record.pass + 1,
            errors(status.faults),
            size(status.bytes as u64),
            clock(elapsed)
        ),
    )
}

fn first_fault(first: &FaultRecord) -> Notification {
    Notification::new(
        Event::Fault,
        "ferrite: memory error",
        format!(
            "ferrite: first memory error on {HOST}: {} on cpu {}, pass {}, at {}, expected {:#018x}, read {:#018x}",
            first.test,
            first.cpu,
            first.pass + 1,
            first.fault.addr,
            first.fault.expected,
            first.fault.actual
        ),
    )
}

/// The run stopped, by `q`, the desktop or the status page, `now` after it started.
pub fn run_ended(now: Instant, snapshot: &Snapshot<'_>) -> Notification {
    let status = &snapshot.status;
    let elapsed = snapshot.history.run_started.map_or(Duration::ZERO, |started| now - started);
    Notification::new(
        Event::End,
        "ferrite: run ended",
        format!(
            "ferrite: run ended on {HOST} after {} passes, {}, {}, {}",
            status.passes,
            errors(status.faults),
            size(status.bytes as u64),
            clock(elapsed)
        ),
    )
}

fn errors(count: u64) -> String {
    if count == 1 { String::from("1 error") } else { format!("{count} errors") }
}

/// `15.9 GiB`, or whole MiB below a GiB.
fn size(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{}.{} GiB", bytes / GIB, bytes % GIB * 10 / GIB)
    } else {
        format!("{} MiB", bytes / MIB)
    }
}

/// `41:12`, or `1:02:03` from an hour on.
fn clock(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    let (hours, minutes, seconds) = (secs / 3600, secs / 60 % 60, secs % 60);
    if hours > 0 { format!("{hours}:{minutes:02}:{seconds:02}") } else { format!("{minutes}:{seconds:02}") }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use alloc::vec::Vec;

    use assert2::{assert, check};
    use ferrite_mem::{CpuAddr, Fault};
    use rstest::{fixture, rstest};

    use super::*;
    use crate::net::testing::idle_status;
    use crate::pace::TestStats;
    use crate::report::History;

    #[rstest]
    #[case(0, "0 MiB")]
    #[case(1 << 20, "1 MiB")]
    #[case((1 << 30) - 1, "1023 MiB")]
    #[case(1 << 30, "1.0 GiB")]
    #[case((15 << 30) + 1_000_000_000, "15.9 GiB")]
    #[case(32 << 30, "32.0 GiB")]
    fn sizes(#[case] bytes: u64, #[case] want: &str) {
        check!(size(bytes) == want);
    }

    #[rstest]
    #[case(0, "0:00")]
    #[case(59, "0:59")]
    #[case(41 * 60 + 12, "41:12")]
    #[case(3600, "1:00:00")]
    #[case(26 * 3600 + 62, "26:01:02")]
    fn clocks(#[case] secs: u64, #[case] want: &str) {
        check!(clock(Duration::from_secs(secs)) == want);
    }

    fn at(secs: u64) -> Instant {
        Instant::from_epoch(Duration::from_secs(secs))
    }

    fn fault() -> FaultRecord {
        let addr = CpuAddr::of(core::ptr::without_provenance::<u8>(0x4000_1008));
        FaultRecord {
            fault: Fault { addr, expected: !0, actual: !0 ^ 0x10 },
            test: "moving-inversions",
            cpu: 2,
            pass: 1,
        }
    }

    fn pass(n: u64) -> PassRecord {
        PassRecord { pass: n, took: Duration::from_secs(60), faults: 0 }
    }

    /// Feeds snapshots to a [`Watch`] and keeps what it sent.
    struct Rig {
        watch: Watch,
        history: History,
        status: SweepStatus,
        faults: VecDeque<FaultRecord>,
        sent: Vec<Notification>,
    }

    impl Rig {
        fn new() -> Self {
            let mut status = idle_status();
            status.bytes = 16 << 30;
            let mut history = History::default();
            history.run_started = Some(at(0));
            Self { watch: Watch::new(), history, status, faults: VecDeque::new(), sent: Vec::new() }
        }

        fn observe(&mut self, secs: u64) -> Vec<Notification> {
            let snapshot = Snapshot {
                status: self.status,
                history: &self.history,
                faults: &self.faults,
                stats: &TestStats::default(),
            };
            let mut sent = Vec::new();
            self.watch.observe(at(secs), &snapshot, &mut |n| sent.push(n));
            self.sent.extend(sent.iter().cloned());
            sent
        }
    }

    #[fixture]
    fn rig() -> Rig {
        Rig::new()
    }

    #[rstest]
    fn a_finished_pass_is_told_once_with_the_run_so_far(mut rig: Rig) {
        check!(rig.observe(10).is_empty());

        rig.history.passes.push_back(pass(2));
        rig.status.faults = 0;
        let sent = rig.observe(41 * 60 + 12);
        assert!(let [note] = sent.as_slice());
        check!(note.event == Event::Pass);
        check!(note.title == "ferrite: pass 3 done");
        check!(note.message == "ferrite: pass 3 done on ferrite.local, 0 errors, 16.0 GiB, 41:12");
        check!(rig.observe(41 * 60 + 13).is_empty());
    }

    #[rstest]
    fn passes_missed_between_calls_are_each_told_in_order(mut rig: Rig) {
        rig.history.passes.extend([pass(0), pass(1)]);
        let sent = rig.observe(5);
        check!(
            sent.iter().map(|n| n.title.as_str()).collect::<Vec<_>>()
                == ["ferrite: pass 1 done", "ferrite: pass 2 done"]
        );
        rig.history.passes.push_back(pass(2));
        check!(rig.observe(6).len() == 1);
    }

    #[rstest]
    fn only_the_first_fault_of_a_run_is_told_and_a_restart_rearms_it(mut rig: Rig) {
        rig.status.first_fault = Some(fault());
        rig.status.faults = 7;
        let sent = rig.observe(1);
        assert!(let [note] = sent.as_slice());
        check!(note.event == Event::Fault);
        check!(
            note.message
                == "ferrite: first memory error on ferrite.local: moving-inversions on cpu 2, pass 2, at 0x40001008, \
                    expected 0xffffffffffffffff, read 0xffffffffffffffef"
        );
        rig.status.faults = 9;
        check!(rig.observe(2).is_empty());

        rig.status.first_fault = None;
        rig.history.reset();
        check!(rig.observe(3).is_empty());
        rig.status.first_fault = Some(fault());
        check!(rig.observe(4).len() == 1);
    }

    #[rstest]
    fn a_restart_starts_counting_passes_again(mut rig: Rig) {
        rig.history.passes.extend([pass(0), pass(1)]);
        rig.observe(1);
        rig.history.reset();
        check!(rig.observe(2).is_empty());
        rig.history.passes.push_back(pass(0));
        let sent = rig.observe(3);
        assert!(let [note] = sent.as_slice());
        check!(note.title == "ferrite: pass 1 done");
    }

    #[rstest]
    fn the_end_says_how_the_run_went(mut rig: Rig) {
        rig.status.passes = 4;
        rig.status.faults = 1;
        let snapshot =
            Snapshot { status: rig.status, history: &rig.history, faults: &rig.faults, stats: &TestStats::default() };
        let note = run_ended(at(3 * 3600 + 5), &snapshot);
        check!(note.event == Event::End);
        check!(note.message == "ferrite: run ended on ferrite.local after 4 passes, 1 error, 16.0 GiB, 3:00:05");
    }

    #[rstest]
    #[case::missing_file(Err(ReadError::NotFound { path: "x".into() }), false)]
    #[case::bad_url(Ok(b"NOTIFY_URL=https://ntfy.sh/t".to_vec()), false)]
    #[case::no_url(Ok(b"# nothing\n".to_vec()), false)]
    #[case::good_file(Ok(b"NOTIFY_URL=http://192.0.2.5:8080/t".to_vec()), true)]
    fn a_missing_or_bad_file_turns_notifications_off(#[case] file: Result<Vec<u8>, ReadError>, #[case] on: bool) {
        check!(notifier(file).is_some() == on);
    }
}
