//! Giving a claimed USB interface back to the kernel driver it was taken from.

use snafu::Snafu;

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum ClaimError {
    #[snafu(display("the device is gone"))]
    Gone,

    #[snafu(display("{reason}"))]
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Snafu)]
pub enum HandBackError {
    #[snafu(display("cannot release the claimed interface"))]
    Release { source: ClaimError },

    #[snafu(display("cannot reattach the kernel driver"))]
    Reattach { source: ClaimError },
}

/// An interface taken from the kernel, as the guard needs to see it.
pub trait Claim {
    /// Release the interface so the kernel may bind a driver again.
    fn release(&mut self) -> Result<(), ClaimError>;
    /// Ask the kernel to bind its driver to the interface.
    fn reattach(&mut self) -> Result<(), ClaimError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandedBack {
    Reattached,
    /// The device was unplugged, so the kernel re-enumerates it by itself.
    DeviceGone,
}

fn hand_back(claim: &mut impl Claim) -> Result<HandedBack, HandBackError> {
    match claim.release() {
        Ok(()) => {},
        Err(ClaimError::Gone) => return Ok(HandedBack::DeviceGone),
        Err(source) => return Err(HandBackError::Release { source }),
    }
    match claim.reattach() {
        Ok(()) => Ok(HandedBack::Reattached),
        Err(ClaimError::Gone) => Ok(HandedBack::DeviceGone),
        Err(source) => Err(HandBackError::Reattach { source }),
    }
}

/// Hands the claim back when dropped, and says how to recover by hand if that fails.
pub struct ClaimGuard<C: Claim> {
    claim: C,
    fix: String,
    report: Box<dyn Fn(&str)>,
    done: bool,
}

impl<C: Claim> ClaimGuard<C> {
    /// `fix` is the recovery advice printed after a failed hand back.
    pub fn new(claim: C, fix: impl Into<String>) -> Self {
        Self { claim, fix: fix.into(), report: Box::new(|text| eprintln!("{text}")), done: false }
    }

    #[must_use]
    pub fn reporting_to(mut self, report: impl Fn(&str) + 'static) -> Self {
        self.report = Box::new(report);
        self
    }

    pub fn claim_mut(&mut self) -> &mut C {
        &mut self.claim
    }

    /// Hand back now and learn the result; dropping afterwards does nothing more.
    pub fn finish(mut self) -> Result<HandedBack, HandBackError> {
        self.done = true;
        hand_back(&mut self.claim)
    }
}

impl<C: Claim> Drop for ClaimGuard<C> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        if let Err(e) = hand_back(&mut self.claim) {
            (self.report)(&format!("ferrite-dev: {}\n{}", crate::chain(&e), self.fix));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use assert2::{assert, check};

    use super::*;

    type Log = Rc<RefCell<Vec<String>>>;

    struct FakeClaim {
        log: Log,
        release: Result<(), ClaimError>,
        reattach: Result<(), ClaimError>,
    }

    impl FakeClaim {
        fn new(log: &Log) -> Self {
            Self { log: Rc::clone(log), release: Ok(()), reattach: Ok(()) }
        }
    }

    impl Claim for FakeClaim {
        fn release(&mut self) -> Result<(), ClaimError> {
            self.log.borrow_mut().push("release".into());
            self.release.clone()
        }

        fn reattach(&mut self) -> Result<(), ClaimError> {
            self.log.borrow_mut().push("reattach".into());
            self.reattach.clone()
        }
    }

    fn failed(reason: &str) -> ClaimError {
        ClaimError::Failed { reason: reason.into() }
    }

    fn guard(claim: FakeClaim, reports: &Log) -> ClaimGuard<FakeClaim> {
        let reports = Rc::clone(reports);
        ClaimGuard::new(claim, "replug the cable").reporting_to(move |text| reports.borrow_mut().push(text.into()))
    }

    #[test]
    fn dropping_releases_then_reattaches_and_stays_quiet() {
        let (log, reports) = (Log::default(), Log::default());
        drop(guard(FakeClaim::new(&log), &reports));
        check!(*log.borrow() == ["release", "reattach"]);
        check!(reports.borrow().is_empty());
    }

    #[test]
    fn finishing_hands_back_once_even_though_the_guard_is_dropped_after() {
        let (log, reports) = (Log::default(), Log::default());
        check!(guard(FakeClaim::new(&log), &reports).finish() == Ok(HandedBack::Reattached));
        check!(*log.borrow() == ["release", "reattach"]);
    }

    #[test]
    fn a_failed_reattach_is_reported_with_its_cause_and_the_fix() {
        let (log, reports) = (Log::default(), Log::default());
        let mut claim = FakeClaim::new(&log);
        claim.reattach = Err(failed("kernel said no"));
        drop(guard(claim, &reports));
        assert!(reports.borrow().len() == 1);
        let text = reports.borrow()[0].clone();
        check!(text == "ferrite-dev: cannot reattach the kernel driver: kernel said no\nreplug the cable");
    }

    #[test]
    fn a_failed_release_skips_the_reattach_and_is_reported() {
        let (log, reports) = (Log::default(), Log::default());
        let mut claim = FakeClaim::new(&log);
        claim.release = Err(failed("interface still in use"));
        drop(guard(claim, &reports));
        check!(*log.borrow() == ["release"]);
        check!(reports.borrow()[0].contains("cannot release the claimed interface: interface still in use"));
    }

    #[test]
    fn an_unplugged_device_needs_no_reattach_and_no_message() {
        let (log, reports) = (Log::default(), Log::default());
        let mut claim = FakeClaim::new(&log);
        claim.release = Err(ClaimError::Gone);
        check!(guard(claim, &reports).finish() == Ok(HandedBack::DeviceGone));
        check!(*log.borrow() == ["release"]);

        let mut claim = FakeClaim::new(&log);
        claim.reattach = Err(ClaimError::Gone);
        drop(guard(claim, &reports));
        check!(reports.borrow().is_empty());
    }
}
