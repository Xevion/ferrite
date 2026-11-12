//! The end-of-run report shared by the DbC apps.

use ferrite_efi::xhci::OpenDbc;
use ferrite_hal::Instant;
use ferrite_xhci::State;

use crate::display::Since;

/// Registers, host port state, milestone times since `start`, counters and event types seen.
pub fn report(dbc: &mut OpenDbc, start: Instant) {
    let snap = dbc.snapshot();
    log::info!("registers after:\n{snap}");
    if let Some(port) = dbc.debug_port() {
        let portsc = dbc.host_portsc(port);
        log::info!(
            "  host PORTSC[{port}]={:#010x} link={} speed={}",
            portsc.raw_value(),
            portsc.link(),
            portsc.speed_code()
        );
    }
    let t = dbc.timings();
    log::info!(
        "state {}; since start: dce_ready={} connected={} configured={}",
        dbc.state(),
        Since { start, at: t.dce_ready },
        Since { start, at: t.connected },
        Since { start, at: t.configured },
    );
    let s = dbc.stats();
    log::info!(
        "events {} (out_done {} in_done {} errors {} last_cc {} unknown {}) port_changes {} halts {} rearms {} kicks {}",
        s.events,
        s.out_done,
        s.in_done,
        s.transfer_errors,
        s.last_error_cc,
        s.unknown_transfer_events,
        s.port_changes,
        s.halts,
        s.rearms,
        s.kicks
    );
    log::info!(
        "bytes queued {} sent {} received {} dropped {} refused {}",
        s.bytes_queued,
        s.bytes_sent,
        s.bytes_received,
        s.bytes_dropped,
        s.bytes_refused
    );
    for (ty, name, count) in s.event_type_counts() {
        log::info!("  event type {ty} ({name}): {count}");
    }
    if let Some(fault) = dbc.last_fault() {
        log::error!("DbC is {}: {fault}", State::Faulted);
    }
}
