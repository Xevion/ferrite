//! The DbC state machine against the simulated controller in `ferrite-testing`.

use std::time::Duration;

use assert2::{assert, check};
use ferrite_testing::{DBC_DEV, Sim, SimClock, SimHal, SimRegion};
use ferrite_xhci::dbc::{OUT_BYTES, OUT_MAX_TRBS, OUT_TRB_MAX};
use ferrite_xhci::{Config, Dbc, Error, InTrbSize, State, XhciDevice, XhciFunction};
use rstest::rstest;

type SimDbc = Dbc<SimHal, SimClock, SimRegion>;

/// IN TRBs kept posted at the default 1 KiB size.
const IN_SLOTS: usize = InTrbSize::Kib1.slots();

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
    configured_with(sim, &Config::default())
}

fn configured_with(sim: &Sim, cfg: &Config) -> SimDbc {
    assert!(let Ok(mut dbc) = open_with(sim, cfg));
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
    check!(sim.dma_violations() == 0, "every address the controller followed lies in our block");
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
fn events_queued_at_a_rearm_drain_before_the_new_ring_reuses_their_slots() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    check!(dbc.write(b"aaaaaaaaaa").is_complete());
    let (first, _) = sim.complete_out().expect("TRB 0");
    check!(dbc.write(b"bbbbbbbbbb").is_complete());
    sim.port_reset();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Armed);

    // The host configures again before the next poll; the event for old slot 0 is still queued.
    sim.configure();
    dbc.poll(|_| {});
    check!(dbc.state() == State::Configured);
    check!(dbc.stats().unknown_transfer_events == 1, "the old event retired nothing");

    let (at, resent) = sim.complete_out().expect("the requeued bytes as one TRB");
    check!(at == first, "the new ring reuses slot 0");
    check!(resent == b"aaaaaaaaaabbbbbbbbbb");
    dbc.poll(|_| {});
    check!(dbc.buffered() == 0);
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
fn failed_dma_allocation_touches_no_dbc_register() {
    let sim = Sim::with_dbc_controller();
    sim.set_dma_fails(true);
    assert!(let Err(Error::Dma { .. }) = open_with(&sim, &Config::default()));
    check!(sim.writes_to(sim.dbc_base() + 0x20).is_empty(), "DCCTRL untouched");
}

#[test]
fn unreachable_dcctrl_fails_open_and_frees_the_unprogrammed_block() {
    let sim = Sim::with_dbc_controller();
    sim.fail_mmio(sim.dbc_base() + 0x20);
    assert!(let Err(Error::Mmio { .. }) = open_with(&sim, &Config::default()));
    check!(sim.dma_freed() == 1);
    check!(sim.dma_live() == 0);
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
    let big = vec![7u8; OUT_BYTES + 4000];
    let w = dbc.write(&big);
    check!(w.accepted == OUT_BYTES);
    check!(w.refused == 4000);
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

/// Complete every posted OUT TRB, polling between rounds so retired slots are
/// reused and a terminator can be posted. Returns each TRB's length and the payload.
fn drain_wire(sim: &Sim, dbc: &mut SimDbc) -> (Vec<usize>, Vec<u8>) {
    let (mut lens, mut payload) = (Vec::new(), Vec::new());
    loop {
        let mut any = false;
        while let Some((_, bytes)) = sim.complete_out() {
            lens.push(bytes.len());
            payload.extend_from_slice(&bytes);
            any = true;
        }
        dbc.poll(|_| {});
        if !any {
            return (lens, payload);
        }
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i ^ (i >> 8) ^ (i >> 16)) as u8).collect()
}

#[rstest]
#[case::one_byte(1, false)]
#[case::one_packet(1024, true)]
#[case::just_over_a_packet(1025, false)]
#[case::one_full_trb(OUT_TRB_MAX, true)]
#[case::full_trb_and_one_byte(OUT_TRB_MAX + 1, false)]
#[case::three_trbs_ending_short(40_000, false)]
#[case::whole_ring(OUT_BYTES, true)]
fn out_burst_ends_on_a_short_packet_exactly_when_needed(#[case] len: usize, #[case] terminated: bool) {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    let data = pattern(len);
    check!(dbc.write(&data).is_complete());

    let (lens, payload) = drain_wire(&sim, &mut dbc);
    check!(payload == data);
    check!(lens.iter().all(|&l| l <= OUT_TRB_MAX));
    check!(lens.last().is_some_and(|&l| l == 0 || l % 1024 != 0), "the host's read ends on a short packet: {lens:?}");
    check!((lens.last() == Some(&0)) == terminated, "{lens:?}");
    check!(dbc.stats().out_terminators == u32::from(terminated));
    check!(dbc.buffered() == 0);
    check!(sim.trb_violations() == 0);
    check!(sim.dma_violations() == 0);
    assert_bytes_balance(&dbc);
}

#[test]
fn terminator_waits_for_everything_posted_and_is_sent_once() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    check!(dbc.write(&[1; 1024]).is_complete());
    check!(dbc.write(&[2; 1024]).is_complete());

    let (_, first) = sim.complete_out().expect("first packet");
    check!(first.len() == 1024);
    dbc.poll(|_| {});
    let (_, second) = sim.complete_out().expect("second packet");
    check!(second.len() == 1024);
    check!(sim.complete_out().is_none(), "nothing follows until the ring has drained");

    dbc.poll(|_| {});
    let (_, ended) = sim.complete_out().expect("the zero-length terminator");
    check!(ended.is_empty());
    dbc.poll(|_| {});
    check!(sim.complete_out().is_none(), "the terminator is not repeated");
    check!(dbc.stats().out_terminators == 1);

    check!(dbc.write(b"tail").is_complete());
    let (lens, _) = drain_wire(&sim, &mut dbc);
    check!(lens == [4], "a short chunk needs no terminator");
}

#[test]
fn out_trbs_split_at_the_ring_wrap_and_stay_inside_the_ring() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    let first = pattern(40 * 1024);
    check!(dbc.write(&first).is_complete());
    let (_, got) = drain_wire(&sim, &mut dbc);
    check!(got == first);

    let second = pattern(40 * 1024 + 5);
    check!(dbc.write(&second).is_complete());
    let (lens, got) = drain_wire(&sim, &mut dbc);
    check!(got == second, "bytes stay in order across the wrap");
    check!(lens == [16 * 1024, 8 * 1024, 16 * 1024, 5], "cut at the wrap, then at the TRB limit: {lens:?}");
    check!(sim.trb_violations() == 0);
    check!(sim.dma_violations() == 0);
    assert_bytes_balance(&dbc);
}

#[test]
fn event_ring_never_overflows_at_full_queue_depth() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    // One TRB per write, up to the in-flight limit; the next byte waits for a slot.
    for i in 0..=OUT_MAX_TRBS {
        check!(dbc.write(&[i as u8]).is_complete());
    }
    for seq in 0..IN_SLOTS {
        assert!(sim.deliver_in(&[seq as u8; 16]));
    }
    sim.connect();
    sim.connect();
    let mut sent = Vec::new();
    while let Some((_, bytes)) = sim.complete_out() {
        sent.extend_from_slice(&bytes);
    }
    check!(sent.len() == OUT_MAX_TRBS, "the byte over the limit has no TRB yet");

    check!(sim.event_overflows() == 0);
    check!(
        sim.event_high_water() >= (OUT_MAX_TRBS + IN_SLOTS) as u64,
        "the controller queued every event before one was read"
    );
    check!(sim.event_high_water() < 256);

    let mut packets = 0;
    dbc.poll(|_| packets += 1);
    check!(packets == IN_SLOTS);
    let (lens, rest) = drain_wire(&sim, &mut dbc);
    check!(lens == [1]);
    check!(rest == [OUT_MAX_TRBS as u8]);
    check!(dbc.stats().unknown_transfer_events == 0);
    assert_bytes_balance(&dbc);
}

#[test]
fn simulator_flags_an_event_ring_overflow() {
    let sim = Sim::with_dbc_controller();
    let _dbc = configured(&sim);
    for _ in 0..300 {
        let _ = sim.push_transfer_event(0, ferrite_testing::CC_SUCCESS, 0);
    }
    check!(sim.event_overflows() > 0);
}

#[test]
fn in_burst_of_every_slot_arrives_in_order_across_ring_laps() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured(&sim);
    let mut expect = 0usize;
    for round in 0..6 {
        for _ in 0..IN_SLOTS {
            let mut packet = vec![0u8; 2 + (expect + round) * 37 % 1000];
            packet[..2].copy_from_slice(&(expect as u16).to_le_bytes());
            assert!(sim.deliver_in(&packet), "slot {expect} was posted");
            expect += 1;
        }
        check!(!sim.deliver_in(b"x"), "no buffer beyond the slot count is posted");

        let mut packets: Vec<Vec<u8>> = Vec::new();
        dbc.poll(|p| packets.push(p.to_vec()));
        check!(packets.len() == IN_SLOTS);
        let first = expect - IN_SLOTS;
        for (i, p) in packets.iter().enumerate() {
            check!(u16::from_le_bytes([p[0], p[1]]) as usize == first + i, "packet {i} of the burst");
        }
    }
    check!(dbc.stats().in_done == 6 * IN_SLOTS as u32);
    check!(dbc.stats().unknown_transfer_events == 0);
    check!(sim.event_overflows() == 0);
    check!(sim.trb_violations() == 0);
}

fn in_config(size: InTrbSize) -> Config {
    Config { in_trb: size, ..Config::default() }
}

/// Poll once and return what arrived as one entry per delivery.
fn received(dbc: &mut SimDbc) -> Vec<Vec<u8>> {
    let mut got = Vec::new();
    dbc.poll(|p| got.push(p.to_vec()));
    got
}

#[test]
fn a_16k_in_trb_completes_on_full() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured_with(&sim, &in_config(InTrbSize::Kib16));
    let data = pattern(16 * 1024);
    assert!(sim.deliver_in(&data));
    check!(received(&mut dbc) == [data], "one delivery of the whole TRB");
    check!(dbc.stats().in_done == 1);
    check!(dbc.stats().bytes_received == 16 * 1024);
}

#[rstest]
fn in_delivers_exactly_the_bytes_received(
    #[values(InTrbSize::Kib1, InTrbSize::Kib4, InTrbSize::Kib16)] size: InTrbSize,
    #[values(1, 1023, 1024, 1500, 4096, 4097, 16383, 16384, 17_000, 20_480)] len: usize,
) {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured_with(&sim, &in_config(size));
    let data = pattern(len);
    // A write ending on a full packet needs the host's zero-length packet to close the last TRB.
    assert!(sim.host_write(&data, len.is_multiple_of(1024)));

    let got: Vec<u8> = received(&mut dbc).concat();
    check!(got == data, "residual math for {len} bytes in {size:?} TRBs");
    check!(dbc.stats().bytes_received == len as u64);
    check!(sim.trb_violations() == 0);
    check!(sim.dma_violations() == 0);
}

#[test]
fn short_packet_mid_trb_delivers_only_its_bytes() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured_with(&sim, &in_config(InTrbSize::Kib16));
    let (first, second) = (pattern(5000), pattern(3));
    assert!(sim.deliver_in(&first));
    assert!(sim.deliver_in(&second));
    check!(received(&mut dbc) == [first, second], "each write ends its own TRB");
    check!(dbc.stats().in_done == 2);
    check!(dbc.stats().unknown_transfer_events == 0);
}

#[test]
fn exact_multiple_write_stays_in_its_trb_until_a_zero_length_packet() {
    let sim = Sim::with_dbc_controller();
    let mut dbc = configured_with(&sim, &in_config(InTrbSize::Kib16));
    let data = pattern(4096);
    assert!(sim.deliver_in(&data));
    check!(received(&mut dbc).is_empty(), "the TRB is not full and no short packet ended it");
    check!(dbc.stats().in_done == 0);

    assert!(sim.deliver_in_zlp(&[]));
    check!(received(&mut dbc) == [data]);
    check!(dbc.stats().in_done == 1);
}

#[test]
fn zero_length_packet_after_a_full_trb_delivers_nothing() {
    for size in [InTrbSize::Kib1, InTrbSize::Kib16] {
        let sim = Sim::with_dbc_controller();
        let mut dbc = configured_with(&sim, &in_config(size));
        let data = pattern(size.bytes());
        assert!(sim.deliver_in_zlp(&data));
        check!(received(&mut dbc) == [data], "{size:?}: the ZLP is not an empty delivery");
        check!(dbc.stats().in_done == 2, "{size:?}: the ZLP used up a TRB");
        check!(dbc.stats().bytes_received == size.bytes() as u64);
    }
}

#[test]
fn large_in_trbs_keep_order_across_ring_laps() {
    let sim = Sim::with_dbc_controller();
    let size = InTrbSize::Kib16;
    let mut dbc = configured_with(&sim, &in_config(size));
    let sizes: [usize; 7] = [100, 16 * 1024, 20_000, 1500, 32 * 1024, 7, 16 * 1024 + 1];
    let (mut sent, mut got) = (Vec::new(), Vec::new());
    let (mut trbs, mut n) = (0usize, 0usize);
    while trbs < 300 {
        let mut used = 0;
        loop {
            let len = sizes[n % sizes.len()];
            let needed = len.div_ceil(size.bytes());
            if used + needed > size.slots() {
                break;
            }
            let data: Vec<u8> = pattern(len).iter().map(|b| b.wrapping_add(n as u8)).collect();
            assert!(sim.deliver_in(&data), "write {n} found its TRBs posted");
            sent.extend_from_slice(&data);
            used += needed;
            n += 1;
        }
        trbs += used;
        dbc.poll(|p| got.extend_from_slice(p));
    }
    check!(got == sent, "bytes arrive in the order the host wrote them");
    check!(dbc.stats().in_done as usize == trbs);
    check!(dbc.stats().unknown_transfer_events == 0);
    check!(sim.event_overflows() == 0);
    check!(sim.trb_violations() == 0);
    check!(sim.dma_violations() == 0);
}
