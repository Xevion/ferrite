//! Requests to change the run, from the console, the loader's link or the status page. They are
//! queued while the BSP serves its duties and applied by the main loop between sweep steps.

use alloc::vec::Vec;

use ferrite_mem::suite::Preset;

use ferrite_efi::cores::CoreChoice;

use crate::goal::Goal;

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
    /// End the run by this goal from now on; the run's progress is untouched.
    Goal(Goal),
    /// Write the end-of-run report to the boot volume now.
    SaveReport,
    /// End the app.
    Quit,
}

impl Command {
    /// Whether a split test in progress has to be cut short for this command to take effect.
    #[must_use]
    pub const fn interrupts(self) -> bool {
        !matches!(self, Self::Resume | Self::Goal(_) | Self::SaveReport)
    }
}

/// Commands in the order they arrived.
#[derive(Debug, Default)]
pub struct Commands(Vec<Command>);

impl Commands {
    /// Queue `command`, unless it repeats the last one queued: a stop the loader keeps asserting is
    /// one request, while pause, resume, pause still toggles three times.
    pub fn push(&mut self, command: Command) {
        if self.0.last() != Some(&command) {
            self.0.push(command);
        }
    }

    #[must_use]
    pub fn interrupting(&self) -> bool {
        self.0.iter().any(|c| c.interrupts())
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Command> + '_ {
        self.0.drain(..)
    }

    /// Take the queued report requests out of the queue; whether there were any. A report is built
    /// from the run as it stands, so it is written at once rather than after the split test.
    pub fn take_report(&mut self) -> bool {
        let before = self.0.len();
        self.0.retain(|c| *c != Command::SaveReport);
        self.0.len() != before
    }

    /// Take the queued goal changes out of the queue; the last one is the goal now wanted.
    pub fn take_goal(&mut self) -> Option<Goal> {
        let mut goal = None;
        self.0.retain(|c| match *c {
            Command::Goal(g) => {
                goal = Some(g);
                false
            },
            _ => true,
        });
        goal
    }
}

#[cfg(test)]
mod tests {
    use core::num::NonZeroU64;

    use assert2::check;

    use super::*;

    #[test]
    fn taking_the_goal_keeps_the_other_commands_in_order_and_the_last_goal_wins() {
        let mut commands = Commands::default();
        let three = Goal { passes: NonZeroU64::new(3), ..Goal::UNLIMITED };
        commands.push(Command::Goal(Goal::UNLIMITED));
        commands.push(Command::Pause);
        commands.push(Command::Goal(three));
        commands.push(Command::Resume);
        check!(commands.take_goal() == Some(three));
        check!(commands.take_goal() == None);
        check!(commands.drain().collect::<Vec<_>>() == [Command::Pause, Command::Resume]);
    }

    #[test]
    fn a_report_request_is_taken_out_and_the_rest_stay_in_order() {
        let mut commands = Commands::default();
        commands.push(Command::Pause);
        commands.push(Command::SaveReport);
        commands.push(Command::Resume);
        check!(commands.take_report());
        check!(!commands.take_report());
        check!(commands.drain().collect::<Vec<_>>() == [Command::Pause, Command::Resume]);
    }

    #[test]
    fn only_a_resume_a_new_goal_or_a_report_leaves_a_split_test_running() {
        let mut commands = Commands::default();
        commands.push(Command::Goal(Goal::UNLIMITED));
        commands.push(Command::Resume);
        commands.push(Command::SaveReport);
        check!(!commands.interrupting());
        commands.push(Command::Pause);
        check!(commands.interrupting());
    }
}
