use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub const ACTIVITY_CELLS: usize = 128;
pub const ACTIVITY_FADE_SECS: f64 = 4.0;

/// Per-region activity heatmap buffer.
///
/// Each cell tracks the last time it was "touched" (written to during a test).
/// The TUI reads brightness values to render activity indicators.
pub struct ActivityBuffer {
    cells: Vec<AtomicU64>,
    epoch: Instant,
}

impl Default for ActivityBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivityBuffer {
    #[must_use]
    pub fn new() -> Self {
        let cells = (0..ACTIVITY_CELLS).map(|_| AtomicU64::new(0)).collect();
        Self {
            cells,
            epoch: Instant::now(),
        }
    }

    /// Mark a position (0.0..1.0) as active at the current time.
    pub fn touch(&self, position: f64) {
        let idx = (position.clamp(0.0, 0.999) * ACTIVITY_CELLS as f64) as usize;
        let now = self.epoch.elapsed().as_nanos() as u64;
        self.cells[idx].store(now, Ordering::Relaxed);
    }

    /// Get brightness (0.0..1.0) for a given cell index, fading over time.
    #[must_use]
    pub fn brightness(&self, cell_idx: usize) -> f64 {
        let nanos = self.cells[cell_idx].load(Ordering::Relaxed);
        if nanos == 0 {
            return 0.0;
        }
        let now_nanos = self.epoch.elapsed().as_nanos() as u64;
        let age_secs = (now_nanos - nanos) as f64 / 1_000_000_000.0;
        (1.0 - age_secs / ACTIVITY_FADE_SECS).max(0.0)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn new_creates_correct_cell_count() {
        let buf = ActivityBuffer::new();
        assert_eq!(buf.cells.len(), ACTIVITY_CELLS);
    }

    #[test]
    fn default_matches_new() {
        let buf = ActivityBuffer::default();
        assert_eq!(buf.cells.len(), ACTIVITY_CELLS);
    }

    #[test]
    fn untouched_cell_has_zero_brightness() {
        let buf = ActivityBuffer::new();
        assert_eq!(buf.brightness(0), 0.0);
        assert_eq!(buf.brightness(ACTIVITY_CELLS - 1), 0.0);
    }

    #[test]
    fn touched_cell_has_positive_brightness() {
        let buf = ActivityBuffer::new();
        buf.touch(0.5);
        let b = buf.brightness(64); // 0.5 * 128 = 64
        assert!(b > 0.9, "recently touched cell should be near 1.0, got {b}");
    }

    #[test]
    fn touch_clamps_position_low() {
        let buf = ActivityBuffer::new();
        buf.touch(-1.0); // should clamp to 0.0, cell 0
        assert!(buf.brightness(0) > 0.0);
    }

    #[test]
    fn touch_clamps_position_high() {
        let buf = ActivityBuffer::new();
        buf.touch(1.5); // should clamp to 0.999, last cell range
        let last = ACTIVITY_CELLS - 1;
        assert!(buf.brightness(last) > 0.0);
    }

    #[test]
    fn touch_at_zero_maps_to_first_cell() {
        let buf = ActivityBuffer::new();
        buf.touch(0.0);
        assert!(buf.brightness(0) > 0.0);
    }

    #[test]
    fn touch_at_boundary_maps_correctly() {
        let buf = ActivityBuffer::new();
        // 0.5 * 128 = 64
        buf.touch(0.5);
        assert!(buf.brightness(64) > 0.0);
        // Adjacent cells should remain untouched
        assert_eq!(buf.brightness(63), 0.0);
        assert_eq!(buf.brightness(65), 0.0);
    }

    #[test]
    fn brightness_fades_with_time() {
        let buf = ActivityBuffer::new();
        buf.touch(0.0);
        let b1 = buf.brightness(0);

        // Spin briefly so elapsed increases
        std::thread::sleep(std::time::Duration::from_millis(50));
        let b2 = buf.brightness(0);
        assert!(b2 <= b1, "brightness should not increase over time");
    }
}
