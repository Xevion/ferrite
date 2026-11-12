//! External commands (`cargo`, `ssh`, `scp`) as plain data, run with separate arguments.

use std::fmt;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const REAP_EVERY: Duration = Duration::from_millis(20);
const PIPE_GRACE: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Kill the command after this long. Only honoured when its output is collected.
    pub limit: Option<Duration>,
}

impl Cmd {
    pub fn new<S: Into<String>>(program: &str, args: impl IntoIterator<Item = S>) -> Self {
        Self { program: program.to_owned(), args: args.into_iter().map(Into::into).collect(), limit: None }
    }

    #[must_use]
    pub const fn with_limit(mut self, limit: Duration) -> Self {
        self.limit = Some(limit);
        self
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
    /// The command ran past its limit and was killed.
    pub timed_out: bool,
}

pub fn run(cmd: &Cmd, mode: OutputMode) -> io::Result<CmdOutput> {
    let mut command = Command::new(&cmd.program);
    command.args(&cmd.args).stdin(Stdio::null());
    match mode {
        OutputMode::Collect => match cmd.limit {
            None => {
                let output = command.output()?;
                Ok(CmdOutput {
                    success: output.status.success(),
                    code: output.status.code(),
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    timed_out: false,
                })
            },
            Some(limit) => collect_within(&mut command, limit),
        },
        OutputMode::Passthrough => {
            let status = command.status()?;
            Ok(CmdOutput { success: status.success(), code: status.code(), ..CmdOutput::default() })
        },
    }
}

/// Forward everything `pipe` yields over a channel, so a reader stuck behind a grandchild that
/// still holds the pipe never blocks the caller.
fn forward(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let Some(mut pipe) = pipe else { return };
        let mut chunk = [0; 4096];
        while let Ok(n @ 1..) = pipe.read(&mut chunk) {
            if tx.send(chunk[..n].to_vec()).is_err() {
                return;
            }
        }
    });
    rx
}

/// Everything forwarded so far, waiting a short while for the stream to end.
fn gather(rx: &mpsc::Receiver<Vec<u8>>) -> String {
    let mut bytes = Vec::new();
    while let Ok(chunk) = rx.recv_timeout(PIPE_GRACE) {
        bytes.extend(chunk);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Run `command` and collect its output, killing it once `limit` has passed.
fn collect_within(command: &mut Command, limit: Duration) -> io::Result<CmdOutput> {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let (stdout, stderr) = (forward(child.stdout.take()), forward(child.stderr.take()));
    let started = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if started.elapsed() >= limit {
            child.kill()?;
            break (child.wait()?, true);
        }
        thread::sleep(REAP_EVERY);
    };
    Ok(CmdOutput {
        success: status.success() && !timed_out,
        code: status.code(),
        stdout: gather(&stdout),
        stderr: gather(&stderr),
        timed_out,
    })
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
    fn a_command_past_its_limit_is_killed_and_reported() {
        let started = Instant::now();
        let cmd = Cmd::new("sh", ["-c", "echo partial; sleep 30"]).with_limit(Duration::from_millis(200));
        assert!(let Ok(out) = run(&cmd, OutputMode::Collect));
        check!(started.elapsed() < Duration::from_secs(10));
        check!((out.success, out.timed_out, out.stdout.as_str()) == (false, true, "partial\n"));
    }

    #[test]
    fn a_command_within_its_limit_is_collected_normally() {
        let cmd = Cmd::new("sh", ["-c", "echo ok; exit 3"]).with_limit(Duration::from_secs(30));
        assert!(let Ok(out) = run(&cmd, OutputMode::Collect));
        check!((out.success, out.code, out.timed_out, out.stdout.as_str()) == (false, Some(3), false, "ok\n"));
    }

    #[test]
    fn a_failing_command_reports_its_exit_code() {
        assert!(let Ok(out) = run(&Cmd::new("sh", ["-c", "echo oops >&2; exit 7"]), OutputMode::Collect));
        check!((out.success, out.code) == (false, Some(7)));
        check!(out.stderr == "oops\n");
    }
}
