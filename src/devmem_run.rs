//! `/dev/mem` targeted-testing backend.
//!
//! A distinct, always-headless execution path: it maps chosen physical ranges
//! (rather than anonymous memory), classifies each for write safety, and either
//! write-tests or read-only probes it. This backend does not honor `--format`
//! or `--events` and skips error analysis; converging it onto the shared run
//! orchestration is tracked separately (XEV-1024).

use snafu::{ResultExt, Whatever};

use ferrite::events::RunEvent;
use ferrite::headless::HeadlessPrinter;
use ferrite::pattern::Pattern;
use ferrite::physmem::phys::PhysResolver;
use ferrite::runner;
use ferrite::shutdown;

use crate::cli::Cli;

type Result<T, E = Whatever> = std::result::Result<T, E>;

/// `/dev/mem` targeted testing: resolve the requested target into concrete
/// physical mappings, then test (or read-only probe) each in turn. Always
/// headless. Exits with a non-zero code if any mapping's write test fails.
pub fn run(
    cli: &Cli,
    target: ferrite::physmem::devmem::DevMemTarget,
    patterns: &[Pattern],
    workers: usize,
    parallel: bool,
) -> Result<()> {
    let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
    let iomem = std::fs::read_to_string("/proc/iomem").unwrap_or_default();
    let system_ram = ferrite::physmem::sysmem::system_ram_ranges(&iomem);

    let mappings = ferrite::physmem::devmem::resolve_mappings(target, &cmdline, &system_ram)
        .whatever_context("failed to resolve /dev/mem mappings")?;

    let mut total_failures: usize = 0;
    for mapping in mappings {
        total_failures += run_mapping(&mapping, cli, patterns, workers, parallel)?;
    }

    let code = shutdown::exit_code(total_failures);
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

/// Test or probe a single physical mapping according to its safety class and
/// the `--devmem-unsafe` override. Returns the number of failures found.
fn run_mapping(
    mapping: &ferrite::physmem::devmem::Mapping,
    cli: &Cli,
    patterns: &[Pattern],
    workers: usize,
    parallel: bool,
) -> Result<usize> {
    use ferrite::physmem::devmem::{Safety, write_allowed};

    let start = mapping.phys_start;
    let end = mapping.phys_start + mapping.len as u64 - 1;
    let label = match mapping.safety {
        Safety::Reserved => "reserved",
        Safety::SystemRam => "System RAM",
        Safety::FirmwareOrMmio => "firmware/MMIO",
    };

    if matches!(mapping.safety, Safety::FirmwareOrMmio) {
        tracing::warn!("devmem: refusing {start:#x}-{end:#x} ({label}) -- never safe to touch");
        return Ok(0);
    }

    if write_allowed(mapping.safety, cli.devmem_unsafe) {
        if matches!(mapping.safety, Safety::SystemRam) {
            tracing::warn!(
                "devmem: --devmem-unsafe write-testing LIVE System RAM {start:#x}-{end:#x} -- \
                 this can corrupt the kernel and crash the machine"
            );
        }
        tracing::info!("devmem: write-testing physical {start:#x}-{end:#x} ({label})");
        run_write(mapping, cli, patterns, workers, parallel)
    } else {
        tracing::info!(
            "devmem: read-only probe of physical {start:#x}-{end:#x} ({label}); \
             pass --devmem-unsafe to write-test (DANGEROUS)"
        );
        run_probe(mapping, cli.units)?;
        Ok(0)
    }
}

/// Context for a `/dev/mem` mapping failure. Live System RAM cannot be mmap'd
/// while it sits in the kernel's direct map (a PAT memtype conflict yields
/// EINVAL), so point the user at the ways to remove it from the direct map.
fn map_context(mapping: &ferrite::physmem::devmem::Mapping) -> String {
    if matches!(mapping.safety, ferrite::physmem::devmem::Safety::SystemRam) {
        "failed to map /dev/mem: the kernel blocks mapping live System RAM that is already \
         in its direct map. Fence the range with memmap= at boot, or offline its memory \
         block, then retest through /dev/mem"
            .to_owned()
    } else {
        "failed to map physical range through /dev/mem".to_owned()
    }
}

/// Run the pattern suite against a writable `/dev/mem` mapping, streaming live
/// output through the headless printer. Physical addresses of failures are
/// resolved exactly (no pagemap) via [`ferrite::physmem::devmem::DevMemResolver`].
fn run_write(
    mapping: &ferrite::physmem::devmem::Mapping,
    cli: &Cli,
    patterns: &[Pattern],
    workers: usize,
    parallel: bool,
) -> Result<usize> {
    let mut buf = ferrite::alloc::TestBuffer::map_physical(mapping.phys_start, mapping.len, true)
        .with_whatever_context(|_| map_context(mapping))?;
    let mut resolver = ferrite::physmem::devmem::DevMemResolver::new(
        buf.as_ptr(),
        mapping.phys_start,
        mapping.len,
    );
    let map_stats = resolver.build_map(buf.as_ptr(), mapping.len).ok();

    let unit_system = cli.units;
    let (tx, rx) = ferrite::events::event_bus();
    let consumer = std::thread::spawn(move || {
        let mut printer = HeadlessPrinter::new(std::io::stdout(), unit_system);
        printer.consume(&rx);
        printer
    });

    let _ = tx.send(RunEvent::RunStart {
        size: mapping.len,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    });
    if let Some(stats) = map_stats {
        let _ = tx.send(RunEvent::MapInfo { stats });
    }

    let run_start = std::time::Instant::now();
    let pass_results = runner::run(
        buf.as_u64_slice_mut(),
        patterns,
        cli.passes,
        parallel,
        &tx,
        Some(&resolver as &(dyn PhysResolver + Sync)),
        &|_| {},
    )
    .whatever_context("pattern execution failed")?;
    let elapsed = run_start.elapsed();

    let _ = tx.send(RunEvent::RunComplete);
    drop(tx);
    let mut printer = consumer.join().expect("event consumer thread panicked");

    let config = ferrite::runner::RunConfig {
        size: mapping.len,
        passes: cli.passes,
        patterns: patterns.to_vec(),
        workers,
    };
    let results = ferrite::runner::RunResults::from_passes(pass_results, config, elapsed);
    printer.print_final_result(results.total_failures);
    Ok(results.total_failures)
}

/// Read-only reachability probe of a physical range via `pread` on `/dev/mem`.
///
/// Unlike `mmap`, `pread` reads live System RAM without hitting the direct-map
/// memtype conflict, so this works where the write path cannot. It never
/// writes, so it is always safe; live RAM mutates under the read, making the
/// checksum a reachability signal rather than a stable value.
fn run_probe(
    mapping: &ferrite::physmem::devmem::Mapping,
    unit_system: ferrite::units::UnitSystem,
) -> Result<()> {
    use std::os::unix::fs::FileExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .open("/dev/mem")
        .whatever_context("failed to open /dev/mem (run as root)")?;

    let end = mapping.phys_start + mapping.len as u64;
    let mut offset = mapping.phys_start;
    let mut chunk = vec![0u8; 4 * 1024 * 1024];
    let mut stats = ferrite::physmem::devmem::ProbeStats::default();
    while offset < end {
        let n = ((end - offset) as usize).min(chunk.len());
        file.read_exact_at(&mut chunk[..n], offset)
            .with_whatever_context(|_| format!("pread /dev/mem at {offset:#x}"))?;
        stats = stats.merge(ferrite::physmem::devmem::probe_bytes(&chunk[..n]));
        offset += n as u64;
    }

    let size = ferrite::units::Size::new(mapping.len as f64, unit_system);
    println!(
        "  probe: {size} readable ({} words, {} nonzero, checksum {:#018x})",
        stats.words_read, stats.nonzero_words, stats.xor_checksum,
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use ferrite::physmem::devmem::{Mapping, Safety};

    use super::map_context;

    fn mapping(safety: Safety) -> Mapping {
        Mapping {
            phys_start: 0x1000,
            len: 0x1000,
            safety,
        }
    }

    #[test]
    fn system_ram_context_mentions_direct_map() {
        let msg = map_context(&mapping(Safety::SystemRam));
        check!(msg.contains("direct map"));
        check!(msg.contains("memmap="));
    }

    #[test]
    fn reserved_context_is_generic() {
        let msg = map_context(&mapping(Safety::Reserved));
        check!(msg == "failed to map physical range through /dev/mem");
    }
}
