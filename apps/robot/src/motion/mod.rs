//! Calibrated three-axis Pip neck. The serial bus never leaves its worker;
//! input producers replace bounded state and never wait on hardware.
mod calibration;
mod drive;
mod poses;
mod sts3215;

use anyhow::{Context, Result, bail, ensure};
use drive::Drive;
use poses::{Gesture, Inputs, Planner};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use sts3215::{Bus, Serial};

const PERIOD: Duration = Duration::from_millis(20);
const TIMING_REPORT_INTERVAL: Duration = Duration::from_secs(5);
const TIMING_SAMPLE_CAP: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimingReport {
    ticks: u64,
    missed: u64,
    total_ticks: u64,
    total_missed: u64,
    p95_cycle_us: u32,
    max_cycle_us: u32,
    max_work_us: u32,
    max_overrun_us: u32,
}

fn duration_us(duration: Duration) -> u32 {
    u32::try_from(duration.as_micros()).unwrap_or(u32::MAX)
}

/// Fixed-capacity loop timing accumulator for the 50 Hz neck worker.
struct LoopTiming {
    window_start: Instant,
    window_ticks: u64,
    window_missed: u64,
    max_cycle_us: u32,
    max_work_us: u32,
    max_overrun_us: u32,
    samples_us: Vec<u32>,
    total_ticks: u64,
    total_missed: u64,
}

impl LoopTiming {
    fn new(now: Instant) -> Self {
        Self {
            window_start: now,
            window_ticks: 0,
            window_missed: 0,
            max_cycle_us: 0,
            max_work_us: 0,
            max_overrun_us: 0,
            samples_us: Vec::with_capacity(TIMING_SAMPLE_CAP),
            total_ticks: 0,
            total_missed: 0,
        }
    }

    fn record(
        &mut self,
        scheduled: Instant,
        started: Instant,
        finished: Instant,
    ) -> Option<TimingReport> {
        let cycle_us = duration_us(finished.saturating_duration_since(scheduled));
        let work_us = duration_us(finished.saturating_duration_since(started));
        let next_deadline = scheduled + PERIOD;
        let missed = finished >= next_deadline;
        let overrun_us = if missed {
            duration_us(finished.saturating_duration_since(next_deadline))
        } else {
            0
        };
        self.window_ticks = self.window_ticks.saturating_add(1);
        self.total_ticks = self.total_ticks.saturating_add(1);
        if missed {
            self.window_missed = self.window_missed.saturating_add(1);
            self.total_missed = self.total_missed.saturating_add(1);
        }
        self.max_cycle_us = self.max_cycle_us.max(cycle_us);
        self.max_work_us = self.max_work_us.max(work_us);
        self.max_overrun_us = self.max_overrun_us.max(overrun_us);
        if self.samples_us.len() < TIMING_SAMPLE_CAP {
            self.samples_us.push(cycle_us);
        }
        if finished.saturating_duration_since(self.window_start) >= TIMING_REPORT_INTERVAL {
            self.flush(finished)
        } else {
            None
        }
    }

    fn flush(&mut self, now: Instant) -> Option<TimingReport> {
        if self.window_ticks == 0 {
            return None;
        }
        let p95_cycle_us = if self.samples_us.is_empty() {
            0
        } else {
            self.samples_us.sort_unstable();
            let index = (self.samples_us.len() * 95).div_ceil(100).saturating_sub(1);
            self.samples_us[index]
        };
        let report = TimingReport {
            ticks: self.window_ticks,
            missed: self.window_missed,
            total_ticks: self.total_ticks,
            total_missed: self.total_missed,
            p95_cycle_us,
            max_cycle_us: self.max_cycle_us,
            max_work_us: self.max_work_us,
            max_overrun_us: self.max_overrun_us,
        };
        self.window_start = now;
        self.window_ticks = 0;
        self.window_missed = 0;
        self.max_cycle_us = 0;
        self.max_work_us = 0;
        self.max_overrun_us = 0;
        self.samples_us.clear();
        Some(report)
    }
}

fn emit_timing(report: TimingReport) {
    let period_ms = u64::try_from(PERIOD.as_millis()).unwrap_or(20);
    if report.missed > 0 {
        tracing::warn!(
            period_ms,
            ticks = report.ticks,
            missed = report.missed,
            total_ticks = report.total_ticks,
            total_missed = report.total_missed,
            p95_cycle_us = report.p95_cycle_us,
            max_cycle_us = report.max_cycle_us,
            max_work_us = report.max_work_us,
            max_overrun_us = report.max_overrun_us,
            "neck loop missed 20ms deadline"
        );
    } else {
        tracing::info!(
            period_ms,
            ticks = report.ticks,
            missed = 0u64,
            total_ticks = report.total_ticks,
            total_missed = report.total_missed,
            p95_cycle_us = report.p95_cycle_us,
            max_cycle_us = report.max_cycle_us,
            max_work_us = report.max_work_us,
            max_overrun_us = 0u32,
            "neck loop timing"
        );
    }
}

#[derive(Clone, Default)]
pub struct MotionInput(Arc<Mutex<Inputs>>);
impl MotionInput {
    pub fn track(&self, x: f32, y: f32) {
        if x.is_finite()
            && y.is_finite()
            && let Ok(mut s) = self.0.lock()
            && !s.sleeping
        {
            s.face = Some((Instant::now(), x.clamp(-1., 1.), y.clamp(-1., 1.)));
        }
    }
    pub fn express(&self, emotion: &str, intensity: f32) {
        if intensity.is_finite()
            && let Ok(mut s) = self.0.lock()
        {
            (s.pose, s.sleeping) = poses::expression(emotion, intensity.clamp(0., 1.));
            if s.sleeping {
                s.face = None;
                s.gesture = None;
            }
        }
    }
    pub fn gesture(&self, name: &str, intensity: f32) {
        if intensity.is_finite()
            && let Ok(mut s) = self.0.lock()
        {
            if name == "none" {
                s.gesture = None;
            } else if let Some(g) = Gesture::parse(name) {
                s.gesture = Some((g, Instant::now(), intensity.clamp(0., 1.)));
            }
        }
    }
    /// Absolute yaw, pitch and roll in calibrated degrees, with a bounded hold.
    pub fn manual(&self, degrees: [f32; 3], hold: Duration) {
        if degrees.iter().all(|x| x.is_finite())
            && let Ok(mut s) = self.0.lock()
        {
            s.manual = Some((degrees, Instant::now() + hold.min(Duration::from_secs(30))));
        }
    }
    pub fn release(&self) {
        if let Ok(mut s) = self.0.lock() {
            s.manual = None;
        }
    }
}

pub struct Motion {
    input: MotionInput,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Motion {
    /// Startup is synchronous: return only after verifying and arming all axes.
    pub fn start(port: &Path, calibration: &Path) -> Result<Self> {
        let joints = calibration::load(calibration)?;
        let mut drive = Drive {
            bus: Serial::open(port)?,
            joints,
        };
        let start = drive.arm()?;
        let input = MotionInput::default();
        let worker_input = input.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("neck-50hz".into())
            .spawn(move || {
                let result = run(&mut drive, &worker_input, &worker_stop, start);
                drop(drive); // Torque off before logging, including on an I/O fault.
                if let Err(e) = result {
                    tracing::error!(error=%e,"neck motion stopped; restart requires verification");
                }
            })?;
        Ok(Self {
            input,
            stop,
            worker: Some(worker),
        })
    }
    pub fn input(&self) -> MotionInput {
        self.input.clone()
    }
}
impl Drop for Motion {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(w) = self.worker.take() {
            w.thread().unpark();
            if w.join().is_err() {
                tracing::error!("neck worker panicked");
            }
        }
    }
}

fn run(
    drive: &mut Drive<impl Bus>,
    input: &MotionInput,
    stop: &AtomicBool,
    start: [u16; 3],
) -> Result<()> {
    let mut deadline = Instant::now();
    let mut planner = Planner::new(drive.joints, start, deadline);
    let mut next_health = deadline + Duration::from_millis(200);
    let mut timing = LoopTiming::new(deadline);
    let mut axis = 0;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now < deadline {
            thread::park_timeout(deadline - now);
            continue;
        }
        let scheduled = deadline;
        if now >= next_health {
            drive.health(axis)?;
            axis = (axis + 1) % 3;
            next_health = now + Duration::from_millis(200);
        }
        let state = input.0.lock().ok().context("neck input poisoned")?.clone();
        drive.bus.goals(&drive.joints, planner.step(now, &state))?;
        let finished = Instant::now();
        if let Some(report) = timing.record(scheduled, now, finished) {
            emit_timing(report);
        }
        deadline += PERIOD;
        if deadline <= finished {
            deadline = finished + PERIOD;
        }
    }
    if let Some(report) = timing.flush(Instant::now()) {
        emit_timing(report);
    }
    Ok(())
}

/// `servo <ping|read|verify|center|provision> PORT CALIBRATION.json [--write-eeprom]`.
/// Maintenance takes the same exclusive port lock as the daemon.
pub fn servo_cli(args: &[String]) -> Result<()> {
    let [command, port, config, extra @ ..] = args else {
        bail!("servo <ping|read|verify|center|provision> PORT CALIBRATION.json [--write-eeprom]")
    };
    ensure!(
        matches!(
            command.as_str(),
            "ping" | "read" | "verify" | "center" | "provision"
        ),
        "unknown servo command"
    );
    if command == "provision" {
        ensure!(
            extra == ["--write-eeprom"],
            "provision requires --write-eeprom"
        );
    } else {
        ensure!(extra.is_empty(), "unexpected servo arguments");
    }
    let joints = calibration::load(Path::new(config))?;
    let mut bus = Serial::open(Path::new(port))?;
    if command == "ping" || command == "read" {
        for j in joints {
            if command == "ping" {
                bus.ping(j.id)?;
                println!("id {}: ok", j.id);
            } else {
                let mut bytes = [0; 2];
                bus.read(j.id, sts3215::PRESENT, &mut bytes)?;
                println!("id {}: {} steps", j.id, u16::from_le_bytes(bytes));
            }
        }
        return Ok(());
    }
    // The CLI runs before the normal daemon runtime. Intercept termination
    // before touching torque or unlocking EEPROM; always join the cleanup.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(supervise_maintenance(
            Drive { bus, joints },
            command.clone(),
        ))
}

async fn supervise_maintenance(drive: Drive<Serial>, command: String) -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let mut task = tokio::task::spawn_blocking(move || maintenance(drive, &command, &flag));
    tokio::select! {
        result=&mut task=>return result?,
        _=term.recv()=>(),
        _=interrupt.recv()=>(),
    }
    stop.store(true, Ordering::Release);
    task.await??;
    bail!("servo maintenance interrupted")
}

fn maintenance(mut drive: Drive<impl Bus>, command: &str, stop: &AtomicBool) -> Result<()> {
    let joints = drive.joints;
    ensure!(
        !stop.load(Ordering::Acquire),
        "servo maintenance interrupted"
    );
    match command {
        "verify" => {
            drive.disable()?;
            for i in 0..3 {
                drive.verify_joint(i)?;
            }
        }
        "provision" => drive.provision(stop)?,
        "center" => {
            let initial = drive.arm()?;
            ensure!(!stop.load(Ordering::Acquire), "servo center interrupted");
            drive.bus.goals(&joints, joints.map(|j| j.center))?;
            let started = Instant::now();
            let mut last_progress = [started; 3];
            let mut closest =
                std::array::from_fn::<_, 3, _>(|i| initial[i].abs_diff(joints[i].center));
            let travel = joints
                .iter()
                .zip(initial)
                .map(|(j, p)| j.degrees(p).abs() / j.max_speed_deg_s)
                .fold(0.0, f32::max);
            let until = started + Duration::from_secs_f32(travel + 3.);
            loop {
                ensure!(!stop.load(Ordering::Acquire), "servo center interrupted");
                let mut centered = true;
                for (i, j) in joints.iter().enumerate() {
                    let distance = drive.verify_joint(i)?.abs_diff(j.center);
                    if distance < closest[i] {
                        closest[i] = distance;
                        last_progress[i] = Instant::now();
                    }
                    if distance > 3 {
                        centered = false;
                        ensure!(
                            last_progress[i].elapsed() < Duration::from_secs(1),
                            "servo {} stalled while centering",
                            j.id
                        );
                    }
                }
                if centered {
                    break;
                }
                ensure!(Instant::now() < until, "neck center deadline elapsed");
                thread::sleep(PERIOD);
            }
        }
        _ => unreachable!(),
    }
    drive.disable()?;
    println!("{command}: verified, torque off");
    Ok(())
}

#[cfg(test)]
mod tests;
