use std::fs;

const SYSCTL_PATH: &str = "/proc/sys/vm/compact_unevictable_allowed";

/// RAII guard that disables memory compaction of unevictable (mlocked) pages.
///
/// Writes `0` to `/proc/sys/vm/compact_unevictable_allowed` on creation
/// and restores the original value on drop. This prevents the kernel from
/// migrating mlocked pages during the test, keeping physical addresses stable.
pub struct CompactionGuard {
    original: String,
    changed: bool,
}

impl CompactionGuard {
    /// Disable compaction of unevictable pages. Returns `None` if the sysctl
    /// cannot be read or written (not root, file missing, etc.).
    pub fn new() -> Option<Self> {
        let original = fs::read_to_string(SYSCTL_PATH).ok()?.trim().to_owned();
        if original == "0" {
            return Some(Self {
                original,
                changed: false,
            });
        }
        fs::write(SYSCTL_PATH, "0\n").ok()?;
        Some(Self {
            original,
            changed: true,
        })
    }
}

impl Drop for CompactionGuard {
    fn drop(&mut self) {
        if self.changed {
            let _ = fs::write(SYSCTL_PATH, format!("{}\n", self.original));
        }
    }
}
