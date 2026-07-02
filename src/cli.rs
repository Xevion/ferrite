use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use clap::ValueEnum;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use tracing::{info, warn};

use ferrite::alloc::{CompactionGuard, TestBuffer};
use ferrite::dimm::DimmTopology;
use ferrite::phys::{MapStats, PagemapResolver, PhysResolver, PhysResolverError};
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
    /// Amount of memory to test (e.g. "256M", "1G", "512K").
    /// Defaults to 64M.
    #[arg(short = 's', long, default_value = "64M", value_parser = parse_size)]
    pub size: usize,

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
    /// Resolve and validate the output flags, returning a consistent [`OutputConfig`].
    ///
    /// # Errors
    ///
    /// Returns an error if the events file path is not valid UTF-8.
    pub fn resolve_output(&self) -> Result<OutputConfig> {
        let format = self.format.unwrap_or_default();

        // Validate events file path is valid UTF-8 (from_path expects &str)
        if let Some(ref p) = self.events {
            p.to_str().with_context(|| {
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

pub fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix(['G', 'g']) {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['M', 'm']) {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix(['K', 'k']) {
        (n, 1024)
    } else {
        (s, 1)
    };
    let num: usize = num_str.parse().map_err(|_| format!("invalid size: {s}"))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| format!("size overflow: {s}"))
}

/// A privilege-related warning that the caller should display.
#[derive(Debug, PartialEq)]
pub(crate) enum PrivilegeWarning {
    /// Physical address resolution requires `CAP_SYS_ADMIN` (or root).
    NoSysAdmin,
    /// `RLIMIT_MEMLOCK` is too low for the requested allocation.
    MlockLimitExceeded { soft: u64, requested: u64 },
    /// Could not query `RLIMIT_MEMLOCK`.
    MlockQueryFailed(String),
}

/// Resolved privilege state used to decide whether to emit warnings.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct PrivilegeContext {
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
pub(crate) fn parse_capability_from_status(status: &str, cap_bit: u32) -> bool {
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
                pages = stats.total_pages,
                thp = stats.thp_pages,
                huge = stats.huge_pages,
                hwpoison = stats.hwpoison_pages,
                "physical address map built"
            );

            std::thread::sleep(Duration::from_millis(100));
            match r.verify_stability(buffer.as_ptr(), buffer.len()) {
                Ok(0) => {}
                Ok(n) => warn!(changed = n, "pages changed physical address after locking"),
                Err(e) => warn!("PFN stability check failed: {e}"),
            }
            (Some(r), Some(stats))
        }
        Err(PhysResolverError::PermissionDenied(e)) => {
            warn!("{e}");
            warn!(
                "run as root or grant the capability: sudo setcap cap_sys_admin+ep $(which ferrite)"
            );
            (None, None)
        }
        Err(PhysResolverError::Unavailable(e)) => {
            info!("{e}");
            (None, None)
        }
        Err(PhysResolverError::ReadError(e)) => {
            warn!("{e}");
            (None, None)
        }
    }
}

pub struct TestSetup {
    pub buffer: TestBuffer,
    /// Held for its [`Drop`] side-effect -- restores the compaction sysctl on teardown.
    #[allow(dead_code)]
    pub compaction_guard: Option<CompactionGuard>,
    pub resolver: Option<PagemapResolver>,
    pub map_stats: Option<MapStats>,
}

pub fn setup_test(cli: &Cli) -> Result<TestSetup> {
    let need_phys = !cli.no_phys;
    let buffer = match TestBuffer::new(cli.size) {
        Ok(r) => r,
        Err(e) => {
            if let Some(hint) = e.help() {
                tracing::warn!("hint: {hint}");
            }
            return Err(e).context("failed to allocate and lock memory");
        }
    };
    let compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };
    let (resolver, map_stats) = setup_phys(&buffer, need_phys);

    if need_phys && let Some(topo) = DimmTopology::build() {
        let dimm_str = topo
            .dimms
            .iter()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ");
        info!("installed DIMMs: {dimm_str}");
    }

    Ok(TestSetup {
        buffer,
        compaction_guard,
        resolver,
        map_stats,
    })
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

        #[allow(clippy::fn_params_excessive_bools)]
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
                size: 64 * 1024 * 1024,
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
