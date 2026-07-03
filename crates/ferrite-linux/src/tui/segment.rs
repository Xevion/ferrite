#![cfg_attr(coverage_nightly, coverage(off))]

use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use super::activity::ActivityBuffer;

/// Basis points representing 100% pattern progress (1 bp = 0.01%).
///
/// Progress is stored as an integer count of basis points so worker threads can
/// update it with a single relaxed atomic store. This is the single source of
/// the bp↔percent scale: writers feed [`Segment::set_progress`] and readers pull
/// [`Segment::progress_percent`], so no call site repeats the conversion.
pub const PROGRESS_BP_FULL: u64 = 10_000;

/// Shared state for the test segment displayed by the TUI.
///
/// Worker threads update atomics from their threads; the TUI reads them for rendering.
pub struct Segment {
    /// Human-readable name for this segment (e.g. formatted size or index).
    pub name: String,
    /// Total size of the segment in bytes.
    pub size_bytes: usize,
    patterns: Vec<String>,
    /// Index into the pattern list of the pattern currently running.
    pub current_pattern_idx: AtomicUsize,
    progress_bp: AtomicU64,
    failure_count: AtomicUsize,
    paused: AtomicBool,
    /// Heatmap of recent write/read activity across the segment.
    pub activity: ActivityBuffer,
    last_failure_time: Mutex<Option<Instant>>,
}

impl Segment {
    /// Creates a segment with zeroed progress, failure count, and activity.
    #[must_use]
    pub fn new(name: String, size_bytes: usize, patterns: Vec<String>) -> Self {
        Self {
            name,
            size_bytes,
            patterns,
            current_pattern_idx: AtomicUsize::new(0),
            progress_bp: AtomicU64::new(0),
            failure_count: AtomicUsize::new(0),
            paused: AtomicBool::new(false),
            activity: ActivityBuffer::new(),
            last_failure_time: Mutex::new(None),
        }
    }

    /// Current pattern name, or "done" if all patterns are complete.
    pub fn current_pattern(&self) -> &str {
        let idx = self.current_pattern_idx.load(Ordering::Relaxed);
        self.patterns
            .get(idx)
            .map_or("done", std::string::String::as_str)
    }

    /// Advance to the given pattern index and reset progress.
    pub fn set_pattern(&self, idx: usize) {
        self.current_pattern_idx.store(idx, Ordering::Relaxed);
        self.progress_bp.store(0, Ordering::Relaxed);
    }

    /// Record fractional progress through the current pattern from a
    /// completed/total sub-pass count, scaled to basis points.
    pub fn set_progress(&self, done: u64, total: u64) {
        let bp = if total > 0 {
            (u128::from(done) * u128::from(PROGRESS_BP_FULL) / u128::from(total)) as u64
        } else {
            0
        };
        self.progress_bp.store(bp, Ordering::Relaxed);
    }

    /// Mark the current pattern as fully complete (100%).
    pub fn complete_progress(&self) {
        self.progress_bp.store(PROGRESS_BP_FULL, Ordering::Relaxed);
    }

    /// Current pattern progress as a percentage in `0.0..=100.0`.
    #[must_use]
    pub fn progress_percent(&self) -> f64 {
        self.progress_bp.load(Ordering::Relaxed) as f64 * 100.0 / PROGRESS_BP_FULL as f64
    }

    /// Number of failures recorded so far.
    #[must_use]
    pub fn failure_count(&self) -> usize {
        self.failure_count.load(Ordering::Relaxed)
    }

    /// Whether the segment is currently paused.
    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Set the paused state.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    /// The pause flag as a neutral signal for the worker's pattern loop.
    ///
    /// The `p` key toggles this same atomic via [`Segment::set_paused`], so the
    /// display state and the worker's pause signal are one source of truth.
    #[must_use]
    pub const fn pause_flag(&self) -> &AtomicBool {
        &self.paused
    }

    /// Record that a failure was found (increments count, updates timestamp).
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    pub fn record_failure(&self) {
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        *self.last_failure_time.lock().unwrap() = Some(Instant::now());
    }

    /// Seconds since the last failure, or `f64::MAX` if none.
    ///
    /// # Panics
    ///
    /// Panics if the internal mutex is poisoned.
    #[must_use]
    pub fn last_failure_age_secs(&self) -> f64 {
        self.last_failure_time
            .lock()
            .unwrap()
            .map(|t| t.elapsed().as_secs_f64())
            .unwrap_or(f64::MAX)
    }
}

#[expect(
    clippy::missing_fields_in_debug,
    reason = "atomics and internal buffers are omitted; Debug shows only progress-relevant fields"
)]
impl fmt::Debug for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Segment")
            .field("name", &self.name)
            .field("size_bytes", &self.size_bytes)
            .field("pattern", &self.current_pattern())
            .field("progress_bp", &self.progress_bp.load(Ordering::Relaxed))
            .field("failures", &self.failure_count.load(Ordering::Relaxed))
            .finish()
    }
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "exact values are deterministic for fixed test inputs"
)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn segment_new_defaults() {
        let rs = Segment::new("test".into(), 4096, vec!["solid".into(), "walk".into()]);
        check!(rs.name == "test");
        check!(rs.size_bytes == 4096);
        check!(rs.current_pattern() == "solid");
        check!(rs.progress_percent() == 0.0);
        check!(rs.failure_count() == 0);
        assert!(!rs.is_paused());
    }

    #[test]
    fn current_pattern_returns_correct_pattern() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into(), "b".into(), "c".into()]);
        check!(rs.current_pattern() == "a");
        rs.current_pattern_idx.store(1, Ordering::Relaxed);
        check!(rs.current_pattern() == "b");
        rs.current_pattern_idx.store(2, Ordering::Relaxed);
        check!(rs.current_pattern() == "c");
    }

    #[test]
    fn current_pattern_returns_done_past_end() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        rs.current_pattern_idx.store(5, Ordering::Relaxed);
        check!(rs.current_pattern() == "done");
    }

    #[test]
    fn set_pattern_updates_index_and_resets_progress() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into(), "b".into()]);
        rs.set_progress(1, 2);
        rs.set_pattern(1);
        check!(rs.current_pattern() == "b");
        check!(rs.progress_percent() == 0.0);
    }

    #[test]
    fn set_progress_scales_to_percent() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        rs.set_progress(1, 2);
        check!(rs.progress_percent() == 50.0);
        rs.complete_progress();
        check!(rs.progress_percent() == 100.0);
    }

    #[test]
    fn set_progress_zero_total_is_zero() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        rs.set_progress(5, 0);
        check!(rs.progress_percent() == 0.0);
    }

    #[test]
    fn record_failure_increments_count() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        check!(rs.failure_count() == 0);
        rs.record_failure();
        check!(rs.failure_count() == 1);
        rs.record_failure();
        check!(rs.failure_count() == 2);
    }

    #[test]
    fn last_failure_age_max_when_no_failures() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        check!(rs.last_failure_age_secs() == f64::MAX);
    }

    #[test]
    fn last_failure_age_small_after_failure() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        rs.record_failure();
        let age = rs.last_failure_age_secs();
        assert!(
            age < 1.0,
            "age should be very small immediately after failure, got {age}"
        );
    }

    #[test]
    fn paused_state_round_trips() {
        let rs = Segment::new("r0".into(), 1024, vec!["a".into()]);
        assert!(!rs.is_paused());
        rs.set_paused(true);
        assert!(rs.is_paused());
        rs.set_paused(false);
        assert!(!rs.is_paused());
    }

    #[test]
    fn debug_format_includes_fields() {
        let rs = Segment::new("test-segment".into(), 8192, vec!["solid".into()]);
        rs.record_failure();
        rs.record_failure();
        rs.record_failure();
        rs.set_progress(1, 2);
        let debug = format!("{rs:?}");
        assert!(debug.contains("test-segment"));
        assert!(debug.contains("8192"));
        assert!(debug.contains("solid"));
        assert!(debug.contains("5000"));
        assert!(debug.contains('3'));
    }
}
