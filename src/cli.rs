use std::fs;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
#[cfg(feature = "tui")]
use clap::ValueEnum;
use nix::sys::resource::{Resource, getrlimit};
use nix::unistd::geteuid;
use owo_colors::OwoColorize;
use tracing::{info, warn};

use ferrite::alloc::CompactionGuard;
use ferrite::alloc::LockedRegion;
use ferrite::dimm::DimmTopology;
use ferrite::phys::{MapStats, PagemapResolver, PhysResolver};
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

    /// Run patterns sequentially on a single core instead of using all CPU cores.
    #[arg(long)]
    pub sequential: bool,

    /// Unit system for sizes and throughput: binary (KiB, MiB, GiB) or decimal (KB, MB, GB).
    #[arg(long, value_enum, default_value_t = UnitSystem::Binary)]
    pub units: UnitSystem,

    /// Emit NDJSON events for structured output.
    /// Without a path (or with '-'), writes JSON to stdout and human output to stderr.
    /// With a file path, writes JSON to that file and human output to stdout.
    #[arg(long, value_name = "PATH", num_args = 0..=1, default_missing_value = "-")]
    pub json: Option<String>,

    /// TUI mode: "auto" (default) uses the TUI when stdout is a terminal,
    /// "always" forces the TUI, "never" uses plain non-interactive output.
    #[cfg(feature = "tui")]
    #[arg(long, value_enum, default_value_t = TuiMode::Auto)]
    pub tui: TuiMode,

    /// Disable physical address resolution (skip pagemap/EDAC/SMBIOS).
    #[arg(long)]
    pub no_phys: bool,

    /// Number of memory regions to test in parallel (default: CPU core count).
    /// Each region runs all patterns independently.
    #[arg(long, default_value_t = 0)]
    pub regions: usize,
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
                eprintln!(
                    "{} CAP_SYS_ADMIN not detected -- physical addresses will be unavailable",
                    "warning:".yellow().bold(),
                );
                eprintln!(
                    "         run as root for physical address resolution: {}",
                    "sudo ferrite".bold()
                );
            }
            PrivilegeWarning::MlockLimitExceeded { soft, requested } => {
                eprintln!(
                    "{} RLIMIT_MEMLOCK is {soft} bytes, but {requested} bytes requested",
                    "warning:".yellow().bold(),
                );
                eprintln!("         mlock will likely fail. Options:");
                eprintln!("           - run as root: {}", "sudo ferrite".bold());
                eprintln!(
                    "           - raise the limit: {}",
                    "ulimit -l unlimited".bold()
                );
                eprintln!(
                    "           - grant the capability: {}",
                    "sudo setcap cap_ipc_lock+ep $(which ferrite)".bold()
                );
            }
            PrivilegeWarning::MlockQueryFailed(e) => {
                eprintln!(
                    "{} could not query RLIMIT_MEMLOCK: {e}",
                    "warning:".yellow().bold(),
                );
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
    region: &LockedRegion,
    need_phys: bool,
) -> (Option<PagemapResolver>, Option<MapStats>) {
    if !need_phys {
        return (None, None);
    }
    match PagemapResolver::new() {
        Ok(mut r) => match r.build_map(region.as_ptr(), region.len()) {
            Ok(stats) => {
                info!(
                    pages = stats.total_pages,
                    thp = stats.thp_pages,
                    huge = stats.huge_pages,
                    hwpoison = stats.hwpoison_pages,
                    "physical address map built"
                );

                std::thread::sleep(Duration::from_millis(100));
                match r.verify_stability(region.as_ptr(), region.len()) {
                    Ok(0) => {}
                    Ok(n) => warn!(changed = n, "pages changed physical address after locking"),
                    Err(e) => warn!("PFN stability check failed: {e}"),
                }
                (Some(r), Some(stats))
            }
            Err(e) => {
                warn!("failed to build page map: {e}");
                (None, None)
            }
        },
        Err(e) => {
            warn!("pagemap unavailable: {e}");
            (None, None)
        }
    }
}

pub struct TestSetup {
    pub region: LockedRegion,
    /// Held for its [`Drop`] side-effect — restores the compaction sysctl on teardown.
    #[allow(dead_code)]
    pub compaction_guard: Option<CompactionGuard>,
    pub resolver: Option<PagemapResolver>,
    pub map_stats: Option<MapStats>,
}

pub fn setup_test(cli: &Cli) -> Result<TestSetup> {
    let need_phys = !cli.no_phys;
    let region = LockedRegion::new(cli.size).context("failed to allocate and lock memory")?;
    let compaction_guard = if need_phys {
        CompactionGuard::new()
    } else {
        None
    };
    let (resolver, map_stats) = setup_phys(&region, need_phys);

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
        region,
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
}
