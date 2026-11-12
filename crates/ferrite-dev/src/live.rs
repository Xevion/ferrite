//! The real environment for `run`: local processes, the wall clock and a capture thread.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::bench::{self, BenchOptions, Phase};
use crate::chain;
use crate::config::Config;
use crate::emit::Emitter;
use crate::log::{self, LogOptions};
use crate::run::{Capture, CaptureKind, CapturePlan, Env};
use crate::runner::{self, Cmd, CmdOutput, OutputMode};

pub struct Live<'a> {
    config: &'a Config,
    started: Instant,
}

impl<'a> Live<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config, started: Instant::now() }
    }
}

impl Env for Live<'_> {
    fn run(&self, cmd: &Cmd, mode: OutputMode) -> io::Result<CmdOutput> {
        runner::run(cmd, mode)
    }

    fn now(&self) -> Duration {
        self.started.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        thread::sleep(duration);
    }

    fn say(&self, message: &str) {
        println!("{message}");
    }

    fn start_capture(&self, plan: &CapturePlan) -> io::Result<Box<dyn Capture>> {
        let mut emitter = Emitter::new().with_file(&plan.file)?;
        let config = self.config.clone();
        let kind = plan.kind.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::Builder::new().name("capture".into()).spawn(move || match kind {
            CaptureKind::Log { probe, duration } => {
                let options = LogOptions { duration: Some(duration), probe };
                log::run(&config, &options, &mut emitter, &flag).err().map(|e| chain(&e))
            },
            CaptureKind::Bench { wait, link } => {
                let options = BenchOptions { phases: Phase::ALL.to_vec(), wait, link };
                bench::run(&config, &options, &mut emitter, &flag).err().map(|e| chain(&e))
            },
        })?;
        Ok(Box::new(Running { stop, handle }))
    }
}

struct Running {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Option<String>>,
}

impl Capture for Running {
    fn stop(self: Box<Self>) -> Option<String> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or_else(|_| Some("the capture thread panicked".to_owned()))
    }
}
