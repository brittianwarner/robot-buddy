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
    let mut axis = 0;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now < deadline {
            thread::park_timeout(deadline - now);
            continue;
        }
        if now >= next_health {
            drive.health(axis)?;
            axis = (axis + 1) % 3;
            next_health = now + Duration::from_millis(200);
        }
        let state = input.0.lock().ok().context("neck input poisoned")?.clone();
        drive.bus.goals(&drive.joints, planner.step(now, &state))?;
        deadline += PERIOD;
        if deadline <= Instant::now() {
            deadline = Instant::now() + PERIOD;
        }
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
