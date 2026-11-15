//! The main loop: poll the network every few milliseconds, render at about
//! 20 Hz, and give the time in between to the memory sweep.
//!
//! While a test runs split over every core, the sweep runs the loop's duties
//! from inside it.

use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString as _};
use core::fmt::Write as _;
use core::time::Duration;

use ferrite_efi::clock::Clock;
use ferrite_efi::config;
use ferrite_efi::cores::{CoreChoice, Cores};
use ferrite_efi::cpu::VectorState;
use ferrite_efi::link::Link as LoaderLink;
use ferrite_efi::mp::Mp;
use ferrite_efi::power::Power;
use ferrite_efi::sensors::Sensors;
use ferrite_efi::{rtc, watchdog::Keepalive};
use ferrite_hal::{Clock as _, Instant, UnixTime, WallClock};
use ferrite_mem::Caps;
use ferrite_net::NetStatus;
use ferrite_power::control::Request;
use ferrite_power::rapl::Domain;
use ratatui::Terminal;
use uefi::Event;
use uefi::boot::{self, EventType, TimerTrigger, Tpl};
use uefi::proto::console::text::{Input, Key, Output, ScanCode};

use crate::backend::{BackendError, UefiBackend};
use crate::control::{Command, Commands};
use crate::dash::format;
use crate::dash::{Dashboard, Frame, Notice};
use crate::diag::Diag;
use crate::goal::{Goal, Met};
use crate::gop::Display;
use crate::hardware::{Facts, Hardware, Vitals};
use crate::net::Net;
use crate::net::notify::{self, Watch};
use crate::report::Snapshot;
use crate::runreport::store::{ReportsDir, Saver};
use crate::runreport::{Report, VERSION, Why};
use crate::sweep::{Flow, Sweep, SweepStatus, Tended};
use crate::ui::{self, View};
use crate::wall::Wall;

/// The processors split tests run on. Every logical processor: verify-heavy kernels scale up to
/// all eight threads on the laptop, while fills saturate at two cores either way.
const CORES: CoreChoice = CoreChoice::All;
const FRAME_INTERVAL: Duration = Duration::from_millis(50);
/// Sweep chunks are sized to take about this long, so the network is polled in time.
const SWEEP_SLICE: Duration = Duration::from_millis(1);
const MIN_CHUNK_BYTES: usize = 64 << 10;
const MAX_CHUNK_BYTES: usize = 4 << 20;
const START_CHUNK_BYTES: usize = 256 << 10;
/// A suite pause shorter than this (a bus-stress gap) is spun through rather than slept, since the
/// firmware timer that wakes a wait ticks far more coarsely.
const SHORT_WAIT: Duration = Duration::from_millis(2);
/// Under the loader the firmware watchdog is armed for `WATCHDOG` and re-armed every
/// `WATCHDOG_REARM`, so a long sweep never trips it and a hung loop does.
const WATCHDOG: Duration = Duration::from_secs(60);
const WATCHDOG_REARM: Duration = Duration::from_secs(5);
/// How long the end of the run waits for its notifications to go out.
const NOTIFY_FLUSH: Duration = Duration::from_secs(4);
/// Longest gap between network polls while waiting for them.
const NOTIFY_STEP: Duration = Duration::from_millis(5);
/// How often draw times are summarised at debug.
const DRAW_LOG_EVERY: Duration = Duration::from_secs(30);
/// How often the CPU's clock and temperature are read. The clock is the average over this interval.
const SENSOR_INTERVAL: Duration = Duration::from_secs(1);
/// How often the power meter's reading goes to the log.
const POWER_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Where frames go: the dashboard on the GOP framebuffer, or a short status screen on the text
/// console of a machine without GOP.
#[expect(clippy::large_enum_variant, reason = "one screen exists for the whole run")]
enum Screen<'a> {
    Pixels { dash: Box<Dashboard>, display: Display },
    Text { terminal: Box<Terminal<UefiBackend<'a>>>, last: Option<View> },
}

impl<'a> Screen<'a> {
    fn open(output: &'a mut Output, clock: &Clock) -> Result<Self, BackendError> {
        match Display::open() {
            Ok(mut display) => {
                let (width, height) = display.size();
                match Dashboard::new(width, height) {
                    Ok(dash) => {
                        display.calibrate(dash.output().0, clock);
                        return Ok(Self::Pixels { dash: Box::new(dash), display });
                    },
                    Err(e) => log::warn!("ui: no dashboard on the {width}x{height} GOP, using the text console: {e}"),
                }
            },
            Err(e) => log::info!("ui: {e}; using the text console"),
        }
        Ok(Self::Text { terminal: Box::new(Terminal::new(UefiBackend::new(output))?), last: None })
    }
}

/// Dashboard frames that changed something since the last summary.
#[derive(Default)]
struct DrawStats {
    since: Instant,
    frames: u32,
    render: Duration,
    push: Duration,
    worst: Duration,
    bytes: u64,
}

impl DrawStats {
    fn record(&mut self, now: Instant, render: Duration, push: Duration, bytes: u64) {
        self.frames += 1;
        self.render += render;
        self.push += push;
        self.worst = self.worst.max(render + push);
        self.bytes += bytes;
        if now - self.since < DRAW_LOG_EVERY {
            return;
        }
        let frames = self.frames.max(1);
        log::debug!(
            "ui: {} frames in {:?}, render mean {:?}, push mean {:?}, worst {:?}, {} KiB pushed",
            self.frames,
            now - self.since,
            self.render / frames,
            self.push / frames,
            self.worst,
            self.bytes >> 10
        );
        *self = Self { since: now, ..Self::default() };
    }
}

/// Sleeps until a timeout or a keypress, for when there is no sweep to run.
struct Waiter {
    /// The timer, then the console's key event.
    events: [Event; 2],
}

impl Waiter {
    fn new(input: &Input) -> uefi::Result<Self> {
        // SAFETY: a plain timer event with no notify function.
        let timer = unsafe { boot::create_event(EventType::TIMER, Tpl::APPLICATION, None, None) }?;
        let key = input.wait_for_key_event()?;
        Ok(Self { events: [timer, key] })
    }

    fn wait(&mut self, timeout: Duration) -> uefi::Result<()> {
        boot::set_timer(&self.events[0], TimerTrigger::Relative(timeout))?;
        boot::wait_for_event(&mut self.events).map_err(|e| e.to_err_without_payload())?;
        Ok(())
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        // SAFETY: the clone is the only handle used from here on; the original is dropped unused.
        let timer = unsafe { self.events[0].unsafe_clone() };
        if let Err(e) = boot::close_event(timer) {
            log::debug!("ui: closing the timer event failed: {e}");
        }
    }
}

/// The wall clock as the firmware's RTC reads it now, or none when it cannot be read.
fn rtc_clock(clock: &Clock) -> Option<WallClock> {
    match rtc::now() {
        Ok(utc) => Some(WallClock::from_rtc(clock.now(), UnixTime::from_epoch(utc))),
        Err(e) => {
            log::warn!("time: the firmware RTC cannot be read ({e}); there is no time of day until an NTP sync");
            None
        },
    }
}

pub struct App<'a> {
    desk: Desk<'a>,
    sweep: Sweep,
    chunk_bytes: usize,
    /// AVX state, enabled by the caller for the app's lifetime; wide sweep steps run inside its guard.
    vector: Option<&'a VectorState>,
}

/// Everything the BSP serves besides the sweep: watchdog, network, keys, screen and log.
struct Desk<'a> {
    input: &'a mut Input,
    screen: Screen<'a>,
    clock: Clock,
    /// The time of day: the RTC, corrected by the network's time client.
    wall: Rc<Wall>,
    net: Net,
    /// Notices passes and the first fault, for push notifications.
    watch: Watch,
    diag: Diag,
    waiter: Option<Waiter>,
    draw_failures: u32,
    draw_stats: DrawStats,
    /// Pause state the `p` key asked for, until the queued commands are applied.
    pause_wanted: Option<bool>,
    next_frame: Instant,
    /// The loader's link, when this app was started by the loader.
    loader: Option<LoaderLink>,
    watchdog: Option<Keepalive>,
    /// Requests from the console, the loader's link and the status page, not yet applied.
    commands: Commands,
    /// The goal part that was met, when the run is being ended for it.
    goal_met: Option<Met>,
    /// Writes the run's report to the boot volume.
    reports: Saver,
    /// What the machine is, and its clock and temperature as `sensors` last read them.
    hardware: Rc<Hardware>,
    sensors: Sensors,
    next_sensor_read: Instant,
    /// The clock request this app made, and the energy and limit meter.
    power: Power,
    next_power_log: Instant,
}

impl<'a> App<'a> {
    /// `caps` must allow the wide tiers only when `vector` is present.
    pub fn new(
        input: &'a mut Input,
        output: &'a mut Output,
        vector: Option<&'a VectorState>,
        caps: Caps,
    ) -> Result<Self, BackendError> {
        let clock = Clock::start();
        let screen = Screen::open(output, &clock)?;
        let settings = config::load();
        let goal = settings.run.map_or(Goal::UNLIMITED, Goal::from);
        log::info!("run: goal {goal}");
        let time = settings.time.unwrap_or_default();
        let wall = Rc::new(Wall::new(rtc_clock(&clock), time.zone.clone(), time.clock));
        let sensors = Sensors::probe(&clock);
        let hardware = Rc::new(Hardware::new(Facts::detect(sensors.identity().clone())));
        let net =
            Net::setup(clock, settings.credentials, settings.notify, &time, Rc::clone(&wall), Rc::clone(&hardware));
        let waiter = Waiter::new(input).inspect_err(|e| log::warn!("ui: no wait events, stalling instead: {e}")).ok();
        let loader = LoaderLink::locate();
        let watchdog = loader.map(|_| Keepalive::new(WATCHDOG, WATCHDOG_REARM, clock.now()));
        let cores = Cores::locate(CORES, clock);
        let mut power = Power::discover();
        power.apply(Request::MAX, cores.as_ref().map(Cores::mp));
        let desk = Desk {
            input,
            screen,
            clock,
            wall,
            net,
            watch: Watch::new(),
            diag: Diag::new(),
            waiter,
            draw_failures: 0,
            draw_stats: DrawStats { since: clock.now(), ..DrawStats::default() },
            pause_wanted: None,
            next_frame: Instant::default(),
            loader,
            watchdog,
            commands: Commands::default(),
            goal_met: None,
            reports: Saver::new(Box::new(ReportsDir)),
            hardware,
            sensors,
            next_sensor_read: Instant::default(),
            power,
            next_power_log: Instant::default(),
        };
        let mut sweep = Sweep::new(caps, cores, goal);
        desk.hardware.set_machine(sweep.machine());
        sweep.measure_bandwidth(&clock, vector);
        Ok(Self { desk, sweep, chunk_bytes: START_CHUNK_BYTES, vector })
    }

    fn now(&self) -> Instant {
        self.desk.clock.now()
    }

    /// Run until `q` or `Esc` on the console, until the desktop asks the loader's app to stop (a
    /// `Stop` message or a `q` typed in `ferrite-dev push`), or until the status page asks to quit.
    /// Logs a sweep summary on the way out.
    pub fn run(&mut self) {
        loop {
            let now = self.now();
            self.sweep.tick(now);
            let tended = self.desk.duties(now, &self.sweep.snapshot());
            if let Some(goal) = tended.goal {
                self.sweep.set_goal(goal);
            }
            let deadline = tended.next;
            if self.apply_commands() == Flow::Stop {
                break;
            }
            let Some(deadline) = deadline else { continue };
            if self.sweep.split_ready(now) {
                let desk = &mut self.desk;
                // A split test cut short by a command runs again from its start, once the
                // commands are applied on the next round.
                self.sweep.run_split(self.vector, &mut |now, snapshot| desk.duties(now, snapshot));
                continue;
            }
            self.idle_until(deadline);
        }
        let sweep = self.sweep.status();
        log::info!(
            "sweep: stopping after {} passes over {} bytes ({} bytes tested), in {sweep}, {} errors, {}",
            sweep.passes,
            sweep.bytes,
            sweep.passes.saturating_mul(sweep.bytes as u64),
            sweep.faults,
            sweep.analysis
        );
        self.sweep.check_cores();
        self.sweep.save_timings(self.now());
        let why = self.desk.goal_met.map_or(Why::Quit, Why::GoalMet);
        self.desk.save_report(self.now(), &self.sweep.snapshot(), why);
        self.desk.announce_end(&self.sweep.snapshot());
        self.desk.net.shutdown();
        self.desk.end_power(self.sweep.mp());
    }

    /// Apply the queued commands in order; [`Flow::Stop`] when one of them was [`Command::Quit`].
    fn apply_commands(&mut self) -> Flow {
        let mut flow = Flow::Continue;
        for command in self.desk.commands.drain() {
            log::info!("app: {command:?}");
            match command {
                Command::Pause => self.sweep.pause(),
                Command::Resume => self.sweep.resume(),
                Command::Restart(preset) => self.sweep.restart(preset),
                Command::Cores(choice) => self.sweep.set_cores(choice),
                Command::Goal(goal) => self.sweep.set_goal(goal),
                // Taken out of the queue by the duties, which have the snapshot it is built from.
                Command::SaveReport => {},
                Command::Quit => flow = Flow::Stop,
            }
        }
        self.desk.pause_wanted = None;
        flow
    }

    /// Sweep until `deadline` (at least one chunk), or wait on the timer and key
    /// events when there is nothing to sweep. Without events the loop just spins.
    fn idle_until(&mut self, deadline: Instant) {
        let now = self.now();
        if !self.sweep.has_work(now) {
            let wake = self.sweep.waiting_until().map_or(deadline, |until| until.min(deadline));
            if wake - now < SHORT_WAIT && self.sweep.waiting_until().is_some() {
                return;
            }
            let timeout = wake - now;
            if let Some(waiter) = self.desk.waiter.as_mut()
                && let Err(e) = waiter.wait(timeout)
            {
                log::warn!("ui: waiting on events failed, polling instead: {e}");
                self.desk.waiter = None;
            }
            return;
        }
        loop {
            let start = self.now();
            self.sweep.step(self.chunk_bytes, start, self.vector);
            let end = self.now();
            let took = end - start;
            if took < SWEEP_SLICE {
                self.chunk_bytes = (self.chunk_bytes * 2).min(MAX_CHUNK_BYTES);
            } else if took > SWEEP_SLICE * 2 {
                self.chunk_bytes = (self.chunk_bytes / 2).max(MIN_CHUNK_BYTES);
            }
            if end >= deadline || !self.sweep.has_work(end) || self.sweep.split_ready(end) {
                return;
            }
        }
    }
}

impl Desk<'_> {
    /// Read the power meter; log its reading every [`POWER_LOG_INTERVAL`].
    fn sample_power(&mut self, now: Instant) {
        let Some(sample) = self.power.sample(now) else { return };
        if now < self.next_power_log {
            return;
        }
        self.next_power_log = now + POWER_LOG_INTERVAL;
        let mut line = String::new();
        for (domain, watts) in sample.power.iter() {
            let _ = write!(line, " {}={watts}", <&str>::from(domain));
        }
        if let Some(throttled) = sample.throttled {
            let _ = write!(line, " throttled={}ms", throttled.as_millis());
        }
        if let Some(reasons) = sample.reasons {
            let _ = write!(line, " limits=[{}]", reasons.active);
        }
        log::info!("power:{line}");
    }

    /// Put the firmware's clock request back and log what the run used.
    fn end_power(&mut self, mp: Option<&Mp>) {
        let meter = self.power.meter();
        if let Some(energy) = meter.energy(Domain::Package) {
            log::info!("power: the package used {energy} over the run");
        }
        let tally = meter.tally();
        if tally.samples > 0 {
            let mut line = String::new();
            for (limit, count) in tally.ranked() {
                let _ = write!(line, " {}={}%", <&str>::from(limit), u64::from(count) * 100 / u64::from(tally.samples));
            }
            log::info!(
                "power: clock limited by{} over {} samples",
                if line.is_empty() { " nothing" } else { &line },
                tally.samples
            );
        }
        self.power.restore(mp);
    }

    /// One round of the duties: re-arm the watchdog, poll the network, read keys and the loader's
    /// stop, draw when a frame is due, log. Returns when they are next due (`None` when a queued
    /// command has to cut the sweep short) and any goal set meanwhile, which takes effect at once
    /// instead of waiting for the split test in progress to end.
    fn duties(&mut self, now: Instant, snapshot: &Snapshot<'_>) -> Tended {
        if let Some(watchdog) = self.watchdog.as_mut() {
            watchdog.tick(now);
        }
        if now >= self.next_sensor_read {
            self.hardware.set_vitals(Vitals::of(self.sensors.sample(now)));
            self.next_sensor_read = now + SENSOR_INTERVAL;
            self.sample_power(now);
        }
        let net = &mut self.net;
        self.watch.observe(now, snapshot, &mut |notification| net.notify(notification));
        let commands = &mut self.commands;
        let net_wait = self.net.poll(now, snapshot, commands).unwrap_or(FRAME_INTERVAL);
        self.read_keys(&snapshot.status);
        if self.desktop_stop_requested() {
            self.commands.push(Command::Quit);
        }
        if self.commands.take_report() {
            self.save_report(now, snapshot, Why::Requested);
        }
        self.check_goal(now, snapshot);
        let sweep = &snapshot.status;
        let status = self.net.status();
        let net = (!status.is_empty()).then_some(status);
        if now >= self.next_frame {
            self.draw(now, snapshot, net.as_ref());
            self.next_frame = now + FRAME_INTERVAL;
        }
        self.diag.tick(now, net.as_ref(), sweep);
        let goal = self.commands.take_goal();
        let due = (!self.commands.interrupting()).then(|| (now + net_wait).min(self.next_frame));
        Tended { next: due, goal }
    }

    /// End the run the way `q` does once its goal is met.
    fn check_goal(&mut self, now: Instant, snapshot: &Snapshot<'_>) {
        let status = &snapshot.status;
        let outlook = &status.outlook;
        let elapsed = snapshot.history.run_started.map_or(Duration::ZERO, |start| now - start);
        if self.goal_met.is_some() || snapshot.history.run_started.is_none() {
            return;
        }
        if let Some(met) = outlook.goal.met(outlook.settled, elapsed, status.faults) {
            self.goal_met = Some(met);
            log::info!("run: goal met ({met}) after {}s, ending the run", elapsed.as_secs());
            self.commands.push(Command::Quit);
        }
    }

    /// Write the report of the run as it stands to its file on the boot volume, and say where on the
    /// log line. A volume that cannot be written is a warning and a note on screen, never the run's end.
    fn save_report(&mut self, now: Instant, snapshot: &Snapshot<'_>, why: Why) {
        let report = Report { snapshot, hardware: &self.hardware, wall: &self.wall, now, why, version: VERSION };
        let text = report.render();
        let run = snapshot.history.run_started;
        let stamp = self
            .wall
            .is_synced()
            .then(|| self.wall.local_time(run.unwrap_or(now)))
            .flatten()
            .map(crate::runreport::store::file_stamp);
        let notice = match self.reports.save(run, stamp.as_deref(), &text) {
            Ok(saved) => {
                log::info!("report: wrote {} ({} bytes)", saved.path, saved.bytes);
                Notice {
                    text: format::fixed(format_args!("report saved: {} ({} bytes)", saved.path, saved.bytes)),
                    failed: false,
                }
            },
            Err(e) => {
                log::warn!("report: not saved: {e}");
                Notice { text: format::fixed(format_args!("report not saved: {e}")), failed: true }
            },
        };
        if let Screen::Pixels { dash, .. } = &mut self.screen {
            dash.notify(notice, now);
        }
    }

    /// Tell the user the run ended, and give the network a few seconds to deliver that and whatever
    /// is still queued before it shuts down.
    fn announce_end(&mut self, snapshot: &Snapshot<'_>) {
        self.net.notify(notify::run_ended(self.clock.now(), snapshot));
        let deadline = self.clock.now() + NOTIFY_FLUSH;
        while self.net.notifications_pending() > 0 {
            let now = self.clock.now();
            if now >= deadline {
                log::warn!("notify: {} notifications not delivered before shutdown", self.net.notifications_pending());
                return;
            }
            if let Some(watchdog) = self.watchdog.as_mut() {
                watchdog.tick(now);
            }
            let Some(wait) = self.net.poll(now, snapshot, &mut self.commands) else {
                log::warn!("notify: no network to send {} notifications on", self.net.notifications_pending());
                return;
            };
            self.commands.drain().for_each(drop);
            if let Some(waiter) = self.waiter.as_mut()
                && let Err(e) = waiter.wait(wait.min(NOTIFY_STEP))
            {
                log::debug!("notify: waiting on events failed: {e}");
                self.waiter = None;
            }
        }
    }

    /// Queue what the console's keys ask for: `q` or `Esc` quits, `p` pauses or resumes, `r`
    /// restarts with the current preset, `F10` saves the report.
    fn read_keys(&mut self, status: &SweepStatus) {
        while let Ok(Some(key)) = self.input.read_key() {
            let command = match key {
                Key::Special(ScanCode::ESCAPE) => Command::Quit,
                Key::Special(ScanCode::FUNCTION_10) => Command::SaveReport,
                Key::Special(_) => continue,
                Key::Printable(c) => match char::from(c).to_ascii_lowercase() {
                    'q' => Command::Quit,
                    'p' => {
                        let paused = self.pause_wanted.unwrap_or(status.paused);
                        self.pause_wanted = Some(!paused);
                        if paused { Command::Resume } else { Command::Pause }
                    },
                    'r' => Command::Restart(status.preset),
                    _ => continue,
                },
            };
            self.commands.push(command);
        }
    }

    /// A `Stop` from the desktop, or a `q` typed there: the same quit key as the console's.
    fn desktop_stop_requested(&self) -> bool {
        let Some(link) = self.loader else {
            return false;
        };
        let mut asked = link.stop_requested();
        let mut typed = [0; 64];
        while let Ok(n) = link.read(&mut typed)
            && n > 0
        {
            asked |= typed[..n].iter().any(|b| matches!(b, b'q' | b'Q'));
        }
        asked
    }

    fn draw(&mut self, now: Instant, snapshot: &Snapshot<'_>, net: Option<&NetStatus>) {
        let net = net.copied();
        let net_err = net.is_none().then(|| self.net.reason());
        let outlook = &snapshot.status.outlook;
        let elapsed = snapshot.history.run_started.map_or(Duration::ZERO, |start| now - start);
        let time = self.wall.line(now, outlook.overall(elapsed).left, outlook.pass);
        let failure = match &mut self.screen {
            Screen::Pixels { dash, display } => {
                let start = self.clock.now();
                dash.render(&Frame { snapshot, net, net_err, now, time, hardware: &self.hardware });
                let rendered = self.clock.now();
                let (canvas, rects) = dash.output();
                if rects.is_empty() {
                    return;
                }
                let bytes = rects.iter().map(|r| r.area() * 4).sum();
                let pushed = display.present(canvas, rects);
                self.draw_stats.record(now, rendered - start, self.clock.now() - rendered, bytes);
                let Err(e) = pushed else { return };
                dash.invalidate();
                e.to_string()
            },
            Screen::Text { terminal, last } => {
                let elapsed = snapshot.history.run_started.map_or(0, |start| (now - start).as_secs());
                let view = View { sweep: snapshot.status, net, net_err, elapsed, time };
                if *last == Some(view) {
                    return;
                }
                let Err(e) = terminal.draw(|frame| ui::render(frame, &view)) else {
                    *last = Some(view);
                    return;
                };
                *last = None;
                e.to_string()
            },
        };
        if self.draw_failures == 0 {
            log::warn!("ui: drawing failed: {failure}");
        }
        self.draw_failures = self.draw_failures.saturating_add(1);
    }
}
