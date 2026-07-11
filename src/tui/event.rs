#![cfg_attr(coverage_nightly, coverage(off))]

use std::fmt;

use crossterm::event;

/// Outcome of the TUI event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TuiOutcome {
    /// User pressed 'q', Esc, or Ctrl+C.
    Quit,
    /// The segment finished testing.
    AllComplete,
    /// Event channel disconnected (all senders dropped).
    Disconnected,
}

/// Result returned by [`run_event_loop`](super::run_event_loop), capturing loop
/// state for the caller.
pub struct TuiLoopResult {
    /// Why the event loop exited.
    pub outcome: TuiOutcome,
    /// Failures collected while the loop ran.
    pub failures: Vec<TuiFailure>,
    /// Verbose-mode state at exit, so the caller can preserve it in follow-up output.
    pub verbose: bool,
}

/// Which bits flipped in a memory test failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlippedBits {
    /// Exactly one bit flipped.
    Single(u8),
    /// Multiple bits flipped — stores bit count and the full XOR mask.
    Multi {
        /// Number of bits that differed.
        count: u8,
        /// Full XOR mask of expected vs actual.
        xor: u64,
    },
}

impl fmt::Display for FlippedBits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Single(pos) => write!(f, "bit {pos}"),
            Self::Multi { count, .. } => write!(f, "{count} bits"),
        }
    }
}

impl FlippedBits {
    /// Classify a flip from a precomputed XOR mask and its popcount.
    ///
    /// Callers holding a `Failure` pass its `xor()` and `flipped_bits()` so the
    /// popcount/xor logic is not recomputed here, keeping this type decoupled
    /// from the `Failure` struct.
    #[must_use]
    pub const fn from_xor(xor: u64, count: u32) -> Self {
        if count == 1 {
            Self::Single(xor.trailing_zeros() as u8)
        } else {
            Self::Multi {
                count: count as u8,
                xor,
            }
        }
    }
}

/// A memory test failure record for TUI display, decoupled from the
/// main crate's `Failure` type.
#[derive(Debug)]
pub struct TuiFailure {
    /// Name of the segment where the failure occurred.
    pub segment_name: String,
    /// Address of the failing word.
    pub address: u64,
    /// Expected value at that address.
    pub expected: u64,
    /// Actual value read back.
    pub actual: u64,
    /// Which bits differed between expected and actual.
    pub flipped_bits: FlippedBits,
    /// Name of the pattern active when the failure was found.
    pub pattern: String,
    /// Fraction (0.0..=1.0) of the pattern's progress when the failure occurred.
    pub progress_fraction: f64,
}

/// Events flowing into the TUI event loop.
#[derive(Debug)]
pub enum TuiEvent {
    /// A raw key press from the terminal.
    Key(event::KeyEvent),
    /// Periodic tick to drive rendering and drain buffered log lines.
    Tick,
    /// A pre-formatted ANSI log line from `tracing_subscriber::fmt`.
    Log(String),
    /// A memory test failure to display.
    Failure(TuiFailure),
    /// The segment finished all configured passes.
    Done,
}
