use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::ToString as _;
use core::cell::{Cell, RefCell};
use core::time::Duration;

use assert2::{assert, check};
use rstest::rstest;
use uefi::Status;

use super::*;

/// A directory in memory that the test keeps a handle on.
#[derive(Clone, Default)]
struct Fake {
    files: Rc<RefCell<BTreeMap<String, String>>>,
    read_only: Rc<Cell<bool>>,
}

impl Fake {
    fn with(names: &[&str]) -> Self {
        let fake = Self::default();
        for name in names {
            fake.files.borrow_mut().insert((*name).into(), "someone else's".into());
        }
        fake
    }

    fn saver(&self) -> Saver {
        Saver::new(Box::new(self.clone()))
    }

    fn listed(&self) -> Vec<String> {
        self.files.borrow().keys().cloned().collect()
    }
}

impl Volume for Fake {
    fn names(&mut self) -> Result<Vec<String>, ReadError> {
        Ok(self.listed())
    }

    fn write(&mut self, name: &str, bytes: &[u8]) -> Result<(), uefi::Error> {
        if self.read_only.get() {
            return Err(Status::WRITE_PROTECTED.into());
        }
        self.files.borrow_mut().insert(name.into(), String::from_utf8_lossy(bytes).into_owned());
        Ok(())
    }
}

const FIRST_RUN: Option<Instant> = Some(Instant::from_epoch(Duration::from_secs(100)));
const RESTARTED: Option<Instant> = Some(Instant::from_epoch(Duration::from_mins(15)));

const STAMP: &str = "2026-11-14_0523";

#[rstest]
#[case::synced_and_free(Some(STAMP), &[], "2026-11-14_0523.txt")]
#[case::another_run_that_minute(Some(STAMP), &["2026-11-14_0523.txt"], "2026-11-14_0523-2.txt")]
#[case::and_a_third(Some(STAMP), &["2026-11-14_0523.txt", "2026-11-14_0523-2.txt"], "2026-11-14_0523-3.txt")]
#[case::fat_ignores_case(Some(STAMP), &["2026-11-14_0523.TXT"], "2026-11-14_0523-2.txt")]
#[case::counters_are_no_obstacle_to_a_stamp(Some(STAMP), &["run-0001.txt"], "2026-11-14_0523.txt")]
#[case::unsynced_and_first(None, &[], "run-0001.txt")]
#[case::unsynced_after_the_highest(None, &["run-0002.txt", "run-0006.txt", "notes.txt", "run-x.txt", "2026-11-14_0523.txt"], "run-0007.txt")]
#[case::unsynced_counts_on_past_a_gap_left_by_a_deleted_file(None, &["run-0009.TXT"], "run-0010.txt")]
fn a_runs_file_is_named_by_the_clock_or_a_counter_and_never_takes_a_taken_name(
    #[case] stamp: Option<&str>,
    #[case] taken: &[&str],
    #[case] name: &str,
) {
    let taken: Vec<String> = taken.iter().map(|name| String::from(*name)).collect();
    check!(choose_name(stamp, &taken) == name);
}

#[test]
fn every_report_of_a_run_replaces_its_file_and_another_run_gets_its_own() {
    let fake = Fake::with(&["2026-11-14_0523.txt"]);
    let mut saver = fake.saver();

    assert!(let Ok(first) = saver.save(FIRST_RUN, Some(STAMP), "early"));
    check!(first.path == "\\EFI\\ferrite\\reports\\2026-11-14_0523-2.txt");
    check!(first.bytes == 5);
    // F10 again, and the run's end, from a clock that has synced to another minute meanwhile.
    assert!(let Ok(again) = saver.save(FIRST_RUN, Some("2026-11-14_0601"), "later and longer"));
    check!(again.path == first.path);
    check!(fake.files.borrow()["2026-11-14_0523-2.txt"] == "later and longer");
    check!(fake.listed() == ["2026-11-14_0523-2.txt", "2026-11-14_0523.txt"]);
    check!(fake.files.borrow()["2026-11-14_0523.txt"] == "someone else's", "another run's file is untouched");

    // A restart is a new run and starts a file of its own.
    assert!(let Ok(restarted) = saver.save(RESTARTED, Some("2026-11-14_0601"), "after the restart"));
    check!(restarted.path == "\\EFI\\ferrite\\reports\\2026-11-14_0601.txt");
    check!(fake.listed().len() == 3);
}

#[test]
fn a_run_first_saved_before_the_clock_synced_keeps_its_counter_name() {
    let fake = Fake::with(&["run-0004.txt"]);
    let mut saver = fake.saver();
    assert!(let Ok(first) = saver.save(None, None, "a"));
    check!(first.path.ends_with("run-0005.txt"));
    assert!(let Ok(synced) = saver.save(None, Some(STAMP), "b"));
    check!(synced.path == first.path);
    check!(fake.listed() == ["run-0004.txt", "run-0005.txt"]);
}

#[test]
fn a_write_that_fails_is_an_error_and_the_next_report_chooses_its_name_afresh() {
    let fake = Fake::default();
    let mut saver = fake.saver();
    fake.read_only.set(true);
    assert!(let Err(e) = saver.save(FIRST_RUN, Some(STAMP), "text"));
    check!(matches!(e, SaveError::Write { .. }));
    check!(e.to_string().starts_with("cannot write \\EFI\\ferrite\\reports\\2026-11-14_0523.txt"), "{e}");
    check!(fake.listed().is_empty());

    // Someone else's report took the name meanwhile; this run must not overwrite it.
    fake.files.borrow_mut().insert("2026-11-14_0523.txt".into(), "someone else's".into());
    fake.read_only.set(false);
    assert!(let Ok(written) = saver.save(FIRST_RUN, Some(STAMP), "text"));
    check!(written.path.ends_with("2026-11-14_0523-2.txt"));
}
