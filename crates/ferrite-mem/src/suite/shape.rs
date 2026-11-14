//! A test's progress shape: the work it does and the waits between, without the patterns.

use alloc::vec::Vec;
use core::ops::ControlFlow;
use core::time::Duration;

use super::plan::{Op, Phase, plan};
use super::{Config, TestKind};
use crate::region::Layout;

/// One stretch of a test: consecutive phases that touch memory, or a wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Words of work, over every phase until the next wait.
    Work(u64),
    /// Nothing touches the memory for exactly this long.
    Wait(Duration),
}

/// Where a test is, given the words it has done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// `done` of `of` words into the work stage at index `stage`.
    Work {
        stage: usize,
        done: u64,
        of: u64,
    },
    /// In the wait at index `stage`, the `nth` (from 1) of `waits`. Work counts stop at a wait's
    /// start, so how far into it the test is takes a clock.
    Wait {
        stage: usize,
        nth: usize,
        waits: usize,
    },
    Finished,
}

/// The stages of one test over the whole memory, from the plan.
///
/// Most tests are one work stage; bit fade is work, wait, work, wait and so on, because its waits
/// are exactly known time that no word count can stand for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shape {
    stages: Vec<Stage>,
    work: u64,
    waits: Duration,
}

impl Shape {
    /// The shape of `kind` for pass `pass` over `layout`.
    #[must_use]
    pub fn of(kind: TestKind, cfg: &Config, pass: u64, layout: &Layout) -> Self {
        let all = layout.all();
        let mut stages = Vec::new();
        let _ = plan(kind, cfg, pass, layout, &mut |phase: Phase| {
            match phase.op {
                Op::Wait(d) => stages.push(Stage::Wait(d)),
                _ => push_work(&mut stages, phase.work_in(all) as u64),
            }
            ControlFlow::Continue(())
        });
        Self::new(stages)
    }

    /// A shape of exactly `stages`, with neighbouring work stages merged and empty ones dropped.
    #[must_use]
    pub fn new(stages: impl IntoIterator<Item = Stage>) -> Self {
        let mut merged = Vec::new();
        for stage in stages {
            match stage {
                Stage::Work(words) => push_work(&mut merged, words),
                Stage::Wait(d) => merged.push(Stage::Wait(d)),
            }
        }
        let work = merged.iter().map(|s| if let Stage::Work(w) = s { *w } else { 0 }).sum();
        let waits = merged.iter().map(|s| if let Stage::Wait(d) = s { *d } else { Duration::ZERO }).sum();
        Self { stages: merged, work, waits }
    }

    #[must_use]
    pub fn stages(&self) -> &[Stage] {
        &self.stages
    }

    /// Words of work in all.
    #[must_use]
    pub const fn work(&self) -> u64 {
        self.work
    }

    /// The time the waits take, in all.
    #[must_use]
    pub const fn waits(&self) -> Duration {
        self.waits
    }

    /// How many waits the test has.
    #[must_use]
    pub fn wait_count(&self) -> usize {
        self.stages.iter().filter(|s| matches!(s, Stage::Wait(_))).count()
    }

    /// Where a test that has done `done` words of work is. A wait begins when the work before it
    /// is done and ends when the work after it starts.
    #[must_use]
    pub fn locate(&self, done: u64) -> Place {
        let waits = self.wait_count();
        let (mut start, mut nth) = (0, 0);
        for (stage, s) in self.stages.iter().enumerate() {
            match *s {
                Stage::Work(words) => {
                    if done < start + words {
                        return Place::Work { stage, done: done - start, of: words };
                    }
                    start += words;
                },
                Stage::Wait(_) => {
                    nth += 1;
                    if done == start {
                        return Place::Wait { stage, nth, waits };
                    }
                },
            }
        }
        Place::Finished
    }
}

/// Add `words` to the work stage at the end of `stages`, or start one; nothing for no words.
fn push_work(stages: &mut Vec<Stage>, words: u64) {
    if words == 0 {
        return;
    }
    match stages.last_mut() {
        Some(Stage::Work(total)) => *total += words,
        _ => stages.push(Stage::Work(words)),
    }
}
