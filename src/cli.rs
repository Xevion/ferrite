use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use clap::ValueEnum;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use snafu::{OptionExt, ResultExt, Whatever};
use tracing::{info, warn};

/// Application-level result defaulting to [`snafu::Whatever`] for loose,
/// message-based errors; the error type stays overridable for callers that
/// need a specific one.
type Result<T, E = Whatever> = std::result::Result<T, E>;

use ferrite::alloc::{CompactionGuard, TestBuffer};
use ferrite::dimm::DimmTopology;
use ferrite::pattern::PatternConfig;
use ferrite::physmem::phys::{MapStats, PagemapResolver, PhysResolver, PhysResolverError};
use ferrite::physmem::sieve::FrameSieve;
use ferrite::units::UnitSystem;

#[cfg(feature = "tui")]
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TuiMode {
    /// Use TUI when stdout is a terminal, plain output otherwise.
    Auto,
    /// Always use the interactive TUI.
    Always,
    /// Never use the TUI; use plain non-interactive output.
    Never,
}

/// Controls how live output and final results render to stdout.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable text output with results table.
    #[default]
    Table,
    /// NDJSON event stream with JSON results.
    Json,
}

/// Controls ANSI color output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    /// Enable color when stdout is a terminal with color support.
    Auto,
    /// Always emit ANSI color codes.
    Always,
    /// Never emit ANSI color codes.
    Never,
}

/// ferrite -- userspace RAM testing tool for Linux
#[derive(Parser)]
#[command(version, about)]
pub struct Cli {
    /// Amount of memory to test (e.g. "256M", "1G", "512K"), or "max" to use
    /// everything available minus --headroom. Defaults to 64M.
    #[arg(short = 's', long, default_value = "64M", value_parser = parse_size_spec)]
    pub size: SizeSpec,

    /// Memory to leave for the rest of the system when allocating. The
    /// allocation walk stops once available memory would drop below this floor.
    #[arg(long, default_value = "1G", value_parser = parse_size)]
    pub headroom: usize,

    /// Number of test passes to run.
    #[arg(short, long, default_value_t = 1)]
    pub passes: usize,

    /// Which test patterns to run. Defaults to all.
    #[arg(short = 't', long = "test", value_enum)]
    pub patterns: Vec<ferrite::pattern::Pattern>,

    /// Worker threads for pattern execution: a count (>= 1) or "auto" (all CPU cores).
    /// 1 runs fully serial.
    #[arg(long, default_value = "auto", value_parser = parse_parallel)]
    pub parallel: Parallelism,

    /// Unit system for sizes and throughput: binary (KiB, MiB, GiB) or decimal (KB, MB, GB).
    #[arg(long, value_enum, default_value_t = UnitSystem::Binary)]
    pub units: UnitSystem,

    /// Output format: "table" (default) for human-readable text, "json" for NDJSON events.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormat>,

    /// Save the NDJSON event stream to a file (always NDJSON regardless of --format).
    #[arg(long, value_name = "FILE")]
    pub events: Option<PathBuf>,

    /// Color output mode: "auto" detects terminal color support,
    /// "always" forces color, "never" disables it.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,

    /// TUI mode: "auto" (default) uses the TUI when stdout is a terminal,
    /// "always" forces the TUI, "never" uses plain non-interactive output.
    #[cfg(feature = "tui")]
    #[arg(long, value_enum, default_value_t = TuiMode::Auto)]
    pub tui: TuiMode,

    /// Disable physical address resolution (skip pagemap/EDAC/SMBIOS).
    #[arg(long)]
    pub no_phys: bool,

    /// Track cumulative physical coverage across runs in this file (created
    /// on first use). Requires physical address resolution (root).
    #[arg(long, value_name = "FILE")]
    pub coverage_file: Option<PathBuf>,

    /// Steer allocation toward untested memory: sweep available RAM before
    /// allocating and hold previously-covered frames hostage so the test
    /// buffer is served fresh frames. Sweeps all available memory regardless
    /// of --size.
    #[arg(long, requires = "coverage_file")]
    pub cull: bool,

    /// Test a specific physical range through /dev/mem instead of anonymous
    /// memory: `START-END` (hex, e.g. 0x39400000-0x395fffff) or `reserved`
    /// (all memmap=-reserved regions). Requires root and `CONFIG_STRICT_DEVMEM=n`.
    /// System RAM is read-only unless --devmem-unsafe is given.
    #[arg(long, value_name = "RANGE", value_parser = ferrite::physmem::devmem::parse_target, conflicts_with = "coverage_file")]
    pub devmem: Option<ferrite::physmem::devmem::DevMemTarget>,

    /// Allow destructive write testing of live System RAM through /dev/mem.
    /// DANGEROUS: writing to memory the kernel is using will corrupt it and
    /// crash the machine. Never enables writes to ACPI/PCI/firmware regions.
    #[arg(long, requires = "devmem")]
    pub devmem_unsafe: bool,

    /// Increase log verbosity: -v raises ferrite to debug, -vv to trace, -vvv
    /// enables trace for all crates. Setting `RUST_LOG` overrides this entirely.
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Cap the number of failures collected per pattern before it stops early.
    /// Bounds memory on badly-failing DIMMs, where an uncapped pattern would
    /// record one failure per word. Use 0 for no limit.
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub max_errors: usize,

    /// Seed for the Random Fill pattern's PRNG, as hex (`0xDEADBEEF`) or a
    /// decimal integer. Omitted: a fresh random seed each run, reported so the
    /// run can be replayed with this flag.
    #[arg(long, value_name = "SEED", value_parser = parse_seed)]
    pub seed: Option<u64>,

    /// Seeded fill-and-verify rounds the Random Fill pattern runs, each with a
    /// distinct derived seed.
    #[arg(long, value_name = "N", default_value_t = 1, value_parser = parse_random_passes)]
    pub random_passes: usize,
}

/// Worker-thread count for pattern execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parallelism {
    /// Use all available CPU cores.
    Auto,
    /// Use exactly this many threads.
    Fixed(std::num::NonZeroUsize),
}

impl Parallelism {
    /// Resolve to a concrete worker-thread count.
    #[must_use]
    pub fn resolve(self) -> usize {
        match self {
            Self::Auto => std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            Self::Fixed(n) => n.get(),
        }
    }
}

/// Parse the `--parallel` flag: either `"auto"` or a positive integer.
///
/// # Errors
///
/// Returns a descriptive error string if the value is `0` or not `"auto"`/an integer.
pub fn parse_parallel(s: &str) -> Result<Parallelism, String> {
    if s.eq_ignore_ascii_case("auto") {
        return Ok(Parallelism::Auto);
    }
    let n: usize = s.parse().map_err(|_| {
        format!("invalid --parallel value: {s} (expected \"auto\" or a positive integer)")
    })?;
    std::num::NonZeroUsize::new(n)
        .map(Parallelism::Fixed)
        .ok_or_else(|| "--parallel must be at least 1".to_owned())
}

/// Parse the `--seed` flag: a `0x`-prefixed hex value or a decimal `u64`.
///
/// # Errors
///
/// Returns a descriptive error string if the value is neither.
pub fn parse_seed(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let parsed = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .map_or_else(|| t.parse::<u64>(), |hex| u64::from_str_radix(hex, 16));
    parsed.map_err(|_| {
        format!("invalid --seed value: {s} (expected hex like 0xDEADBEEF or a decimal u64)")
    })
}

/// Parse the `--random-passes` flag: a positive integer.
///
/// # Errors
///
/// Returns a descriptive error string if the value is `0` or not an integer.
pub fn parse_random_passes(s: &str) -> Result<usize, String> {
    let n: usize = s
        .parse()
        .map_err(|_| format!("invalid --random-passes value: {s} (expected a positive integer)"))?;
    if n == 0 {
        return Err("--random-passes must be at least 1".to_owned());
    }
    Ok(n)
}

/// A fresh seed from OS entropy, used when `--seed` is not given.
///
/// `RandomState` is seeded from the OS RNG on construction; hashing a fixed
/// value yields a per-process-random `u64` without pulling in an RNG crate.
fn os_random_seed() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(0x9E37_79B9_7F4A_7C15);
    h.finish()
}

/// Resolved output configuration after validating CLI flag interactions.
#[derive(Debug)]
pub struct OutputConfig {
    /// Format for stdout (human table or JSON).
    pub format: OutputFormat,
    /// Optional path for the NDJSON event file. `None` = no event file.
    pub events_file: Option<PathBuf>,
    /// Whether ANSI colors should be emitted.
    pub color_enabled: bool,
}

impl Cli {
    /// Upper-bound estimate of the requested allocation in bytes, for
    /// privilege checks that run before allocation. `max` estimates with
    /// `MemTotal` (the reservation size used by [`setup_test`]).
    /// Resolve the pattern runtime config, generating a fresh random seed when
    /// `--seed` was not given. Call once per run so the seed stays stable.
    #[must_use]
    pub fn pattern_config(&self) -> PatternConfig {
        PatternConfig {
            random_seed: self.seed.unwrap_or_else(os_random_seed),
            random_passes: self.random_passes,
        }
    }

    #[must_use]
    pub fn requested_bytes_estimate(&self) -> usize {
        match self.size {
            SizeSpec::Bytes(n) => n,
            SizeSpec::Max => {
                ferrite::physmem::sysmem::mem_total().map_or(usize::MAX, |t| t as usize)
            }
        }
    }

    /// Resolve and validate the output flags, returning a consistent [`OutputConfig`].
    ///
    /// # Errors
    ///
    /// Returns an error if the events file path is not valid UTF-8.
    pub fn resolve_output(&self) -> Result<OutputConfig> {
        let format = self.format.unwrap_or_default();

        // Validate events file path is valid UTF-8 (from_path expects &str)
        if let Some(ref p) = self.events {
            p.to_str().with_whatever_context(|| {
                format!("--events path is not valid UTF-8: {}", p.to_string_lossy())
            })?;
        }

        let color_enabled = match self.color {
            _ if format == OutputFormat::Json => false,
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                supports_color::on(supports_color::Stream::Stdout).is_some_and(|c| c.has_basic)
            }
        };

        Ok(OutputConfig {
            format,
            events_file: self.events.clone(),
            color_enabled,
        })
    }
}

/// The requested test size: an explicit byte count or "max" (everything
/// available minus headroom, resolved at allocation time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeSpec {
    Bytes(usize),
    Max,
}

/// Parse the `--size` flag: a size like "256M"/"1G", or "max".
///
/// # Errors
///
/// Returns a descriptive error string for anything else.
pub fn parse_size_spec(s: &str) -> Result<SizeSpec, String> {
    if s.trim().eq_ignore_ascii_case("max") {
        return Ok(SizeSpec::Max);
    }
    parse_size(s).map(SizeSpec::Bytes)
}

const SIZE_UNITS: [(char, usize); 3] = [('G', 1024 * 1024 * 1024), ('M', 1024 * 1024), ('K', 1024)];

pub fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (num_str, multiplier) = SIZE_UNITS
        .iter()
        .find_map(|&(suffix, multiplier)| {
            s.strip_suffix([suffix, suffix.to_ascii_lowercase()])
                .map(|n| (n, multiplier))
        })
        .unwrap_or((s, 1));
    let num: usize = num_str.parse().map_err(|_| format!("invalid size: {s}"))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow: {s}"))
}

/// A privilege-related warning that the caller should display.
#[derive(Debug, PartialEq, Eq)]
pub enum PrivilegeWarning {
    /// Physical address resolution requires `CAP_SYS_ADMIN` (or root).
    NoSysAdmin,
    /// `RLIMIT_MEMLOCK` is too low for the requested allocation.
    MlockLimitExceeded { soft: u64, requested: u64 },
    /// Could not query `RLIMIT_MEMLOCK`.
    MlockQueryFailed(String),
}

/// Resolved privilege state used to decide whether to emit warnings.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent privilege facet, not a state machine encoded as flags"
)]
pub struct PrivilegeContext {
    pub is_root: bool,
    pub has_ipc_lock: bool,
    pub has_sys_admin: bool,
    pub need_phys: bool,
    /// `Ok(soft_limit)` or `Err(message)` from querying `RLIMIT_MEMLOCK`.
    pub memlock_result: Result<u64, String>,
    pub requested_bytes: usize,
}

impl PrivilegeContext {
    /// Query the current process's privilege state from the OS.
    pub fn from_system(requested_bytes: usize, need_phys: bool) -> Self {
        let is_root = geteuid().is_root();
        let has_ipc_lock = has_capability(14); // CAP_IPC_LOCK
        let has_sys_admin = has_capability(21); // CAP_SYS_ADMIN
        let memlock_result = getrlimit(Resource::RLIMIT_MEMLOCK)
            .map(|(soft, _)| soft)
            .map_err(|e| e.to_string());
        Self {
            is_root,
            has_ipc_lock,
            has_sys_admin,
            need_phys,
            memlock_result,
            requested_bytes,
        }
    }

    /// Compute which privilege warnings apply to the current state.
    pub fn warnings(&self) -> Vec<PrivilegeWarning> {
        let mut out = Vec::new();

        if self.need_phys && !self.is_root && !self.has_sys_admin {
            out.push(PrivilegeWarning::NoSysAdmin);
        }

        if self.is_root || self.has_ipc_lock {
            return out;
        }

        match &self.memlock_result {
            Ok(soft) => {
                if *soft != u64::MAX && (self.requested_bytes as u64) > *soft {
                    out.push(PrivilegeWarning::MlockLimitExceeded {
                        soft: *soft,
                        requested: self.requested_bytes as u64,
                    });
                }
            }
            Err(e) => {
                out.push(PrivilegeWarning::MlockQueryFailed(e.clone()));
            }
        }

        out
    }
}

/// Check whether the process has sufficient privileges to mlock memory.
pub fn check_privileges(requested_bytes: usize, need_phys: bool) {
    let warnings = PrivilegeContext::from_system(requested_bytes, need_phys).warnings();
    for w in &warnings {
        match w {
            PrivilegeWarning::NoSysAdmin => {
                tracing::warn!(
                    "CAP_SYS_ADMIN not detected -- physical addresses will be unavailable. \
                     Run as root (sudo ferrite) or grant the capability \
                     (sudo setcap cap_sys_admin+ep $(which ferrite))"
                );
            }
            PrivilegeWarning::MlockLimitExceeded { soft, requested } => {
                tracing::warn!(
                    soft,
                    requested,
                    "RLIMIT_MEMLOCK is {soft} bytes, but {requested} bytes requested. \
                     mlock will likely fail. Run as root (sudo ferrite), \
                     raise the limit (ulimit -l unlimited), or grant the capability \
                     (sudo setcap cap_ipc_lock+ep $(which ferrite))"
                );
            }
            PrivilegeWarning::MlockQueryFailed(e) => {
                tracing::warn!("could not query RLIMIT_MEMLOCK: {e}");
            }
        }
    }
}

pub fn has_capability(cap_bit: u32) -> bool {
    let Ok(status) = fs::read_to_string("/proc/self/status") else {
        return false;
    };
    parse_capability_from_status(&status, cap_bit)
}

/// Parse the effective capability bitmask from `/proc/self/status` content.
/// Returns true if the given capability bit is set in the `CapEff` field.
pub fn parse_capability_from_status(status: &str, cap_bit: u32) -> bool {
    status
        .lines()
        .find_map(|line| {
            let hex = line.strip_prefix("CapEff:\t")?;
            let bits = u64::from_str_radix(hex.trim(), 16).ok()?;
            Some(bits & (1 << cap_bit) != 0)
        })
        .unwrap_or(false)
}

/// Set up physical address resolution, returning the resolver and map stats if successful.
pub fn setup_phys(
    buffer: &TestBuffer,
    need_phys: bool,
) -> (Option<PagemapResolver>, Option<MapStats>) {
    if !need_phys {
        return (None, None);
    }
    let resolver_result = match PagemapResolver::new() {
        Ok(mut r) => match r.build_map(buffer.as_ptr(), buffer.len()) {
            Ok(stats) => Ok((r, stats)),
            Err(e) => Err(PhysResolverError::from_build(e)),
        },
        Err(e) => Err(PhysResolverError::from_open(e)),
    };

    match resolver_result {
        Ok((r, stats)) => {
            info!(
                pages = %ferrite::units::format_count(stats.total_pages as u64),
                thp = %ferrite::units::format_count(stats.thp_pages as u64),
                huge = %ferrite::units::format_count(stats.huge_pages as u64),
                hwpoison = %ferrite::units::format_count(stats.hwpoison_pages as u64),
                "physical address map built"
            );

            std::thread::sleep(Duration::from_millis(100));
            match r.verify_stability(buffer.as_ptr(), buffer.len()) {
                Ok(0) => {}
                Ok(n) => warn!(
                    changed = %ferrite::units::format_count(n as u64),
                    "pages changed physical address after locking"
                ),
                Err(e) => warn!("PFN stability check failed: {e}"),
            }
            (Some(r), Some(stats))
        }
        Err(PhysResolverError::PermissionDenied { source }) => {
            warn!("{source}");
            warn!(
                "run as root or grant the capability: sudo setcap cap_sys_admin+ep $(which ferrite)"
            );
            (None, None)
        }
        Err(PhysResolverError::Unavailable { source }) => {
            info!("{source}");
            (None, None)
        }
        Err(PhysResolverError::ReadError { source }) => {
            warn!("{source}");
            (None, None)
        }
    }
}

/// Log how the budgeted allocation walk ended, at a severity matching intent:
/// a trimmed explicit request is a warning; a trimmed `max` request is the
/// expected outcome and logs at info.
fn report_alloc_outcome(outcome: &ferrite::alloc::AllocOutcome, spec: SizeSpec) {
    use ferrite::alloc::StopReason;
    use ferrite::units::{Size, UnitSystem};

    let size = |bytes: usize| Size::new(bytes as f64, UnitSystem::Binary);
    match &outcome.stop {
        StopReason::Completed => {}
        StopReason::HeadroomFloor { available } => {
            let msg = format!(
                "locked {} of {} requested (stopped at headroom floor, {} available)",
                size(outcome.achieved),
                size(outcome.requested),
                size(*available as usize),
            );
            if matches!(spec, SizeSpec::Max) {
                info!("{msg}");
            } else {
                warn!("{msg}");
            }
        }
        StopReason::ChunkFailed(e) => {
            warn!(
                "locked {} of {} requested (chunk activation failed: {e})",
                size(outcome.achieved),
                size(outcome.requested),
            );
        }
    }
}

pub struct TestSetup {
    pub buffer: TestBuffer,
    /// Held for its [`Drop`] side-effect -- restores the compaction sysctl on teardown.
    #[cfg_attr(
        not(feature = "tui"),
        expect(
            dead_code,
            reason = "only forwarded to TuiTestSetup, which requires the tui feature"
        )
    )]
    pub compaction_guard: Option<CompactionGuard>,
    pub resolver: Option<PagemapResolver>,
    pub map_stats: Option<MapStats>,
    /// Installed DIMM topology (SMBIOS + EDAC), if resolvable. Emitted as
    /// [`ferrite::events::RunEvent::DimmInfo`] by the run path.
    pub topology: Option<DimmTopology>,
}

/// What [`setup_test`] produced: a locked buffer ready to run, or the
/// discovery that there is nothing left to test.
pub enum SetupOutcome {
    Ready(TestSetup),
    /// The `--cull` sieve held every acquirable frame hostage, leaving the
    /// allocator nothing below the headroom floor: cumulative coverage is at
    /// its ceiling for this boot.
    CullCeiling,
}

/// A `--cull` sieve that holds every previously-covered frame hostage leaves
/// the allocator nothing below the headroom floor. That exhaustion is the
/// coverage ceiling for this boot, not a failure.
const fn is_cull_ceiling(sieve_active: bool, err: &ferrite::alloc::AllocError) -> bool {
    sieve_active && matches!(err, ferrite::alloc::AllocError::Exhausted { .. })
}

/// Run the `--cull` frame sieve against the cumulative covered set. Returns
/// the sieve holding hostage blocks, to be dropped once the test buffer is
/// locked. Best-effort: failures degrade to an ordinary allocation.
fn run_sieve(covered: &[ferrite::physmem::pfn::PfnRange], headroom: u64) -> Option<FrameSieve> {
    use ferrite::units::{Size, UnitSystem};

    if covered.is_empty() {
        info!("--cull: no prior coverage to cull against; skipping sieve");
        return None;
    }
    let size = |bytes: usize| Size::new(bytes as f64, UnitSystem::Binary);
    match FrameSieve::hold(covered, headroom, None) {
        Ok((sieve, outcome)) => {
            info!(
                "sieve: swept {}, holding {} previously-covered memory hostage, \
                 released {} for the test buffer",
                size(outcome.swept),
                size(outcome.held),
                size(outcome.released),
            );
            Some(sieve)
        }
        Err(e) => {
            warn!("--cull sieve failed ({e}); continuing without culling");
            None
        }
    }
}

pub fn setup_test(
    cli: &Cli,
    cull: Option<&[ferrite::physmem::pfn::PfnRange]>,
) -> Result<SetupOutcome> {
    let need_phys = !cli.no_phys;
    let requested = match cli.size {
        SizeSpec::Bytes(n) => n,
        SizeSpec::Max => ferrite::physmem::sysmem::mem_total()
            .whatever_context("cannot resolve --size max: /proc/meminfo is unreadable")?
            as usize,
    };
    let sieve = cull.and_then(|covered| run_sieve(covered, cli.headroom as u64));
    let buffer = match TestBuffer::new_budgeted(requested, cli.headroom as u64) {
        Ok((buffer, outcome)) => {
            report_alloc_outcome(&outcome, cli.size);
            buffer
        }
        Err(e) if is_cull_ceiling(sieve.is_some(), &e) => {
            return Ok(SetupOutcome::CullCeiling);
        }
        Err(e) => {
            if let Some(hint) = e.help() {
                tracing::warn!("hint: {hint}");
            }
            return Err(e).whatever_context("failed to allocate and lock memory");
        }
    };
    // The buffer is locked: release the hostages back to the system.
    drop(sieve);
    let compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };
    let (resolver, map_stats) = setup_phys(&buffer, need_phys);

    // Carry the built topology out to the run path, which emits it as a
    // RunEvent::DimmInfo (reaching human, JSON, and events surfaces) rather than
    // logging it once here and discarding it.
    let topology = if need_phys {
        DimmTopology::build()
    } else {
        None
    };

    Ok(SetupOutcome::Ready(TestSetup {
        buffer,
        compaction_guard,
        resolver,
        map_stats,
        topology,
    }))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use ferrite::units::format_size;

    use super::{parse_capability_from_status, parse_size};

    proptest! {
        #[test]
        fn parse_size_never_panics(s in any::<String>()) {
            let _ = parse_size(&s);
        }

        #[test]
        fn parse_size_roundtrip(bytes: usize) {
            prop_assert_eq!(parse_size(&format_size(bytes)), Ok(bytes));
        }
    }

    mod cull_ceiling {
        use assert2::check;

        use ferrite::alloc::AllocError;

        use crate::cli::is_cull_ceiling;

        #[test]
        fn exhausted_with_sieve_active_is_ceiling() {
            check!(is_cull_ceiling(
                true,
                &AllocError::Exhausted { available: 1024 }
            ));
        }

        #[test]
        fn exhausted_without_sieve_is_an_error() {
            check!(!is_cull_ceiling(
                false,
                &AllocError::Exhausted { available: 1024 }
            ));
        }

        #[test]
        fn non_exhaustion_failures_are_errors_even_with_sieve() {
            check!(!is_cull_ceiling(true, &AllocError::ZeroSize));
        }
    }

    mod size_spec {
        use assert2::check;

        use crate::cli::{SizeSpec, parse_size_spec};

        #[test]
        fn max_keyword_case_insensitive() {
            check!(parse_size_spec("max") == Ok(SizeSpec::Max));
            check!(parse_size_spec("MAX") == Ok(SizeSpec::Max));
            check!(parse_size_spec("Max") == Ok(SizeSpec::Max));
        }

        #[test]
        fn explicit_sizes_parse_as_bytes() {
            check!(parse_size_spec("512M") == Ok(SizeSpec::Bytes(512 * 1024 * 1024)));
            check!(parse_size_spec("1G") == Ok(SizeSpec::Bytes(1024 * 1024 * 1024)));
            check!(parse_size_spec("4096") == Ok(SizeSpec::Bytes(4096)));
        }

        #[test]
        fn junk_is_rejected() {
            check!(parse_size_spec("banana").is_err());
            check!(parse_size_spec("").is_err());
        }
    }

    mod capability_parsing {
        use assert2::{assert, check};

        use super::parse_capability_from_status;

        const STATUS_WITH_CAPS: &str = "\
Name:\tferrite
Umask:\t0022
State:\tR (running)
Tgid:\t12345
Pid:\t12345
CapInh:\t0000000000000000
CapPrm:\t000001ffffffffff
CapEff:\t000001ffffffffff
CapBnd:\t000001ffffffffff
CapAmb:\t0000000000000000";

        const STATUS_NO_CAPS: &str = "\
Name:\tferrite
CapEff:\t0000000000000000";

        #[test]
        fn cap_ipc_lock_present() {
            assert!(parse_capability_from_status(STATUS_WITH_CAPS, 14));
        }

        #[test]
        fn cap_sys_admin_present() {
            // CAP_SYS_ADMIN = bit 21
            assert!(parse_capability_from_status(STATUS_WITH_CAPS, 21));
        }

        #[test]
        fn cap_absent_when_zero() {
            check!(!parse_capability_from_status(STATUS_NO_CAPS, 14));
            check!(!parse_capability_from_status(STATUS_NO_CAPS, 21));
        }

        #[test]
        fn missing_capeff_line() {
            let status = "Name:\tferrite\nPid:\t1234\n";
            check!(!parse_capability_from_status(status, 14));
        }

        #[test]
        fn malformed_hex() {
            let status = "CapEff:\tnot_hex";
            check!(!parse_capability_from_status(status, 14));
        }

        #[test]
        fn empty_status() {
            check!(!parse_capability_from_status("", 0));
        }

        #[test]
        fn specific_bit_only() {
            // Only bit 14 set (CAP_IPC_LOCK)
            let status = "CapEff:\t0000000000004000";
            assert!(parse_capability_from_status(status, 14));
            check!(!parse_capability_from_status(status, 13));
            check!(!parse_capability_from_status(status, 15));
            check!(!parse_capability_from_status(status, 21));
        }
    }

    mod privilege_context {
        use assert2::{assert, check};

        use crate::cli::{PrivilegeContext, PrivilegeWarning};

        #[expect(
            clippy::fn_params_excessive_bools,
            reason = "mirrors PrivilegeContext's independent flags one-for-one"
        )]
        fn ctx(
            is_root: bool,
            has_ipc_lock: bool,
            has_sys_admin: bool,
            need_phys: bool,
            memlock_result: Result<u64, String>,
            requested_bytes: usize,
        ) -> PrivilegeContext {
            PrivilegeContext {
                is_root,
                has_ipc_lock,
                has_sys_admin,
                need_phys,
                memlock_result,
                requested_bytes,
            }
        }

        #[test]
        fn no_warnings_when_root() {
            let c = ctx(true, false, false, false, Ok(1024), 1024 * 1024);
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn no_warnings_with_ipc_lock() {
            let c = ctx(false, true, false, false, Ok(u64::MAX), 64 * 1024 * 1024);
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn warns_when_need_phys_and_no_sys_admin() {
            let c = ctx(false, false, false, true, Ok(u64::MAX), 64 * 1024 * 1024);
            let w = c.warnings();
            assert!(w.len() == 1);
            check!(w[0] == PrivilegeWarning::NoSysAdmin);
        }

        #[test]
        fn no_phys_warning_when_has_sys_admin() {
            let c = ctx(false, false, true, true, Ok(u64::MAX), 64 * 1024 * 1024);
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn rlimit_query_failed() {
            let c = ctx(
                false,
                false,
                false,
                false,
                Err("EPERM".into()),
                64 * 1024 * 1024,
            );
            let w = c.warnings();
            assert!(w.len() == 1);
            check!(w[0] == PrivilegeWarning::MlockQueryFailed("EPERM".into()));
        }

        #[test]
        fn rlimit_unlimited_no_warning() {
            let c = ctx(false, false, false, false, Ok(u64::MAX), usize::MAX);
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn rlimit_too_small() {
            let c = ctx(
                false,
                false,
                false,
                false,
                Ok(1024 * 1024),
                10 * 1024 * 1024,
            );
            let w = c.warnings();
            assert!(w.len() == 1);
            check!(
                w[0] == PrivilegeWarning::MlockLimitExceeded {
                    soft: 1024 * 1024,
                    requested: 10 * 1024 * 1024,
                }
            );
        }

        #[test]
        fn rlimit_exactly_at_limit_no_warning() {
            let c = ctx(false, false, false, false, Ok(1024), 1024);
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn rlimit_within_limit_no_warning() {
            let c = ctx(
                false,
                false,
                false,
                false,
                Ok(64 * 1024 * 1024),
                1024 * 1024,
            );
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn root_skips_rlimit_check() {
            // root + memlock error: rlimit block should be skipped entirely
            let c = ctx(true, false, false, false, Err("fail".into()), 1024);
            assert!(c.warnings().is_empty());
        }

        #[test]
        fn need_phys_and_rlimit_exceeded_both_fire() {
            let c = ctx(false, false, false, true, Ok(1024), 1024 * 1024);
            let w = c.warnings();
            assert!(w.len() == 2);
            check!(w[0] == PrivilegeWarning::NoSysAdmin);
            check!(
                w[1] == PrivilegeWarning::MlockLimitExceeded {
                    soft: 1024,
                    requested: 1024 * 1024,
                }
            );
        }
    }

    mod parallelism {
        use assert2::{assert, check};

        use crate::cli::{Parallelism, parse_parallel};

        #[test]
        fn auto_lowercase() {
            check!(parse_parallel("auto") == Ok(Parallelism::Auto));
        }

        #[test]
        fn auto_case_insensitive() {
            check!(parse_parallel("AUTO") == Ok(Parallelism::Auto));
            check!(parse_parallel("Auto") == Ok(Parallelism::Auto));
        }

        #[test]
        fn valid_counts() {
            let one = parse_parallel("1").unwrap();
            assert!(let Parallelism::Fixed(n) = one);
            check!(n.get() == 1);

            let eight = parse_parallel("8").unwrap();
            assert!(let Parallelism::Fixed(n) = eight);
            check!(n.get() == 8);
        }

        #[test]
        fn rejects_zero() {
            assert!(parse_parallel("0").is_err());
        }

        #[test]
        fn rejects_junk() {
            assert!(parse_parallel("banana").is_err());
            assert!(parse_parallel("").is_err());
            assert!(parse_parallel("-1").is_err());
        }

        #[test]
        fn resolve_fixed_returns_n() {
            let p = parse_parallel("6").unwrap();
            check!(p.resolve() == 6);
        }

        #[test]
        fn resolve_auto_returns_positive() {
            check!(Parallelism::Auto.resolve() >= 1);
        }
    }

    mod seed_parsing {
        use assert2::{assert, check};

        use crate::cli::{parse_random_passes, parse_seed};

        #[test]
        fn hex_with_prefix() {
            check!(parse_seed("0xDEADBEEF") == Ok(0xDEAD_BEEF));
            check!(parse_seed("0Xff") == Ok(255));
        }

        #[test]
        fn decimal() {
            check!(parse_seed("42") == Ok(42));
            check!(parse_seed("  1000  ") == Ok(1000));
        }

        #[test]
        fn max_u64_round_trips() {
            check!(parse_seed("0xFFFFFFFFFFFFFFFF") == Ok(u64::MAX));
        }

        #[test]
        fn rejects_junk() {
            assert!(parse_seed("nope").is_err());
            assert!(parse_seed("0xZZ").is_err());
            assert!(parse_seed("").is_err());
        }

        #[test]
        fn random_passes_accepts_positive() {
            check!(parse_random_passes("1") == Ok(1));
            check!(parse_random_passes("30") == Ok(30));
        }

        #[test]
        fn random_passes_rejects_zero_and_junk() {
            assert!(parse_random_passes("0").is_err());
            assert!(parse_random_passes("-1").is_err());
            assert!(parse_random_passes("x").is_err());
        }
    }

    mod output_resolution {
        use std::path::PathBuf;

        use assert2::check;

        use crate::cli::{ColorMode, OutputFormat, Parallelism};

        /// Build a minimal `Cli` with only the output-relevant fields set.
        fn cli(
            format: Option<OutputFormat>,
            events: Option<&str>,
            color: ColorMode,
        ) -> crate::cli::Cli {
            crate::cli::Cli {
                size: crate::cli::SizeSpec::Bytes(64 * 1024 * 1024),
                headroom: 1024 * 1024 * 1024,
                passes: 1,
                patterns: vec![],
                parallel: Parallelism::Auto,
                units: ferrite::units::UnitSystem::Binary,
                format,
                events: events.map(PathBuf::from),
                color,
                #[cfg(feature = "tui")]
                tui: crate::cli::TuiMode::Never,
                no_phys: true,
                coverage_file: None,
                cull: false,
                devmem: None,
                devmem_unsafe: false,
                verbose: 0,
                max_errors: 1000,
                seed: None,
                random_passes: 1,
            }
        }

        #[test]
        fn defaults_produce_table_format() {
            let out = cli(None, None, ColorMode::Auto).resolve_output().unwrap();
            check!(out.format == OutputFormat::Table);
            check!(out.events_file.is_none());
        }

        #[test]
        fn explicit_table_format() {
            let out = cli(Some(OutputFormat::Table), None, ColorMode::Auto)
                .resolve_output()
                .unwrap();
            check!(out.format == OutputFormat::Table);
        }

        #[test]
        fn format_json_alone() {
            let out = cli(Some(OutputFormat::Json), None, ColorMode::Auto)
                .resolve_output()
                .unwrap();
            check!(out.format == OutputFormat::Json);
            check!(out.events_file.is_none());
        }

        #[test]
        fn format_json_with_events_file() {
            let out = cli(
                Some(OutputFormat::Json),
                Some("/tmp/test.ndjson"),
                ColorMode::Auto,
            )
            .resolve_output()
            .unwrap();
            check!(out.format == OutputFormat::Json);
            check!(out.events_file.as_deref() == Some(std::path::Path::new("/tmp/test.ndjson")));
        }

        #[test]
        fn events_file_with_table_format() {
            let out = cli(None, Some("/tmp/events.ndjson"), ColorMode::Auto)
                .resolve_output()
                .unwrap();
            check!(out.format == OutputFormat::Table);
            check!(out.events_file.is_some());
        }

        #[test]
        fn events_file_with_explicit_table_format() {
            let out = cli(
                Some(OutputFormat::Table),
                Some("/tmp/events.ndjson"),
                ColorMode::Auto,
            )
            .resolve_output()
            .unwrap();
            check!(out.format == OutputFormat::Table);
            check!(out.events_file.is_some());
        }

        #[test]
        fn color_always() {
            let out = cli(None, None, ColorMode::Always).resolve_output().unwrap();
            check!(out.color_enabled);
        }

        #[test]
        fn color_never() {
            let out = cli(None, None, ColorMode::Never).resolve_output().unwrap();
            check!(!out.color_enabled);
        }

        #[test]
        fn json_format_forces_color_off() {
            let out = cli(Some(OutputFormat::Json), None, ColorMode::Always)
                .resolve_output()
                .unwrap();
            check!(!out.color_enabled);
        }

        #[test]
        fn json_format_with_events_forces_color_off() {
            let out = cli(
                Some(OutputFormat::Json),
                Some("/tmp/events.ndjson"),
                ColorMode::Always,
            )
            .resolve_output()
            .unwrap();
            check!(!out.color_enabled);
        }

        #[test]
        fn implicit_format_defaults_to_table() {
            let out = cli(None, Some("/tmp/events.ndjson"), ColorMode::Auto)
                .resolve_output()
                .unwrap();
            check!(out.format == OutputFormat::Table);
        }
    }
}
