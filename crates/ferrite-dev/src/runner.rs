//! External commands (`cargo`, `ssh`, `scp`) as plain data, run with separate arguments.

use std::fmt;
use std::io;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new<S: Into<String>>(program: &str, args: impl IntoIterator<Item = S>) -> Self {
        Self { program: program.to_owned(), args: args.into_iter().map(Into::into).collect() }
    }
}

impl fmt::Display for Cmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program)?;
        self.args.iter().try_for_each(|arg| write!(f, " {arg}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Capture stdout and stderr.
    Collect,
    /// Let the command write to this process's terminal.
    Passthrough,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CmdOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

pub fn run(cmd: &Cmd, mode: OutputMode) -> io::Result<CmdOutput> {
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args).stdin(Stdio::null());
    match mode {
        OutputMode::Collect => {
            let output = command.output()?;
            Ok(CmdOutput {
                success: output.status.success(),
                code: output.status.code(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        },
        OutputMode::Passthrough => {
            let status = command.status()?;
            Ok(CmdOutput { success: status.success(), code: status.code(), ..CmdOutput::default() })
        },
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn arguments_reach_the_program_unsplit_and_output_is_collected() {
        let cmd = Cmd::new("printf", ["%s|", "a b", "$HOME; echo x"]);
        assert!(let Ok(out) = run(&cmd, OutputMode::Collect));
        check!(out.success);
        check!(out.stdout == "a b|$HOME; echo x|");
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        assert!(let Ok(out) = run(&Cmd::new("sh", ["-c", "echo oops >&2; exit 7"]), OutputMode::Collect));
        check!((out.success, out.code) == (false, Some(7)));
        check!(out.stderr == "oops\n");
    }
}
