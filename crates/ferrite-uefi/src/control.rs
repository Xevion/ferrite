//! Requests to change the run, from the console, the loader's link or the status page. They are
//! queued while the BSP serves its duties and applied by the main loop between sweep steps.

use alloc::vec::Vec;

use ferrite_mem::suite::Preset;

use crate::sweep::CoreChoice;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Stop sweeping after the current step; a split test is abandoned and runs again from its
    /// start on resume.
    Pause,
    Resume,
    /// Throw away every result and start again from pass 0 with this preset.
    Restart(Preset),
    /// Run split tests on these processors from the next test on.
    Cores(CoreChoice),
    /// End the app.
    Quit,
}

impl Command {
    /// Whether a split test in progress has to be cut short for this command to take effect.
    #[must_use]
    pub const fn interrupts(self) -> bool {
        !matches!(self, Self::Resume)
    }
}

/// Commands in the order they arrived.
#[derive(Debug, Default)]
pub struct Commands(Vec<Command>);

impl Commands {
    pub fn push(&mut self, command: Command) {
        self.0.push(command);
    }

    #[must_use]
    pub fn interrupting(&self) -> bool {
        self.0.iter().any(|c| c.interrupts())
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Command> + '_ {
        self.0.drain(..)
    }
}
