//! The DbC state machine against the simulated controller in `ferrite-testing`.

use std::time::Duration;

use assert2::{assert, check};
use ferrite_testing::{DBC_DEV, Sim, SimClock, SimHal, SimRegion};
use ferrite_xhci::{Config, Dbc, Error, State, XhciDevice, XhciFunction};

type SimDbc = Dbc<SimHal, SimClock, SimRegion>;

const DCE: u32 = 1 << 31;

fn device(sim: &Sim) -> XhciDevice {
    let mut hal = sim.hal();
    assert!(let Ok(Some(func)) = XhciFunction::identify(&mut hal, DBC_DEV));
    assert!(let Ok(dev) = XhciDevice::probe(&mut hal, func));
    dev
}

fn open_with(sim: &Sim, cfg: &Config) -> Result<SimDbc, Error> {
    Dbc::open(sim.hal(), sim.clock(), &mut sim.dma(), &device(sim), cfg)
}

fn open(sim: &Sim) -> SimDbc {
    assert!(let Ok(dbc) = open_with(sim, &Config::default()));
    dbc
}

fn configured(sim: &Sim) -> SimDbc {
    let mut dbc = open(sim);
    sim.connect();
    dbc.poll(|_| {});
    sim.configure();
    dbc.poll(|_| {});
    assert!(dbc.state() == State::Configured);
    dbc
}

fn assert_bytes_balance(dbc: &SimDbc) {
    let s = dbc.stats();
    check!(s.bytes_queued == s.bytes_sent + s.bytes_dropped + dbc.buffered() as u64);
}

#[test]
fn arm_connect_configure_and_echo() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = open(&sim);
    check!(dbc.state() == State::Armed);
    check!(sim.dcctrl() & DCE != 0);

    sim.connect();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Armed, "connected but not yet configured");
    check!(dbc.timings().connected.is_some());

    sim.configure();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Configured);
    check!(dbc.debug_port().map(ferrite_xhci::PortNumber::get) == Some(1));

    let w = dbc.write(b"hello");
    check!(w.accepted == 5 && w.is_complete());
    let (_, sent) = sim.complete_out().expect("an OUT TRB was posted");
    check!(sent == b"hello");

    assert!(sim.deliver_in(b"ping"));
    let mut got = Vec::new();
    dbc.poll(|b| got.extend_from_slice(b));
    check!(got == b"ping");
    check!(dbc.stats().bytes_sent == 5);
    check!(dbc.stats().bytes_received == 4);
    check!(dbc.stats().unknown_transfer_events == 0);
    assert_bytes_balance(&dbc);
}

#[test]
fn debug_port_reset_rearms_without_trusting_stale_dcr() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    sim.port_reset();
    // The host re-enumerates before the next poll, so DCCTRL already reads DCR=1 again.
    sim.configure();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Armed, "the reset is handled before DCR is believed");
    check!(dbc.stats().rearms == 1);
    check!(sim.dcportsc() & (1 << 21) == 0, "PRC acknowledged");

    dbc.poll(|_| {});
    check!(dbc.state() == State::Configured);
}

#[test]
fn stale_event_after_rearm_retires_nothing() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    for chunk in [b"aaaaaaaaaa", b"bbbbbbbbbb", b"cccccccccc"] {
        check!(dbc.write(chunk).is_complete());
    }
    let (first, _) = sim.complete_out().expect("TRB 0");
    let (_, _) = sim.complete_out().expect("TRB 1");
    let (third, _) = sim.complete_out().expect("TRB 2");
    check!(third == first + 32);
    dbc.poll(|_| {});
    check!(dbc.buffered() == 0);

    check!(dbc.write(b"dddddddddd").is_complete());
    sim.port_reset();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Armed);
    sim.configure();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Configured);

    // An event for the old lap's third TRB arrives late.
    assert!(sim.push_transfer_event(third, ferrite_testing::CC_SUCCESS, 0));
    dbc.poll(|_| {});
    check!(dbc.stats().unknown_transfer_events == 1);
    check!(dbc.buffered() == 10, "the requeued bytes are still waiting");

    let (_, resent) = sim.complete_out().expect("requeued TRB");
    check!(resent == b"dddddddddd");
    dbc.poll(|_| {});
    check!(dbc.buffered() == 0);
    check!(dbc.stats().bytes_dropped == 0);
    assert_bytes_balance(&dbc);
}

#[test]
fn kick_keeps_bytes_written_before_configuration() {
    let sim = Sim::with_dbc_controller();
    let cfg = Config { timeout: Duration::from_secs(5), ..Config::default() };
    let mut dbc = open_with(&sim, &cfg).expect("open");
    check!(dbc.write(b"early").is_complete());

    let err = dbc.wait_configured().expect_err("no host ever answers");
    assert!(let Error::RegisterTimeout { reg: "DCCTRL", .. } = err);
    check!(dbc.stats().kicks >= 2);
    check!(dbc.state() == State::Armed);

    sim.connect();
    dbc.poll(|_| {});
    sim.configure();
    dbc.poll(|_| {});
    let (_, sent) = sim.complete_out().expect("an OUT TRB");
    check!(sent == b"early");
    check!(dbc.stats().bytes_dropped == 0);
}

#[test]
fn dce_stuck_on_drop_leaks_instead_of_freeing() {
    let sim = Sim::with_dbc_controller();
    let dbc = configured(&sim);
    sim.set_dce_stuck(true);
    drop(dbc);
    check!(sim.dma_freed() == 0);
    check!(sim.dma_live() == 1);

    let sim = Sim::with_dbc_controller();
    drop(configured(&sim));
    check!(sim.dma_freed() == 1, "a clean disable frees the block");
}

#[test]
fn open_frees_memory_the_controller_never_saw() {
    let sim = Sim::with_dbc_controller();
    sim.poke(sim.dbc_base() + 0x20, DCE);
    sim.set_dce_stuck(true);
    assert!(let Err(Error::RegisterTimeout { op: "clear DCE", .. }) = open_with(&sim, &Config::default()));
    check!(sim.dma_freed() == 1, "DCE was stuck before our addresses were programmed");
}

#[test]
fn open_failing_to_set_dce_frees_once_dce_is_confirmed_clear() {
    let sim = Sim::with_dbc_controller();
    sim.set_dce_ignores_set(true);
    assert!(let Err(Error::RegisterTimeout { op: "set DCE", .. }) = open_with(&sim, &Config::default()));
    check!(sim.dma_freed() == 1);
}

#[test]
fn dce_lost_faults_then_recovers_with_backoff() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    sim.reset_controller();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Faulted);
    assert!(let Some(Error::DceLost { .. }) = dbc.last_fault());

    dbc.poll(|_| {});
    check!(dbc.state() == State::Faulted, "no retry before the backoff");

    sim.advance(Config::default().fault_retry);
    dbc.poll(|_| {});
    check!(dbc.state() == State::Armed);
    check!(dbc.last_fault().is_none());
    check!(dbc.stats().recoveries == 1);
    check!(sim.dcctrl() & DCE != 0);
}

#[test]
fn wait_configured_on_a_disabled_dbc_says_disabled() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = open(&sim);
    dbc.disable().expect("DCE clears");
    assert!(let Err(Error::Disabled) = dbc.wait_configured());
}

#[test]
fn iommu_refuses_unless_overridden() {
    let sim = Sim::with_dbc_controller();
    sim.set_iommu(true);
    assert!(let Err(Error::IommuPresent) = open_with(&sim, &Config::default()));
    check!(sim.dma_live() == 0, "refused before allocating");
    check!(open_with(&sim, &Config { allow_iommu: true, ..Config::default() }).is_ok());
}

#[test]
fn full_buffer_refuses_and_failed_transfers_drop() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    let big = vec![7u8; 20_000];
    let w = dbc.write(&big);
    check!(w.accepted == 16 * 1024);
    check!(w.refused == 20_000 - 16 * 1024);
    check!(dbc.stats().bytes_refused == w.refused as u64);

    let (_, lost) = sim.complete_out_with(4).expect("an OUT TRB");
    dbc.poll(|_| {});
    check!(dbc.stats().transfer_errors == 1);
    check!(dbc.stats().bytes_dropped == lost.len() as u64);
    assert_bytes_balance(&dbc);
}

#[test]
fn idle_poll_reads_only_dcctrl() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    dbc.poll(|_| {});
    let before = sim.mmio_reads();
    for _ in 0..10 {
        dbc.poll(|_| {});
    }
    check!(sim.mmio_reads() - before == 10);
}

#[test]
fn unplug_rearms_and_requeues() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    check!(dbc.write(b"lost?").is_complete());
    sim.unplug();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Armed);
    check!(dbc.buffered() == 5);
}
